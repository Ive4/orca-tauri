#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    if let Err(error) = orca_tauri::run() {
        eprintln!("Orca Tauri could not start: {error:#}");
        #[cfg(windows)]
        {
            use windows::{
                core::PCWSTR,
                Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK},
            };
            let body: Vec<u16> = format!(
                "Orca Tauri could not start:\n\n{error:#}\n\nCheck ~/.orca-tauri/config.json."
            )
            .encode_utf16()
            .chain(Some(0))
            .collect();
            let title: Vec<u16> = "Orca Tauri".encode_utf16().chain(Some(0)).collect();
            unsafe {
                let _ = MessageBoxW(
                    None,
                    PCWSTR(body.as_ptr()),
                    PCWSTR(title.as_ptr()),
                    MB_OK | MB_ICONERROR,
                );
            }
        }
        std::process::exit(1);
    }
}
