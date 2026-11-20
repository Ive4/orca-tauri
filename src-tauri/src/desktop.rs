use crate::{
    config::{AppPaths, Config},
    process::{CoreManager, CorePhase},
    task_status::{self, TaskSummary, TaskTarget},
    tray,
    updater::{CoreRelease, CoreUpdater, InstalledCore},
    window,
};
use anyhow::{bail, ensure, Context, Result};
use semver::Version;
use std::{
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, RwLock,
    },
    time::Duration,
};
use tauri::{AppHandle, Manager, RunEvent};
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_updater::{Update, UpdaterExt};

pub struct AppState {
    config: RwLock<Config>,
    pub paths: AppPaths,
    pub core: CoreManager,
    pub updater: CoreUpdater,
    operation: tokio::sync::Mutex<()>,
    pub window_operation: tokio::sync::Mutex<()>,
    pub quitting: AtomicBool,
    busy: AtomicBool,
    pending_show: AtomicBool,
    message: Mutex<String>,
    latest_official_version: Mutex<Option<Version>>,
    tasks: Mutex<Option<TaskSnapshot>>,
    retry_tasks: tokio::sync::Notify,
    core_update: Mutex<Option<CoreRelease>>,
    app_update: Mutex<Option<Update>>,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct TaskSnapshot {
    pid: u32,
    pub summary: Option<Arc<TaskSummary>>,
}

impl AppState {
    pub fn settings(&self) -> Result<Config> {
        Ok(self
            .config
            .read()
            .map_err(|_| anyhow::anyhow!("settings lock poisoned"))?
            .clone())
    }

    pub fn message(&self) -> String {
        self.message
            .lock()
            .map(|message| message.clone())
            .unwrap_or_else(|_| "Internal state error".into())
    }

    fn set_message(&self, app: &AppHandle, value: impl Into<String>) {
        if let Ok(mut message) = self.message.lock() {
            *message = value.into();
        }
        tray::refresh(app, self);
    }

    pub(crate) fn tasks_for(&self, pid: u32) -> Option<TaskSnapshot> {
        self.tasks
            .lock()
            .ok()?
            .as_ref()
            .filter(|snapshot| snapshot.pid == pid)
            .cloned()
    }

    pub(crate) fn retry_tasks(&self) {
        if self.core.status().phase == CorePhase::Running {
            if let Ok(mut snapshot) = self.tasks.lock() {
                *snapshot = None;
            }
            self.retry_tasks.notify_one();
        }
    }

    pub(crate) fn available_updates(&self) -> (Option<String>, Option<String>) {
        (
            self.app_update
                .lock()
                .ok()
                .and_then(|update| update.as_ref().map(|update| update.version.clone())),
            self.core_update
                .lock()
                .ok()
                .and_then(|release| release.as_ref().map(CoreRelease::display_version)),
        )
    }

    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::SeqCst)
    }

    pub(crate) fn latest_official_version(&self) -> Option<Version> {
        self.latest_official_version.lock().ok()?.clone()
    }
}

#[derive(Clone)]
pub enum Action {
    Show,
    ShowPaired,
    ShowTask(TaskTarget),
    Toggle,
    Destroy,
    Restart,
    Stop,
    CheckOfficialVersion,
    CheckCoreUpdate,
    CheckAppUpdate,
    Update,
    UpdateApp,
    Rollback,
    Autostart,
}

