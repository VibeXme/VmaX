#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

pub mod adaptive_bitrate;
pub mod audio;
pub mod bitrate;
pub mod capture;
pub mod codec_tiers;
pub mod color;
pub mod commands;
pub mod congestion;
pub mod connections;
pub mod connectivity;
pub mod cursor;
pub mod encoder;
pub mod encoder_quality;
pub mod encoder_recovery;
pub mod graphics;
pub mod handshake;
pub mod haptics;
pub mod identity;
pub mod input;
pub mod internet;
pub mod invite;
pub mod jitter_buffer;
pub mod nat_mobility;

#[cfg(target_os = "windows")]
pub mod native_client;

#[cfg(target_os = "windows")]
pub mod native_d3d11;

pub mod network;
pub mod nvenc;
pub mod opus_audio;
pub mod pacer;
pub mod pacing;
pub mod presence;
pub mod protocol;
pub mod quality;
pub mod recovery_jitter;
mod relay_probe;
mod relay_upgrade;
pub mod signaling;
pub mod stun;
pub mod telemetry;
pub mod transport;
mod vmax_audio_aaudio_opus;
mod vmax_nvenc_ull;
mod vmax_relay_ice;
pub mod wan_transport;

#[cfg(target_os = "android")]
pub mod android_jni;

use commands::AppState;
use getrandom;
use tauri::{Emitter, Manager, WindowEvent};
use tracing_subscriber::EnvFilter;

/// Opts the process out of Windows' background power throttling.
///
/// Windows 11 places a process that is not the foreground window under EcoQoS,
/// which starves the capture/encode threads on the host and the WebView's
/// decode/present tasks on the client. The picture stays smooth but arrives
/// hundreds of milliseconds late, which is exactly the "worse when the app is
/// out of focus" symptom. This is a per-process setting, so it covers both
/// roles for the lifetime of the app.
#[cfg(target_os = "windows")]
fn disable_power_throttling() {
    // PROCESS_POWER_THROTTLING_STATE. Declared locally with raw FFI so this
    // stays independent of the windows-crate feature set, matching the crash
    // handler above.
    #[repr(C)]
    struct ProcessPowerThrottlingState {
        version: u32,
        control_mask: u32,
        state_mask: u32,
    }
    const CURRENT_VERSION: u32 = 1;
    const EXECUTION_SPEED: u32 = 0x1;
    // ProcessPowerThrottling in PROCESS_INFORMATION_CLASS.
    const PROCESS_POWER_THROTTLING: i32 = 4;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut std::ffi::c_void;
        fn SetProcessInformation(
            process: *mut std::ffi::c_void,
            information_class: i32,
            information: *mut std::ffi::c_void,
            information_length: u32,
        ) -> i32;
    }

    let mut state = ProcessPowerThrottlingState {
        version: CURRENT_VERSION,
        control_mask: EXECUTION_SPEED,
        // A clear StateMask bit means "do not throttle execution speed".
        state_mask: 0,
    };
    // SAFETY: `state` is a correctly sized PROCESS_POWER_THROTTLING_STATE and
    // outlives the call. Failure is non-fatal: the app still runs throttled.
    let applied = unsafe {
        SetProcessInformation(
            GetCurrentProcess(),
            PROCESS_POWER_THROTTLING,
            (&raw mut state).cast(),
            std::mem::size_of::<ProcessPowerThrottlingState>() as u32,
        ) == 1
    };
    if !applied {
        tracing::debug!("process power throttling opt-out was not applied");
    }
}

