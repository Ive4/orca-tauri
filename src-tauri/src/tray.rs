use crate::{
    desktop::{self, Action, AppState},
    process::CorePhase,
    task_status::TaskStatus,
    window,
};
use anyhow::{Context, Result};
use semver::Version;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    cmp::Ordering,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tauri::{
    image::Image,
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    webview::NewWindowResponse,
    App, AppHandle, Manager, PhysicalPosition, PhysicalSize, Rect, WebviewUrl, WebviewWindow,
    WebviewWindowBuilder, WindowEvent,
};
use tauri_plugin_autostart::ManagerExt;

const LABEL: &str = "tray-panel";
const ENTRY: &str = "/tray/index.html";
const WIDTH: f64 = 348.;
const MAX_HEIGHT: f64 = 430.;

#[derive(Default)]
struct Panel {
    transition: tokio::sync::Mutex<()>,
    anchor: Mutex<Option<Anchor>>,
    dismissed_on_icon: Mutex<Option<Instant>>,
    badge: Mutex<usize>,
}

#[derive(Clone, Copy)]
struct Anchor {
    left: f64,
    right: f64,
    top: f64,
    bottom: f64,
    work_left: f64,
    work_right: f64,
    work_top: f64,
    work_bottom: f64,
    scale: f64,
    tray_rect: bool,
}

impl Anchor {
    fn at(app: &AppHandle, point: PhysicalPosition<f64>, rect: Option<Rect>) -> Result<Self> {
        let primary = app.primary_monitor()?;
        // macOS tray events are multiplied by the status item's display scale,
        // but Tao's screen lookup uses global points. Use the cursor sample
        // normalized with the primary display scale Tao applied to it.
        #[cfg(target_os = "macos")]
        let lookup = app.cursor_position().unwrap_or(point).to_logical::<f64>(
            primary
                .as_ref()
                .map_or(1., |monitor| monitor.scale_factor()),
        );
        #[cfg(not(target_os = "macos"))]
        let lookup = point;
        let monitor = app
            .monitor_from_point(lookup.x, lookup.y)?
            .or(primary)
            .context("no screen for tray popup")?;
        let scale = monitor.scale_factor();
        let work = monitor.work_area();
        let (origin, size) = rect
            .map(|rect| {
                (
                    rect.position.to_physical::<f64>(scale),
                    rect.size.to_physical::<f64>(scale),
                )
            })
            .unwrap_or((point, PhysicalSize::new(0., 0.)));
        Ok(Self {
            left: origin.x,
            right: origin.x + size.width,
            top: origin.y,
            bottom: origin.y + size.height,
            work_left: work.position.x as f64,
            work_right: work.position.x as f64 + work.size.width as f64,
            work_top: work.position.y as f64,
            work_bottom: work.position.y as f64 + work.size.height as f64,
            scale,
            tray_rect: rect.is_some(),
        })
    }

    fn width(self) -> f64 {
        WIDTH.min((self.work_right - self.work_left) / self.scale)
    }

    fn below(self) -> bool {
        let below = self.work_bottom - self.bottom.max(self.work_top);
        let above = self.top.min(self.work_bottom) - self.work_top;
        below >= MAX_HEIGHT * self.scale || below >= above
    }

    fn available(self) -> f64 {
        let space = if self.below() {
            self.work_bottom - self.bottom.max(self.work_top)
        } else {
            self.top.min(self.work_bottom) - self.work_top
        };
        (space / self.scale - 4.).clamp(64., MAX_HEIGHT).floor()
    }