pub fn request(app: &AppHandle, action: Action) {
    let Some(state) = app
        .try_state::<Arc<AppState>>()
        .map(|state| Arc::clone(&*state))
    else {
        return;
    };
    if state.quitting.load(Ordering::SeqCst) {
        return;
    }
    // Existing windows remain usable while an update is downloading. These
    // native-only actions do not wait for the core operation mutex.
    let existing = window::current(app).is_some();
    if matches!(action, Action::Destroy)
        || (existing && matches!(action, Action::Toggle | Action::Show | Action::ShowPaired))
        || (matches!(window::current(app), Some(window::Target::Local))
            && matches!(&action, Action::ShowTask(_)))
    {
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            let result = match action {
                Action::Show => window::show(&app, &state, window::Target::Local).await,
                Action::ShowPaired => window::show(&app, &state, window::Target::Paired).await,
                Action::ShowTask(ref target) => {
                    window::reveal_existing_task(&app, &state, target).await
                }
                _ => window::destroy(&app, &state).await,
            };
            if let Err(error) = result {
                report(&app, &state, error);
            }
            tray::refresh(&app, &state);
        });
        return;
    }
    if state.busy.swap(true, Ordering::SeqCst) {
        if matches!(action, Action::Show | Action::Toggle) {
            state.pending_show.store(true, Ordering::SeqCst);
        }
        return;
    }
    state.set_message(
        app,
        match action {
            Action::Show | Action::ShowPaired | Action::ShowTask(_) | Action::Toggle => {
                "正在打开窗口…"
            }
            Action::Destroy => "正在关闭窗口…",
            Action::Restart => "正在重启内核…",
            Action::Stop => "正在停止内核及其任务…",
            Action::CheckOfficialVersion => "正在查询 Orca 官方版本…",
            Action::CheckCoreUpdate => "正在检查内核更新…",
            Action::Update => "正在准备内核更新…",
            Action::CheckAppUpdate => "正在检查应用更新…",
            Action::UpdateApp => "正在准备应用更新…",
            Action::Rollback => "正在回退内核…",
            Action::Autostart => "正在修改开机启动设置…",
        },
    );
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let result = {
            let _guard = state.operation.lock().await;
            if state.quitting.load(Ordering::SeqCst) {
                Ok(())
            } else {
                execute(&app, &state, action).await
            }
        };
        if let Err(error) = result {
            report(&app, &state, error);
        }
        state.busy.store(false, Ordering::SeqCst);
        tray::refresh(&app, &state);
        if state.pending_show.swap(false, Ordering::SeqCst) {
            request(&app, Action::Show);
        }
    });
}

async fn execute(app: &AppHandle, state: &AppState, action: Action) -> Result<()> {
    match action {
        Action::Show => {
            ensure_started(app, state).await?;
            window::show(app, state, window::Target::Local).await?;
        }
        Action::ShowPaired => {
            ensure_started(app, state).await?;
            window::show(app, state, window::Target::Paired).await?;
        }
        Action::ShowTask(ref target) => {
            ensure_started(app, state).await?;
            window::show_task(app, state, target).await?;
        }
        Action::Toggle => {
            if window::current(app).is_some() {
                window::destroy(app, state).await?;
            } else {
                ensure_started(app, state).await?;
                window::show(app, state, window::Target::Local).await?;
            }
        }
        Action::Destroy => window::destroy(app, state).await?,
        Action::Restart => {
            let reopen = window::current(app);
            window::destroy(app, state).await?;
            state.set_message(app, "正在重启内核…");
            state.core.stop().await?;
            ensure_started(app, state).await?;
            if let Some(target) = reopen {
                window::show(app, state, target).await?;
            }
        }
        Action::Stop => {
            window::destroy(app, state).await?;
            state.core.stop().await?;
            state.set_message(app, "内核已停止");
        }
        Action::CheckOfficialVersion => {
            let latest = state.updater.latest_official_version().await?;
            *state
                .latest_official_version
                .lock()
                .map_err(|_| anyhow::anyhow!("official version lock poisoned"))? = Some(latest);
            state.set_message(app, "官方版本查询完成");
        }
        Action::CheckCoreUpdate => check_core_update(app, state).await?,
        Action::CheckAppUpdate => check_app_update(app, state).await?,
        Action::Update => update(app, state).await?,
        Action::UpdateApp => update_app(app, state).await?,
        Action::Rollback => {
            let candidate = state
                .updater
                .load_previous()?
                .context("没有可回退的内核版本")?;
            let session = state.updater.begin_update().await;
            switch_core(app, state, &candidate, || session.commit(&candidate)).await?;
        }
        Action::Autostart => {
            let mut config = state.settings()?;
            let previous = app.autolaunch().is_enabled()?;
            let desired = !previous;
            if desired {
                app.autolaunch().enable()?;
            } else {
                app.autolaunch().disable()?;
            }
            config.autostart = desired;
            if let Err(error) = config.save(&state.paths.config) {
                if previous {
                    let _ = app.autolaunch().enable();
                } else {
                    let _ = app.autolaunch().disable();
                }
                return Err(error).context("保存开机启动设置失败，已尝试恢复系统设置");
            }
            *state
                .config
                .write()
                .map_err(|_| anyhow::anyhow!("settings lock poisoned"))? = config;
            state.set_message(
                app,
                if desired {
                    "已开启开机启动"
                } else {
                    "已关闭开机启动"
                },
            );
        }
    }
    match action {
        Action::Show | Action::ShowPaired | Action::ShowTask(_) => {
            state.set_message(app, "窗口已打开")
        }
        Action::Destroy => state.set_message(app, "窗口已关闭"),
        Action::Toggle => state.set_message(
            app,
            if window::current(app).is_some() {
                "窗口已打开"
            } else {
                "窗口已关闭"
            },
        ),
        Action::Restart => state.set_message(app, "内核重启完成"),
        Action::Rollback => state.set_message(app, "内核回退完成"),
        _ => {}
    }
    Ok(())
}

