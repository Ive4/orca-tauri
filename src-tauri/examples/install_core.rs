//! Offline installation for a locally produced archive. Quit the desktop shell
//! first: the launcher refuses to attach to another process using its port.
use anyhow::{ensure, Context, Result};
use orca_tauri::{
    config::{AppPaths, Config},
    process::CoreManager,
    updater::CoreUpdater,
};

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    ensure!(
        args.len() == 3,
        "usage: install_core ARCHIVE.zip vUPSTREAM_VERSION SHA256"
    );
    let root = AppPaths::discover()?;
    let config = Config::load_or_create(&root)?;
    let paths = AppPaths::initialize(root, &config)?;
    let updater = CoreUpdater::new(paths.core.clone(), None, env!("ORCA_TAURI_TARGET").into())?;
    let session = updater.begin_update().await;
    let candidate = session
        .prepare_local_archive(std::path::Path::new(&args[0]), &args[1], &args[2])
        .await?;
    let core = CoreManager::new()?;
    config
        .start_core(&core, &paths, &candidate)
        .await
        .context("candidate failed startup validation")?;
    session.commit(&candidate)?;
    core.stop().await?;
    println!(
        "Installed {}. You can now start Orca Tauri.",
        candidate.manifest.display_version()
    );
    Ok(())
}