#[cfg(target_os = "windows")]
fn install_crash_handler() {
    use std::fs::{File, create_dir_all};
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::Once;
    use std::ffi::c_void;

    // Win32 API constants
    const EXCEPTION_EXECUTE_HANDLER: i32 = 1;
    const EXCEPTION_ACCESS_VIOLATION: u32 = 0xC0000005;
    const EXCEPTION_STACK_OVERFLOW: u32 = 0xC00000FD;
    const EXCEPTION_INT_DIVIDE_BY_ZERO: u32 = 0xC0000094;
    const EXCEPTION_BREAKPOINT: u32 = 0x80000003;
    const EXCEPTION_ILLEGAL_INSTRUCTION: u32 = 0xC000001D;

    // Use raw Win32 API via std::ffi to avoid windows crate type complexity
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetUnhandledExceptionFilter(lpTopLevelExceptionFilter: *mut c_void) -> *mut c_void;
        fn GetCurrentProcessId() -> u32;
    }

    #[repr(C)]
    struct ExceptionRecord {
        exception_code: u32,
        exception_flags: u32,
        exception_record: *mut ExceptionRecord,
        exception_address: *mut c_void,
        number_parameters: u32,
        exception_information: [usize; 15],
    }

    #[repr(C)]
    struct ExceptionPointers {
        exception_record: *mut ExceptionRecord,
        context_record: *mut c_void,
    }

    static INIT: Once = Once::new();

    // Crash log directory: %TEMP%\VmaX (always writable)
    fn crash_log_dir() -> PathBuf {
        std::env::temp_dir().join("VmaX")
    }

    fn crash_log_path() -> PathBuf {
        crash_log_dir().join("crash.log")
    }

    fn startup_log_path() -> PathBuf {
        crash_log_dir().join("startup.log")
    }

    fn ensure_log_dir() {
        let _ = create_dir_all(crash_log_dir());
    }

    fn write_log(path: &PathBuf, content: &str) {
        ensure_log_dir();
        if let Ok(mut file) = File::options().create(true).append(true).open(path) {
            let _ = writeln!(file, "{}", content);
        }
    }

    INIT.call_once(|| {
        ensure_log_dir();
        // Startup verification log
        write_log(
            &startup_log_path(),
            &format!(
                "[{}] VmaX crash handler installed (PID: {})",
                chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
                unsafe { GetCurrentProcessId() }
            ),
        );

        // 1. Rust panic hook
        std::panic::set_hook(Box::new(|info| {
            write_crash_log("Panic", &format!("{:?}", info), info.location().map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column())));
        }));

        // 2. Windows unhandled exception filter (catches access violations, stack overflow, etc.)
        unsafe {
            SetUnhandledExceptionFilter(exception_filter as *mut c_void);
        }
    });

    unsafe extern "system" fn exception_filter(pointers: *mut ExceptionPointers) -> i32 {
        if !pointers.is_null() {
            let record = unsafe { &*(*pointers).exception_record };
            let code = record.exception_code;
            let addr = record.exception_address;
            let addr_str = if addr.is_null() {
                "unknown".to_string()
            } else {
                format!("{:p}", addr)
            };
            let desc = match code {
                EXCEPTION_ACCESS_VIOLATION => "Access Violation",
                EXCEPTION_STACK_OVERFLOW => "Stack Overflow",
                EXCEPTION_INT_DIVIDE_BY_ZERO => "Integer Divide by Zero",
                EXCEPTION_BREAKPOINT => "Breakpoint",
                EXCEPTION_ILLEGAL_INSTRUCTION => "Illegal Instruction",
                _ => "Unknown Exception",
            };
            write_crash_log(
                "Native Exception",
                &format!("{} (0x{:08X}) at {}", desc, code, addr_str),
                None,
            );
        }
        EXCEPTION_EXECUTE_HANDLER
    }

    fn write_crash_log(kind: &str, msg: &str, location: Option<String>) {
        use std::fs::File;
        use std::io::Write;
        let path = crash_log_path();
        ensure_log_dir();
        if let Ok(mut file) = File::create(&path) {
            let _ = writeln!(file, "=== VmaX Crash Report ===");
            let _ = writeln!(file, "Time: {}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f"));
            let _ = writeln!(file, "Type: {}", kind);
            let _ = writeln!(file, "Message: {}", msg);
            if let Some(loc) = location {
                let _ = writeln!(file, "Location: {}", loc);
            }
            let _ = writeln!(file, "\nBacktrace:\n{}", std::backtrace::Backtrace::force_capture());
        }
        // Also append to a rotating log
        let log_entry = format!(
            "[{}] {} - {}",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
            kind,
            msg
        );
        write_log(&startup_log_path(), &log_entry);
        tracing::error!(type = %kind, message = %msg, "Application crashed");
    }
}