async fn ensure_started(app: &AppHandle, state: &AppState) -> Result<()> {
    if state.core.status().phase == CorePhase::Running {
        return Ok(());
    }
    if let Some(installed) = state.updater.load_active()? {
        state.set_message(app, "正在启动内核…");
        start_core(state, &installed).await?;
    } else {
        let archive = bundled_core_archive(app).context(
            "尚未安装内核：设置 config.json 的 update_repository，然后使用托盘“设置 → 关于与更新 → 检查内核更新”",
        )?;
        install_bundled_core(app, state, &archive).await?;
    }
    state.set_message(app, "内核运行中");
    Ok(())
}

fn bundled_core_archive(app: &AppHandle) -> Option<std::path::PathBuf> {
    let archive = app.path().resource_dir().ok()?.join("bootstrap-core.zip");
    archive.is_file().then_some(archive)
}

#[derive(serde::Deserialize)]
struct BundledCoreIdentity {
    version: String,
    sha256: String,
    package_revision: Option<String>,
    target: Option<String>,
}

async fn bundled_core_identity(archive: &Path) -> Result<BundledCoreIdentity> {
    let identity = tokio::fs::read(archive.with_extension("json"))
        .await
        .context("read bundled core identity")?;
    serde_json::from_slice(&identity).context("parse bundled core identity")
}

async fn install_bundled_core(app: &AppHandle, state: &AppState, archive: &Path) -> Result<()> {
    state.set_message(app, "正在校验随附内核…");
    let seed = bundled_core_identity(archive).await?;
    let session = state.updater.begin_update().await;
    let core = session
        .prepare_local_archive(archive, &seed.version, &seed.sha256)
        .await?;
    ensure!(
        seed.package_revision
            .is_none_or(|revision| core.manifest.package_revision.as_ref() == Some(&revision))
            && seed
                .target
                .is_none_or(|target| core.manifest.target == target),
        "bundled core identity differs from package manifest"
    );
    start_core(state, &core).await?;
    let activation = activate_started_core(state, || session.commit(&core));
    if let Err(error) = activation {
        state
            .core
            .stop()
            .await
            .with_context(|| format!("clean up bundled core after failed activation: {error:#}"))?;
        return Err(error);
    }
    Ok(())
}