    fn resize(self, window: &WebviewWindow, requested: f64) -> tauri::Result<()> {
        let gap = 4. * self.scale;
        let width = self.width() * self.scale;
        let below = self.below();
        let edge = if below {
            self.bottom.max(self.work_top)
        } else {
            self.top.min(self.work_bottom)
        };
        let available = if below {
            self.work_bottom - edge
        } else {
            edge - self.work_top
        };
        let height = (requested.clamp(64., MAX_HEIGHT) * self.scale).min((available - gap).max(1.));
        let x =
            (self.right + 14. * self.scale - width).clamp(self.work_left, self.work_right - width);
        let y = if below {
            edge + gap
        } else {
            edge - gap - height
        };
        #[cfg(target_os = "macos")]
        {
            window.set_position(tauri::LogicalPosition::new(x / self.scale, y / self.scale))?;
            window.set_size(tauri::LogicalSize::new(
                width / self.scale,
                height / self.scale,
            ))
        }
        #[cfg(not(target_os = "macos"))]
        {
            window.set_position(PhysicalPosition::new(x.round() as i32, y.round() as i32))?;
            window.set_size(PhysicalSize::new(
                width.round() as u32,
                height.round() as u32,
            ))
        }
    }

    fn pointer(self) -> f64 {
        let width = self.width() * self.scale;
        let x =
            (self.right + 14. * self.scale - width).clamp(self.work_left, self.work_right - width);
        (((self.left + self.right) * 0.5 - x) / self.scale).clamp(12., self.width() - 12.)
    }

    fn contains_icon(self, position: PhysicalPosition<f64>) -> bool {
        self.tray_rect
            && (self.left..=self.right).contains(&position.x)
            && (self.top..=self.bottom).contains(&position.y)
    }
}

pub fn create(app: &mut App, state: &AppState) -> Result<()> {
    let open = MenuItem::with_id(app, "show", "打开 Orca", true, None::<&str>)?;
    #[cfg(target_os = "linux")]
    let tasks = MenuItem::with_id(app, "tasks", "任务", true, None::<&str>)?;
    #[cfg(target_os = "linux")]
    let menu = Menu::with_items(app, &[&tasks, &open])?;
    #[cfg(not(target_os = "linux"))]
    let menu = Menu::with_items(app, &[&open])?;
    app.manage(Panel::default());
    let builder = TrayIconBuilder::with_id("orca-tray").icon(tray_icon(0)?);
    #[cfg(target_os = "macos")]
    let builder = builder.icon_as_template(true);
    builder
        .tooltip("Orca")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "show" => desktop::request(app, Action::Show),
            #[cfg(target_os = "linux")]
            "tasks" => {
                if let Ok(point) = app.cursor_position() {
                    toggle(app, point, None);
                }
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                position,
                rect,
                ..
            } = event
            {
                toggle(tray.app_handle(), position, Some(rect));
            }
        })
        .build(app)?;
    refresh(app.handle(), state);
    Ok(())
}

#[cfg(target_os = "macos")]
fn tray_icon(_waiting: usize) -> tauri::Result<Image<'static>> {
    Image::from_bytes(include_bytes!("../icons/tray-cat.png"))
}

#[cfg(not(target_os = "macos"))]
fn tray_icon(waiting: usize) -> tauri::Result<Image<'static>> {
    let icon = Image::from_bytes(include_bytes!("../icons/tray-cat-color.png"))?;
    if waiting == 0 {
        return Ok(icon);
    }
    let mut rgba = icon.rgba().to_vec();
    let mut pixel = |x, y, color: [u8; 4]| {
        let offset = (y * 32 + x) * 4;
        rgba[offset..offset + 4].copy_from_slice(&color);
    };
    for y in 0..13 {
        for x in 19..32 {
            let dx = x as i32 - 25;
            let dy = y as i32 - 6;
            if dx * dx + dy * dy <= 40 {
                pixel(x, y, [250, 218, 157, 255]);
            }
        }
    }
    let digits: [[u8; 5]; 9] = [
        [2, 6, 2, 2, 7],
        [7, 1, 7, 4, 7],
        [7, 1, 7, 1, 7],
        [5, 5, 7, 1, 1],
        [7, 4, 7, 1, 7],
        [7, 4, 7, 5, 7],
        [7, 1, 2, 2, 2],
        [7, 5, 7, 5, 7],
        [7, 5, 7, 1, 7],
    ];
    let color = [85, 50, 10, 255];
    let left = if waiting > 9 { 22 } else { 24 };
    for (y, mask) in digits[waiting.min(9) - 1].iter().enumerate() {
        for x in 0..3 {
            if mask & (4 >> x) != 0 {
                pixel(left + x, y + 4, color);
            }
        }
    }
    if waiting > 9 {
        for (x, y) in [(28, 5), (27, 6), (28, 6), (29, 6), (28, 7)] {
            pixel(x, y, color);
        }
    }
    Ok(Image::new_owned(rgba, 32, 32))
}