#[cfg(not(target_os = "windows"))]
fn install_crash_handler() {
    std::panic::set_hook(Box::new(|info| {
        use std::fs::File;
        use std::io::Write;
        use std::path::PathBuf;
        let mut path = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("."));
        path.pop();
        path.push("crash.log");
        if let Ok(mut file) = File::create(&path) {
            let _ = writeln!(file, "=== VmaX Crash Report ===");
            let _ = writeln!(file, "Time: {}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f"));
            let _ = writeln!(file, "Panic: {:?}", info);
            if let Some(loc) = info.location() {
                let _ = writeln!(file, "Location: {}:{}:{}", loc.file(), loc.line(), loc.column());
            }
            let _ = writeln!(file, "\nBacktrace:\n{}", std::backtrace::Backtrace::force_capture());
        }
        tracing::error!(panic = %info, "Application panicked");
    }));
}

#[cfg(desktop)]
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
};

#[cfg(target_os = "android")]
struct RemoteKeyboard(tauri::plugin::PluginHandle<tauri::Wry>);

#[tauri::command]
async fn set_remote_keyboard(app: tauri::AppHandle, visible: bool) -> Result<(), String> {
    #[cfg(target_os = "android")]
    {
        app.state::<RemoteKeyboard>()
            .0
            .run_mobile_plugin_async(if visible { "show" } else { "hide" }, ())
            .await
            .map_err(|err| err.to_string())
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (app, visible);
        Ok(())
    }
}

#[tauri::command]
async fn set_stream_immersive(app: tauri::AppHandle, active: bool) -> Result<(), String> {
    #[cfg(target_os = "android")]
    {
        app.state::<RemoteKeyboard>()
            .0
            .run_mobile_plugin_async(if active { "enterStream" } else { "leaveStream" }, ())
            .await
            .map_err(|err| err.to_string())
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (app, active);
        Ok(())
    }
}

#[tauri::command]
fn autostart_enabled(app: tauri::AppHandle) -> Result<bool, String> {
    #[cfg(desktop)]
    {
        use tauri_plugin_autostart::ManagerExt;
        app.autolaunch().is_enabled().map_err(|e| e.to_string())
    }
    #[cfg(not(desktop))]
    {
        let _ = app;
        Ok(false)
    }
}

#[tauri::command]
fn set_autostart(app: tauri::AppHandle, enabled: bool) -> Result<(), String> {
    #[cfg(desktop)]
    {
        use tauri_plugin_autostart::ManagerExt;
        let manager = app.autolaunch();
        if enabled {
            manager.enable()
        } else {
            manager.disable()
        }
        .map_err(|e| e.to_string())
    }
    #[cfg(not(desktop))]
    {
        let _ = (app, enabled);
        Err("Autostart is available on desktop".into())
    }
}