async fn start_core(state: &AppState, core: &InstalledCore) -> Result<()> {
    ensure!(
        !state.quitting.load(Ordering::SeqCst),
        "application is exiting"
    );
    let settings = state.settings()?;
    settings.start_core(&state.core, &state.paths, core).await?;
    ensure!(
        !state.quitting.load(Ordering::SeqCst),
        "application is exiting"
    );
    Ok(())
}

fn activate_started_core(state: &AppState, commit: impl FnOnce() -> Result<()>) -> Result<()> {
    ensure!(
        state.core.web_client_offer().is_some(),
        "candidate core did not provide a Web client pairing offer"
    );
    commit()
}

async fn check_core_update(app: &AppHandle, state: &AppState) -> Result<()> {
    *state.core_update.lock().unwrap() = None;
    let session = state.updater.begin_update().await;
    let release = session
        .latest_release()
        .await?
        .context("仓库中没有适用于当前平台的稳定内核包")?;
    if let Some(current) = state.updater.load_active()? {
        if !release.is_newer_than(&current.manifest)? {
            state.set_message(app, "当前内核已是最新版本");
            return Ok(());
        }
    }
    let version = release.display_version();
    *state.core_update.lock().unwrap() = Some(release);
    state.set_message(app, format!("发现内核更新 · {version}"));
    Ok(())
}

async fn update(app: &AppHandle, state: &AppState) -> Result<()> {
    let release = state
        .core_update
        .lock()
        .unwrap()
        .take()
        .context("请先检查内核更新")?;
    let session = state.updater.begin_update().await;
    if let Some(current) = state.updater.load_active()? {
        if !release.is_newer_than(&current.manifest)? {
            state.set_message(app, "当前内核已是最新版本");
            return Ok(());
        }
    }
    // The old process and its window continue to work during download/unpack.
    state.set_message(app, format!("正在下载内核 {}…", release.display_version()));
    let candidate = session.prepare_release(&release).await?;
    ensure!(
        !state.quitting.load(Ordering::SeqCst),
        "application is exiting"
    );
    switch_core(app, state, &candidate, || session.commit(&candidate)).await?;
    let version = candidate.manifest.display_version();
    state.set_message(app, format!("内核更新完成 · {version}"));
    notify(app, state, &format!("内核和 UI 已更新至 {version}"));
    Ok(())
}

async fn check_app_update(app: &AppHandle, state: &AppState) -> Result<()> {
    *state.app_update.lock().unwrap() = None;
    let mut updater = app.updater_builder().timeout(Duration::from_secs(180));
    #[cfg(windows)]
    {
        // We shut down the window and core below. Keep the tray if launching
        // the installer fails, since Tauri's default hook clears it first.
        updater = updater.on_before_exit(|| {});
    }
    if let Some(proxy) = state.settings()?.download_proxy {
        updater = updater.proxy(proxy.parse()?);
    }
    let Some(update) = updater.build()?.check().await? else {
        state.set_message(app, "当前应用已是最新版本");
        return Ok(());
    };
    ensure!(
        update.download_url.scheme() == "https",
        "应用更新下载必须使用 HTTPS"
    );
    let version = update.version.clone();
    *state.app_update.lock().unwrap() = Some(update);
    state.set_message(app, format!("发现应用更新 · {version}"));
    Ok(())
}

