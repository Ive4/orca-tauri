pub mod config;
pub mod process;
pub mod updater;

#[cfg(feature = "desktop")]
mod desktop;
#[cfg(all(target_os = "macos", any(feature = "desktop", test)))]
mod official_import;
#[cfg(any(feature = "desktop", test))]
mod task_status;
#[cfg(feature = "desktop")]
mod tray;
#[cfg(feature = "desktop")]
mod window;

#[cfg(feature = "desktop")]
pub use desktop::run;