pub fn refresh(app: &AppHandle, state: &AppState) {
    let Some(panel) = app.try_state::<Panel>() else {
        return;
    };
    let Some(tray) = app.tray_by_id("orca-tray") else {
        return;
    };
    let core = state.core.status();
    let waiting = core
        .pid
        .filter(|_| core.phase == CorePhase::Running)
        .and_then(|pid| state.tasks_for(pid)?.summary)
        .filter(|summary| !summary.partial)
        .map_or(0, |summary| {
            summary
                .rows
                .iter()
                .filter(|row| matches!(row.status, TaskStatus::Waiting | TaskStatus::Blocked))
                .count()
        });
    {
        let Ok(mut badge) = panel.badge.lock() else {
            return;
        };
        if *badge == waiting {
            return;
        }
        *badge = waiting;
    }
    let _ = tray.set_tooltip(Some(if waiting == 0 {
        "Orca".into()
    } else {
        format!("Orca · {waiting} 项待处理")
    }));
    #[cfg(target_os = "macos")]
    let _ = tray.set_title((waiting > 0).then(|| waiting.to_string()));
    #[cfg(not(target_os = "macos"))]
    if let Ok(icon) = tray_icon(waiting) {
        let _ = tray.set_icon(Some(icon));
    }
}

fn toggle(app: &AppHandle, point: PhysicalPosition<f64>, rect: Option<Rect>) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(error) = toggle_panel(&app, point, rect).await {
            tracing::warn!(error = %error, "could not toggle tray panel");
        }
    });
}

async fn toggle_panel(
    app: &AppHandle,
    point: PhysicalPosition<f64>,
    rect: Option<Rect>,
) -> Result<()> {
    let panel = app.state::<Panel>();
    let Ok(_guard) = panel.transition.try_lock() else {
        return Ok(());
    };
    let suppressed = panel
        .dismissed_on_icon
        .lock()
        .unwrap()
        .take()
        .is_some_and(|time| time.elapsed() < Duration::from_millis(500));
    if let Some(window) = app.get_webview_window(LABEL) {
        window.destroy()?;
        return Ok(());
    }
    if suppressed
        || app
            .state::<Arc<AppState>>()
            .quitting
            .load(std::sync::atomic::Ordering::SeqCst)
    {
        return Ok(());
    }

    let anchor = Anchor::at(app, point, rect)?;
    *panel.anchor.lock().unwrap() = Some(anchor);
    let window = WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("tray/index.html".into()))
        .title("Orca 任务")
        .inner_size(anchor.width(), 360.)
        .visible(false)
        .focused(false)
        .decorations(false)
        .resizable(false)
        .skip_taskbar(true)
        .always_on_top(true)
        .visible_on_all_workspaces(true)
        .transparent(true)
        .shadow(true)
        .accept_first_mouse(true)
        .disable_drag_drop_handler()
        .on_navigation(|url| {
            #[cfg(windows)]
            let bundled = ("http", Some("tauri.localhost"));
            #[cfg(not(windows))]
            let bundled = ("tauri", Some("localhost"));
            (url.scheme(), url.host_str()) == bundled
                && url.path() == ENTRY
                && url.port().is_none()
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
        })
        .on_new_window(|_, _| NewWindowResponse::Deny)
        .build()?;
    let popup = window.clone();
    let handle = app.clone();
    window.on_window_event(move |event| {
        if matches!(event, WindowEvent::Focused(false)) && popup.is_visible().unwrap_or(false) {
            if handle.cursor_position().is_ok_and(|point| {
                #[cfg(target_os = "macos")]
                let point = point
                    .to_logical::<f64>(
                        handle
                            .primary_monitor()
                            .ok()
                            .flatten()
                            .map_or(1., |monitor| monitor.scale_factor()),
                    )
                    .to_physical::<f64>(anchor.scale);
                anchor.contains_icon(point)
            }) {
                *handle.state::<Panel>().dismissed_on_icon.lock().unwrap() = Some(Instant::now());
            }
            let _ = popup.destroy();
        }
    });
    // Do not leave a failed local load alive in the background.
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_secs(5)).await;
        if window.is_visible().is_ok_and(|visible| !visible) {
            let _ = window.destroy();
        }
    });
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PanelPage {
    Tasks,
    Settings,
    About,
    Advanced,
}