fn setup_app(app: &mut tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    use tauri_plugin_deep_link::DeepLinkExt;
    let handle = app.handle().clone();
    if let Err(error) = presence::device(&handle) {
        tracing::warn!(%error, "device identity initialization failed");
    }
    #[cfg(any(windows, target_os = "linux"))]
    if let Err(error) = app.deep_link().register_all() {
        tracing::warn!(%error, "could not register invite links");
    }
    let links = handle.clone();
    app.deep_link().on_open_url(move |event| {
        for url in event.urls() {
            presence::queue_link(&links, url.as_str());
        }
    });
    if let Ok(Some(urls)) = app.deep_link().get_current() {
        for url in urls {
            presence::queue_link(&handle, url.as_str());
        }
    }
    presence::start(handle);
    #[cfg(desktop)]
    {
        fn show(app: &tauri::AppHandle) {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.unminimize();
                let _ = window.set_focus();
            }
        }
        let open = MenuItem::with_id(app, "show", "Open VmaX", true, None::<&str>)?;
        let copy = MenuItem::with_id(
            app,
            "copy_invite",
            "Copy Quick Invite Link",
            true,
            None::<&str>,
        )?;
        let toggle = MenuItem::with_id(app, "toggle_host", "Toggle Hosting", true, None::<&str>)?;
        let quit = MenuItem::with_id(app, "quit", "Quit VmaX", true, None::<&str>)?;
        let menu = Menu::with_items(app, &[&open, &copy, &toggle, &quit])?;
        let mut tray = TrayIconBuilder::new()
            .tooltip("VmaX")
            .menu(&menu)
            .show_menu_on_left_click(false)
            .on_menu_event(|app, event| match event.id.as_ref() {
                "show" => show(app),
                "quit" => app.exit(0),
                "toggle_host" => {
                    let app = app.clone();
                    tauri::async_runtime::spawn(async move {
                        if let Err(error) = presence::tray_toggle(app.clone()).await {
                            show(&app);
                            let _ = app.emit("vmax://cloud-error", error);
                        }
                        let _ = app.emit("vmax://cloud-updated", ());
                    });
                }
                "copy_invite" => {
                    use tauri_plugin_clipboard_manager::ClipboardExt;
                    let app = app.clone();
                    tauri::async_runtime::spawn(async move {
                        let result =
                            presence::create_invite_token(app.clone())
                                .await
                                .and_then(|link| {
                                    app.clipboard().write_text(link).map_err(|e| e.to_string())
                                });
                        if let Err(error) = result {
                            show(&app);
                            let _ = app.emit("vmax://cloud-error", error);
                        }
                    });
                }
                _ => {}
            })
            .on_tray_icon_event(|tray, event| {
                if let TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } = event
                {
                    show(tray.app_handle());
                }
            });
        if let Some(icon) = app.default_window_icon().cloned() {
            tray = tray.icon(icon);
        }
        tray.build(app)?;
        if std::env::args().any(|arg| arg == "--minimized")
            && let Some(window) = app.get_webview_window("main")
        {
            let _ = window.hide();
        }
    }
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Prime the Windows OS RNG before rustls/aws-lc-rs initializes its entropy
    // source, preventing a JitterEntropy panic on systems where the CNG RNG
    // has not yet been primed.
    let _ = getrandom::getrandom(&mut [0u8; 32]);

    // Both the `ring` and `aws-lc-rs` rustls features are enabled transitively,
    // so rustls cannot choose a provider itself. Install `ring` once before any
    // TLS/reqwest client is built (including `PresenceState::default()` below).
    signaling::ensure_rustls_provider();

    // Install crash logging for both Rust panics AND native Windows exceptions (access violations, etc.)
    install_crash_handler();

    // Latency must not depend on window focus: keep capture/encode and the
    // WebView pipeline out of Windows' background EcoQoS throttling.
    #[cfg(target_os = "windows")]
    disable_power_throttling();

    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .try_init();

    let builder = tauri::Builder::default();
    #[cfg(desktop)]
    let builder = builder.plugin(tauri_plugin_single_instance::init(|app, _, _| {
        if let Some(window) = app.get_webview_window("main") {
            let _ = window.show();
            let _ = window.unminimize();
            let _ = window.set_focus();
        }
    }));
    #[cfg(target_os = "android")]
    let builder = builder.plugin(
        tauri::plugin::Builder::<tauri::Wry>::new("remote-keyboard")
            .setup(|app, api| {
                app.manage(RemoteKeyboard(
                    api.register_android_plugin("com.vmax.app", "RemoteKeyboardPlugin")?,
                ));
                Ok(())
            })
            .build(),
    );

    #[cfg(desktop)]
    let builder = builder
        .plugin(
            tauri_plugin_autostart::Builder::new()
                .args(["--minimized"])
                .build(),
        )
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_clipboard_manager::init());
    let result = builder
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_deep_link::init())
        .manage(AppState::default())
        .manage(presence::PresenceState::default())
        .setup(setup_app)
        .on_window_event(|window, event| {
            #[cfg(desktop)]
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
            #[cfg(not(desktop))]
            let _ = (window, event);
        })
        .invoke_handler(tauri::generate_handler![
            presence::identity_summary,
            presence::take_deep_link,
            presence::cloud_project_url,
            presence::cloud_configure,
            presence::cloud_sign_in,
            presence::cloud_sign_out,
            presence::cloud_profile,
            presence::cloud_snapshot,
            presence::cloud_friend,
            presence::cloud_search,
            presence::cloud_host_available,
            presence::create_invite_token,
            presence::cloud_connect,
            presence::cloud_decide,
            autostart_enabled,
            set_autostart,
            set_remote_keyboard,
            set_stream_immersive,
            commands::start_host_session,
            commands::list_host_displays,
            commands::list_host_audio_outputs,
            commands::prepare_internet,
            commands::controller_driver_status,
            commands::install_controller_driver,
            commands::restart_as_administrator,
            commands::join_session,
            commands::stop_session,
            commands::get_network_snapshot,
            commands::get_saved_connections,
            commands::friend_inbox,
            commands::friend_access,
            commands::dismiss_friend_request,
            commands::update_saved_connections,
            commands::toggle_local_signaling,
            commands::scan_lan_hosts,
            commands::list_relay_rooms,
            commands::test_media_relay,
            commands::send_probe_packet,
            commands::subscribe_stream_frames,
            commands::send_input_event,
            commands::set_fullscreen_keyboard_capture,
            commands::send_remote_text,
            commands::set_stream_input_paused,
            commands::set_host_only_input,
            commands::set_guest_input_permissions,
            commands::set_touch_gamepad,
            commands::report_client_capabilities,
            commands::request_keyframe,
            commands::set_stream_bitrate_limit,
            commands::reconnect_stream_gamepad,
            commands::native_video_state,
            commands::report_client_decode_latency,
            commands::set_constant_fps_mode,
            commands::update_runtime_audio_denoise,
            commands::update_settings,
            commands::graphics_backends,
            commands::set_haptics_enabled,
            commands::get_haptics_settings,

            #[cfg(target_os = "windows")]
            commands::native_d3d11_backend_state,
            #[cfg(target_os = "windows")]
            commands::init_native_client_renderer,
            #[cfg(target_os = "windows")]
            commands::present_native_frame,

            commands::start_host_opus_audio,
            commands::start_client_opus_audio,
            commands::push_opus_audio_packet,
            commands::get_opus_plc_stats,
            commands::get_crash_log,
            commands::get_startup_log,
        ])
        .build(tauri_context::get());

    match result {
        Ok(app) => app.run(|handle, event| {
            if matches!(event, tauri::RunEvent::Exit) {
                handle.state::<AppState>().stop_internet_on_exit();
            }
        }),
        Err(err) => tracing::error!(error = %err, "error while running VmaX application"),
    }
}

/// Tauri context generation.
///
/// Every target, Android included, uses the macro. The hand-written
/// `Context::new` variant that `build.rs` used to emit for Android was written
/// against an older Tauri API (`PatternKind`, the `semver` crate, a different
/// `Context::new` signature) and could no longer compile, which is why the
/// mobile build broke as soon as that generated file was regenerated.
mod tauri_context {
    pub fn get() -> tauri::Context<tauri::Wry> {
        tauri::generate_context!()
    }
}
