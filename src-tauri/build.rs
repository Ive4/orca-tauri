fn main() {
    println!("cargo:rerun-if-env-changed=ORCA_TAURI_REPOSITORY");
    println!(
        "cargo:rustc-env=ORCA_TAURI_TARGET={}",
        std::env::var("TARGET").expect("Cargo provides TARGET")
    );
    #[cfg(feature = "desktop")]
    tauri_build::build();
}