async fn update_app(app: &AppHandle, state: &AppState) -> Result<()> {
    let update = state
        .app_update
        .lock()
        .unwrap()
        .take()
        .context("请先检查应用更新")?;
    state.set_message(app, format!("正在下载并验证应用 {}…", update.version));
    let mut downloaded = 0u64;
    let mut displayed_mib = 0u64;
    let package = update
        .download(
            |chunk, total| {
                downloaded = downloaded.saturating_add(chunk as u64);
                let mib = downloaded / (1024 * 1024);
                if mib > displayed_mib {
                    displayed_mib = mib;
                    let progress = match total.filter(|total| *total > 0) {
                        Some(total) => {
                            format!("已下载 {mib} / {} MiB", total.div_ceil(1024 * 1024))
                        }
                        None => format!("已下载 {mib} MiB"),
                    };
                    state.set_message(app, format!("正在下载应用 {} · {progress}", update.version));
                }
            },
            || state.set_message(app, "下载完成，正在验证应用更新…"),
        )
        .await?;

    // The Windows updater exits the process itself. Finish the owned core's
    // shutdown before handing over so installation cannot interrupt cleanup.
    if state.quitting.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    let reopen = window::current(app);
    let was_running = state.core.status().phase == CorePhase::Running;
    state.set_message(app, format!("正在安装应用 {}…", update.version));
    let result = async {
        window::destroy(app, state).await?;
        state.core.stop().await?;
        update.install(package)?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if let Err(error) = result {
        state.quitting.store(false, Ordering::SeqCst);
        if was_running {
            if let Err(recovery) = ensure_started(app, state).await {
                bail!("应用更新失败且内核未能恢复。更新错误：{error:#}；恢复错误：{recovery:#}");
            }
            if let Some(target) = reopen {
                window::show(app, state, target)
                    .await
                    .with_context(|| format!("应用更新失败后恢复窗口：{error:#}"))?;
            }
        }
        return Err(error).context("安装应用更新");
    }
    app.request_restart();
    Ok(())
}

async fn switch_core(
    app: &AppHandle,
    state: &AppState,
    candidate: &InstalledCore,
    commit: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let previous = state.updater.load_active()?;
    let reopen = window::current(app);
    let was_running = state.core.status().phase == CorePhase::Running;
    window::destroy(app, state).await?;
    state.set_message(app, "正在验证候选内核…");
    state.core.stop().await?;
    let result = match start_core(state, candidate).await {
        Ok(()) => activate_started_core(state, commit),
        Err(error) => Err(error),
    };
    if let Err(error) = result {
        // Keep ownership until the candidate's tree has actually exited. Windows
        // termination is asynchronous; releasing its handle and immediately
        // binding the old core's port would race the candidate's shutdown.
        if let Err(cleanup) = state.core.stop().await {
            bail!("新内核验证失败且进程回收尚未完成，未启动旧内核以避免端口冲突。更新错误：{error:#}；回收错误：{cleanup:#}");
        }
        if !state.quitting.load(Ordering::SeqCst) && was_running {
            if let Some(previous) = previous.as_ref() {
                if let Err(recovery) = start_core(state, previous).await {
                    bail!("新内核验证失败，旧内核也未能恢复。更新错误：{error:#}；恢复错误：{recovery:#}");
                }
                if let Some(target) = reopen {
                    let _ = window::show(app, state, target).await;
                }
            }
        }
        return Err(error).context("新内核未激活；原 active.json 保持不变");
    }
    state.set_message(app, "内核运行中");
    if let Some(target) = reopen {
        window::show(app, state, target).await?;
    }
    Ok(())
}

pub fn quit(app: &AppHandle) {
    let Some(state) = app
        .try_state::<Arc<AppState>>()
        .map(|state| Arc::clone(&*state))
    else {
        app.exit(0);
        return;
    };
    if state.quitting.swap(true, Ordering::SeqCst) {
        return;
    }
    state.set_message(app, "正在退出并回收进程…");
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        // Do not wait indefinitely for a download or a stuck startup operation.
        let bounded = tokio::time::timeout(Duration::from_secs(12), async {
            // CoreManager serializes process operations itself. Waiting for the
            // outer operation lock here would make a network download delay quit.
            let _ = window::destroy(&app, &state).await;
            state.core.stop().await
        })
        .await;
        if !matches!(bounded, Ok(Ok(()))) {
            state.core.emergency_stop_sync();
        }
        app.exit(0);
    });
}