#[tauri::command]
pub(crate) async fn tray_state(window: WebviewWindow, page: PanelPage) -> Result<Value, String> {
    if window.label() != LABEL {
        return Err("此窗口不能读取托盘状态".into());
    }
    let app = window.app_handle();
    let state = app.state::<Arc<AppState>>();
    let anchor = app
        .state::<Panel>()
        .anchor
        .lock()
        .unwrap()
        .ok_or("缺少托盘位置")?;
    let core = state.core.status();
    let snapshot = core.pid.and_then(|pid| state.tasks_for(pid));
    let status = match core.phase {
        CorePhase::Running
            if snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.summary.is_some()) =>
        {
            "ready"
        }
        CorePhase::Running if snapshot.is_some() => "unavailable",
        CorePhase::Running | CorePhase::Starting => "loading",
        CorePhase::Stopped => "stopped",
        CorePhase::Failed | CorePhase::Stopping => "offline",
    };
    let busy = state.is_busy();
    let shell = match page {
        PanelPage::Tasks => Value::Null,
        PanelPage::Settings => json!({
            "autostart": app.autolaunch().is_enabled().unwrap_or(false),
            "windowOpen": window::current(app).is_some(),
        }),
        PanelPage::About => {
            let active = state.updater.load_active().ok().flatten();
            let (pending_app, pending_core) = state.available_updates();
            json!({
                "appVersion": app.package_info().version.to_string(),
                "coreVersion": active.as_ref().map(|core| core.manifest.display_version()),
                "checkApp": app.config().plugins.0.contains_key("updater"),
                "checkCore": state.settings().is_ok_and(|s| s.update_repository.is_some()),
                "pendingApp": pending_app,
                "pendingCore": pending_core,
                "officialVersion": state.latest_official_version().map(|latest|
                    upstream_label(active.as_ref().map(|core| core.manifest.version.as_str()), &latest)
                ),
            })
        }
        PanelPage::Advanced => json!({
            "coreInstalled": state.updater.load_active().ok().flatten().is_some(),
            "rollbackAvailable": state.updater.load_previous().ok().flatten().is_some(),
            "running": core.phase == CorePhase::Running,
        }),
    };
    let summary = matches!(page, PanelPage::Tasks)
        .then(|| snapshot.and_then(|snapshot| snapshot.summary))
        .flatten();
    Ok(json!({
        "taskState": status,
        "summary": summary.as_deref(),
        "canOpen": !busy || matches!(window::current(app), Some(window::Target::Local)),
        "busy": busy,
        "shell": shell,
        "message": (!matches!(page, PanelPage::Tasks)).then(|| state.message().chars().take(180).collect::<String>()),
        "below": anchor.below(),
        "pointer": anchor.pointer(),
        "availableHeight": anchor.available(),
    }))
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum PanelAction {
    Resize { height: u16 },
    Close,
    RetryTasks,
    Open { id: Option<String> },
    OpenPaired,
    CloseOrca,
    CheckApp,
    CheckCore,
    CheckOfficial,
    InstallApp,
    InstallCore,
    Autostart,
    Restart,
    Stop,
    Rollback,
    Config,
    Logs,
    Data,
    Quit,
}

