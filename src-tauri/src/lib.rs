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
pub mod native_client;
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
use tauri::{Emitter, Manager, WindowEvent};
use tracing_subscriber::EnvFilter;

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
            presence::cloud_host_available,
            presence::create_invite_token,
            presence::cloud_connect,
            presence::cloud_decide,
            autostart_enabled,
            set_autostart,
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
            commands::native_d3d11_backend_state,
            commands::report_client_decode_latency,
            commands::set_constant_fps_mode,
            commands::update_runtime_audio_denoise,
            commands::update_settings,
            commands::graphics_backends,
            commands::set_haptics_enabled,
            commands::get_haptics_settings,
            commands::init_native_client_renderer,
            commands::present_native_frame,
            commands::start_host_opus_audio,
            commands::start_client_opus_audio,
            commands::push_opus_audio_packet,
            commands::get_opus_plc_stats,
        ])
        .build(tauri::generate_context!());

    match result {
        Ok(app) => app.run(|handle, event| {
            if matches!(event, tauri::RunEvent::Exit) {
                handle.state::<AppState>().stop_internet_on_exit();
            }
        }),
        Err(err) => tracing::error!(error = %err, "error while running VmaX application"),
    }
}
