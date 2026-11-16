// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    #[cfg(target_os = "windows")]
    if let Some(pid) = std::env::args()
        .nth(1)
        .and_then(|arg| arg.strip_prefix("--wait-for-vmax=")?.parse::<u32>().ok())
        && pid != std::process::id()
    {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Threading::{
            OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
        };
        if let Ok(process) = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) } {
            unsafe {
                let _ = WaitForSingleObject(process, 15_000);
                let _ = CloseHandle(process);
            }
        }
    }
    vmax_lib::run()
}