#[tauri::command]
pub(crate) async fn tray_action(window: WebviewWindow, action: PanelAction) -> Result<(), String> {
    if window.label() != LABEL {
        return Err("此窗口不能操作托盘".into());
    }
    let app = window.app_handle();
    let state = app.state::<Arc<AppState>>();
    let action = match action {
        PanelAction::Resize { height } => {
            let anchor = app
                .state::<Panel>()
                .anchor
                .lock()
                .unwrap()
                .ok_or("缺少托盘位置")?;
            anchor
                .resize(&window, f64::from(height))
                .map_err(|_| "无法调整任务弹窗")?;
            if !window.is_visible().map_err(|_| "无法读取任务弹窗")? {
                window
                    .show()
                    .and_then(|()| window.set_focus())
                    .map_err(|_| "无法显示任务弹窗")?;
            }
            return Ok(());
        }
        PanelAction::Close => return window.destroy().map_err(|_| "无法关闭任务弹窗".into()),
        PanelAction::RetryTasks => {
            state.retry_tasks();
            return Ok(());
        }
        PanelAction::Open { id } => {
            let action = if let Some(id) = id {
                let core = state.core.status();
                let summary = core
                    .pid
                    .filter(|_| core.phase == CorePhase::Running)
                    .and_then(|pid| state.tasks_for(pid)?.summary)
                    .ok_or("任务状态已变化，请刷新后重试")?;
                let target = summary
                    .rows
                    .iter()
                    .find(|row| row.id == id)
                    .ok_or("此任务已不在列表中")?
                    .target
                    .clone();
                if target.worktree_id.is_empty() || target.tab_id.is_empty() {
                    Action::Show
                } else {
                    Action::ShowTask(target)
                }
            } else {
                Action::Show
            };
            if state.is_busy() && !matches!(window::current(app), Some(window::Target::Local)) {
                return Err("Orca 正忙，请稍后再试".into());
            }
            window.destroy().map_err(|_| "无法关闭任务弹窗")?;
            action
        }
        PanelAction::OpenPaired => {
            if state.is_busy() {
                return Err("Orca 正忙，请稍后再试".into());
            }
            window.destroy().map_err(|_| "无法关闭任务弹窗")?;
            Action::ShowPaired
        }
        PanelAction::CloseOrca => Action::Destroy,
        PanelAction::CheckApp => Action::CheckAppUpdate,
        PanelAction::CheckCore => Action::CheckCoreUpdate,
        PanelAction::CheckOfficial => Action::CheckOfficialVersion,
        PanelAction::InstallApp => Action::UpdateApp,
        PanelAction::InstallCore => Action::Update,
        PanelAction::Autostart => Action::Autostart,
        PanelAction::Restart => Action::Restart,
        PanelAction::Stop => Action::Stop,
        PanelAction::Rollback => Action::Rollback,
        PanelAction::Config => {
            desktop::open_path(app, "config");
            return Ok(());
        }
        PanelAction::Logs => {
            desktop::open_path(app, "logs");
            return Ok(());
        }
        PanelAction::Data => {
            desktop::open_path(app, "data");
            return Ok(());
        }
        PanelAction::Quit => {
            desktop::quit(app);
            return Ok(());
        }
    };
    desktop::request(app, action);
    Ok(())
}

fn upstream_label(current: Option<&str>, latest: &Version) -> String {
    let label = format!("官方最新 v{latest}");
    let Some(current) = current
        .and_then(|tag| tag.strip_prefix('v'))
        .and_then(|version| Version::parse(version).ok())
    else {
        return label;
    };
    match current.cmp_precedence(latest) {
        Ordering::Equal => format!("{label} · 已同步"),
        Ordering::Greater => format!("{label} · 本机版本更高"),
        Ordering::Less
            if current.major == latest.major
                && current.minor == latest.minor
                && current.patch < latest.patch
                && current.pre.is_empty() =>
        {
            format!("{label} · 低 {} 个补丁号", latest.patch - current.patch)
        }
        Ordering::Less => format!("{label} · 有新版"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_label_compares_release_versions() {
        let latest = semver::Version::parse("1.4.265").unwrap();
        for (current, expected) in [
            (Some("v1.4.265"), "官方最新 v1.4.265 · 已同步"),
            (Some("v1.4.264"), "官方最新 v1.4.265 · 低 1 个补丁号"),
            (Some("v1.3.264"), "官方最新 v1.4.265 · 有新版"),
            (Some("v1.4.266"), "官方最新 v1.4.265 · 本机版本更高"),
            (None, "官方最新 v1.4.265"),
        ] {
            assert_eq!(upstream_label(current, &latest), expected);
        }
    }
}