pub fn open_path(app: &AppHandle, selector: &str) {
    let Some(state) = app.try_state::<Arc<AppState>>() else {
        return;
    };
    let path: &Path = match selector {
        "config" => &state.paths.config,
        "logs" => &state.paths.logs,
        _ => &state.paths.root,
    };
    if let Err(error) = open::that(path) {
        report(app, &state, error.into());
    }
}

fn notify(app: &AppHandle, state: &AppState, body: &str) {
    if state
        .settings()
        .map(|settings| settings.notifications)
        .unwrap_or(false)
    {
        let _ = app
            .notification()
            .builder()
            .title("Orca-Tauri")
            .body(body)
            .show();
    }
}

fn report(app: &AppHandle, state: &AppState, error: anyhow::Error) {
    // Error contexts must never contain pairing URLs or environment values.
    tracing::error!(error = %error, "native operation failed");
    state.set_message(app, format!("操作失败 · {}", error));
    notify(
        app,
        state,
        &format!("{}。可从托盘“设置 → 高级 → 打开日志目录”查看详情。", error),
    );
}

pub fn run() -> Result<()> {
    let root = AppPaths::discover()?;
    let config = Config::load_or_create(&root)?;
    let paths = AppPaths::initialize(root, &config)?;
    let file_appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("shell")
        .filename_suffix("log")
        .max_log_files(7)
        .build(&paths.logs)?;
    let (writer, _log_guard) = tracing_appender::non_blocking(file_appender);
    let _ = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(writer)
        .with_env_filter(tracing_subscriber::EnvFilter::new("orca_tauri=info"))
        .try_init();
    let updater = CoreUpdater::new(
        paths.core.clone(),
        config.update_repository.clone(),
        env!("ORCA_TAURI_TARGET").into(),
    )?
    .with_proxy(config.download_proxy.as_deref())?;
    let state = Arc::new(AppState {
        config: RwLock::new(config),
        paths,
        core: CoreManager::new()?,
        updater,
        operation: tokio::sync::Mutex::new(()),
        window_operation: tokio::sync::Mutex::new(()),
        quitting: AtomicBool::new(false),
        busy: AtomicBool::new(false),
        pending_show: AtomicBool::new(false),
        message: Mutex::new("内核未启动".into()),
        latest_official_version: Mutex::new(None),
        tasks: Mutex::new(None),
        retry_tasks: tokio::sync::Notify::new(),
        core_update: Mutex::new(None),
        app_update: Mutex::new(None),
    });
    let setup_state = state.clone();
    let context = tauri::generate_context!();
    let builder = tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            tray::tray_state,
            tray::tray_action
        ])
        // This plugin must precede all other plugins and runtime creation.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            request(app, Action::Show)
        }))
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, _shortcut, event| {
                    if event.state() == ShortcutState::Pressed {
                        request(app, Action::Toggle);
                    }
                })
                .build(),
        )
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--background"]),
        ))
        .plugin(tauri_plugin_notification::init());
    let builder = if context.config().plugins.0.contains_key("updater") {
        builder.plugin(tauri_plugin_updater::Builder::new().build())
    } else {
        builder
    };
    let app = builder
        .manage(state.clone())
        .setup(move |app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            tray::create(app, &setup_state)?;
            let settings = setup_state.settings()?;
            if let Err(error) = app
                .global_shortcut()
                .register(settings.global_shortcut.as_str())
            {
                tracing::warn!(error = %error, "global shortcut unavailable; tray remains usable");
                notify(
                    app.handle(),
                    &setup_state,
                    "快捷键注册失败；可通过托盘打开窗口。",
                );
            }
            let autostart = app.autolaunch();
            let result = if settings.autostart {
                autostart.enable()
            } else {
                autostart.disable()
            };
            if let Err(error) = result {
                tracing::warn!(error = %error, "could not sync autostart preference");
            }
            let app_handle = app.handle().clone();
            let startup_state = setup_state.clone();
            tauri::async_runtime::spawn(async move {
                if settings.start_core_on_launch {
                    startup_state.busy.store(true, Ordering::SeqCst);
                    let _guard = startup_state.operation.lock().await;
                    match startup_state.updater.load_active() {
                        Ok(None) if bundled_core_archive(&app_handle).is_none() => {
                            startup_state.set_message(&app_handle, "尚未安装内核");
                        }
                        Ok(_) => {
                            if let Err(error) = ensure_started(&app_handle, &startup_state).await {
                                report(&app_handle, &startup_state, error);
                            }
                        }
                        Err(error) => report(&app_handle, &startup_state, error),
                    }
                    startup_state.busy.store(false, Ordering::SeqCst);
                    tray::refresh(&app_handle, &startup_state);
                }
                if settings.open_window_on_start
                    && !std::env::args().any(|arg| arg == "--background")
                {
                    request(&app_handle, Action::Show);
                }
                if startup_state.pending_show.swap(false, Ordering::SeqCst) {
                    request(&app_handle, Action::Show);
                }
            });
            let signal_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                #[cfg(unix)]
                {
                    use tokio::signal::unix::{signal, SignalKind};
                    if let (Ok(mut term), Ok(mut interrupt)) = (
                        signal(SignalKind::terminate()),
                        signal(SignalKind::interrupt()),
                    ) {
                        tokio::select! { _ = term.recv() => (), _ = interrupt.recv() => () }
                        quit(&signal_handle);
                    }
                }
                #[cfg(windows)]
                if tokio::signal::ctrl_c().await.is_ok() {
                    quit(&signal_handle);
                }
            });
            // Read task status over the owned core’s local IPC, even with no window.
            let monitor_handle = app.handle().clone();
            let monitor_state = setup_state.clone();
            tauri::async_runtime::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_secs(3));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                let mut last_poll = None::<(u32, tokio::time::Instant)>;
                loop {
                    let retry = tokio::select! {
                        _ = interval.tick() => false,
                        _ = monitor_state.retry_tasks.notified() => true,
                    };
                    if monitor_state.quitting.load(Ordering::SeqCst) {
                        continue;
                    }
                    let status = monitor_state.core.status();
                    tray::refresh(&monitor_handle, &monitor_state);
                    if status.phase != CorePhase::Running {
                        last_poll = None;
                        if let Ok(mut tasks) = monitor_state.tasks.lock() {
                            *tasks = None;
                        }
                        continue;
                    }
                    let Some(pid) = status.pid else { continue };
                    if !retry
                        && last_poll.is_some_and(|(previous, at)| {
                            previous == pid && at.elapsed() < Duration::from_secs(5)
                        })
                    {
                        continue;
                    }
                    last_poll = Some((pid, tokio::time::Instant::now()));
                    let summary = task_status::read_owned(&monitor_state.paths.runtime_data, pid)
                        .await
                        .ok()
                        .map(Arc::new);
                    let current = monitor_state.core.status();
                    if current.phase != CorePhase::Running || current.pid != Some(pid) {
                        continue;
                    }
                    let snapshot = Some(TaskSnapshot { pid, summary });
                    let mut changed = false;
                    if let Ok(mut tasks) = monitor_state.tasks.lock() {
                        changed = *tasks != snapshot;
                        *tasks = snapshot;
                    }
                    if changed {
                        tray::refresh(&monitor_handle, &monitor_state);
                    }
                }
            });
            Ok(())
        })
        .build(context)
        .context("initialize desktop shell")?;
    app.run(move |app, event| match event {
        RunEvent::ExitRequested { api, code, .. } => {
            // Destroying the last window can request an implicit exit even
            // during quit(). Keep the event loop alive until cleanup calls exit().
            if code.is_none() {
                api.prevent_exit();
            } else if !state.quitting.load(Ordering::SeqCst) {
                api.prevent_exit();
                quit(app);
            }
        }
        RunEvent::Exit => state.core.emergency_stop_sync(),
        _ => {}
    });
    Ok(())
}
