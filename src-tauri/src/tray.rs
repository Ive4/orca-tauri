use crate::{
    desktop::{self, Action, AppState, TaskSnapshot},
    process::CorePhase,
    task_status::TaskSummary,
    window,
};
use anyhow::Result;
use semver::Version;
use std::cmp::Ordering;
use tauri::{
    image::Image,
    menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    App, AppHandle, Manager, Wry,
};
use tauri_plugin_autostart::ManagerExt;

struct TrayItems {
    status: MenuItem<Wry>,
    tasks: Submenu<Wry>,
    agent_status: MenuItem<Wry>,
    terminal_status: MenuItem<Wry>,
    app_progress: MenuItem<Wry>,
    core_version: MenuItem<Wry>,
    check_official_version: MenuItem<Wry>,
    ui_version: MenuItem<Wry>,
    open: MenuItem<Wry>,
    open_paired: MenuItem<Wry>,
    close: MenuItem<Wry>,
    restart: MenuItem<Wry>,
    stop: MenuItem<Wry>,
    update: MenuItem<Wry>,
    update_app: MenuItem<Wry>,
    rollback: MenuItem<Wry>,
    autostart: CheckMenuItem<Wry>,
}

pub fn create(app: &mut App, state: &AppState) -> Result<()> {
    let status = MenuItem::with_id(
        app,
        "status",
        "Orca-Tauri · 正在初始化",
        false,
        None::<&str>,
    )?;
    let agent_status = MenuItem::new(app, "Agent · 等待内核启动", false, None::<&str>)?;
    let terminal_status = MenuItem::new(app, "终端 · 等待内核启动", false, None::<&str>)?;
    let app_progress = MenuItem::new(app, "应用进展", false, None::<&str>)?;
    let app_version = MenuItem::new(
        app,
        format!("外壳版本 {}", app.package_info().version),
        false,
        None::<&str>,
    )?;
    let core_version = MenuItem::new(app, "内核版本", false, None::<&str>)?;
    let check_official_version = MenuItem::with_id(
        app,
        "check-official-version",
        "查询 Orca 官方最新稳定版…",
        true,
        None::<&str>,
    )?;
    let ui_version = MenuItem::new(app, "UI版本", false, None::<&str>)?;
    let open = MenuItem::with_id(app, "show", "打开独立 Orca", true, None::<&str>)?;
    let open_paired = MenuItem::with_id(app, "show-paired", "连接官方 Orca…", true, None::<&str>)?;
    let close = MenuItem::with_id(app, "destroy", "关闭窗口", false, None::<&str>)?;
    let restart = MenuItem::with_id(
        app,
        "restart",
        "重启内核 / 重新配对（将停止任务）",
        true,
        None::<&str>,
    )?;
    let stop = MenuItem::with_id(app, "stop", "停止内核及其所有任务", true, None::<&str>)?;
    let update = MenuItem::with_id(app, "update", "检查并更新内核…", true, None::<&str>)?;
    let update_app = MenuItem::with_id(
        app,
        "update-app",
        "检查并安装应用更新…",
        false,
        None::<&str>,
    )?;
    let rollback = MenuItem::with_id(app, "rollback", "回退到上一版内核", false, None::<&str>)?;
    let autostart = CheckMenuItem::with_id(
        app,
        "autostart",
        "开机启动",
        true,
        state.settings()?.autostart,
        None::<&str>,
    )?;
    let config = MenuItem::with_id(app, "config", "打开配置文件", true, None::<&str>)?;
    let logs = MenuItem::with_id(app, "logs", "打开日志目录", true, None::<&str>)?;
    let data = MenuItem::with_id(app, "data", "打开数据目录", true, None::<&str>)?;
    let quit = MenuItem::with_id(
        app,
        "quit",
        "退出 Orca-Tauri（将停止任务）",
        true,
        None::<&str>,
    )?;
    let tasks = Submenu::with_items(app, "Orca 任务", true, &[&agent_status, &terminal_status])?;
    let maintenance = Submenu::with_items(app, "内核管理", true, &[&restart, &stop, &rollback])?;
    let about = Submenu::with_items(
        app,
        "关于与更新",
        true,
        &[
            &app_version,
            &core_version,
            &ui_version,
            &check_official_version,
            &PredefinedMenuItem::separator(app)?,
            &update_app,
            &update,
            &app_progress,
            &MenuItem::new(app, "安装更新将停止任务并重启", false, None::<&str>)?,
        ],
    )?;
    let settings = Submenu::with_items(
        app,
        "设置",
        true,
        &[
            &autostart,
            &about,
            &PredefinedMenuItem::separator(app)?,
            &config,
            &logs,
            &data,
        ],
    )?;
    let menu = Menu::with_items(
        app,
        &[
            &status,
            &PredefinedMenuItem::separator(app)?,
            &open,
            &open_paired,
            &close,
            &PredefinedMenuItem::separator(app)?,
            &tasks,
            &maintenance,
            &settings,
            &PredefinedMenuItem::separator(app)?,
            &quit,
        ],
    )?;
    let icon = Image::from_bytes(include_bytes!("../icons/32x32.png"))?;
    TrayIconBuilder::with_id("orca-tray")
        .icon(icon)
        .tooltip("Orca-Tauri")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "show" => desktop::request(app, Action::Show),
            "show-paired" => desktop::request(app, Action::ShowPaired),
            "destroy" => desktop::request(app, Action::Destroy),
            "restart" => desktop::request(app, Action::Restart),
            "stop" => desktop::request(app, Action::Stop),
            "check-official-version" => desktop::request(app, Action::CheckOfficialVersion),
            "update" => desktop::request(app, Action::Update),
            "update-app" => desktop::request(app, Action::UpdateApp),
            "rollback" => desktop::request(app, Action::Rollback),
            "autostart" => desktop::request(app, Action::Autostart),
            "config" => desktop::open_path(app, "config"),
            "logs" => desktop::open_path(app, "logs"),
            "data" => desktop::open_path(app, "data"),
            "quit" => desktop::quit(app),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                desktop::request(tray.app_handle(), Action::Show);
            }
        })
        .build(app)?;
    app.manage(TrayItems {
        status,
        tasks,
        agent_status,
        terminal_status,
        app_progress,
        core_version,
        check_official_version,
        ui_version,
        open,
        open_paired,
        close,
        restart,
        stop,
        update,
        update_app,
        rollback,
        autostart,
    });
    refresh(app.handle(), state);
    Ok(())
}

pub fn refresh(app: &AppHandle, state: &AppState) {
    let Some(items) = app.try_state::<TrayItems>() else {
        return;
    };
    let status = state.core.status();
    let busy = state.is_busy();
    let active = state.updater.load_active();
    let installed = matches!(&active, Ok(Some(_)));
    let version = match &active {
        Ok(Some(core)) => core.manifest.display_version(),
        Ok(None) => "尚未安装".into(),
        Err(_) => "无法读取".into(),
    };
    let _ = items.core_version.set_text(format!("内核版本 {version}"));
    let _ = items.ui_version.set_text(format!("UI版本 {version}"));
    let _ = items.check_official_version.set_enabled(!busy);
    let check_label = match state.latest_official_version() {
        Some(latest) => {
            let current = active.as_ref().ok().and_then(Option::as_ref);
            let result =
                upstream_label(current.map(|core| core.manifest.version.as_str()), &latest);
            format!("上次查询 · {result}（点击刷新）")
        }
        None => "查询 Orca 官方最新稳定版…".into(),
    };
    let _ = items.check_official_version.set_text(check_label);
    let phase = match status.phase {
        CorePhase::Stopped => "内核未启动",
        CorePhase::Starting => "内核正在启动",
        CorePhase::Running => "内核运行中",
        CorePhase::Stopping => "内核正在停止",
        CorePhase::Failed => "内核异常 · 请查看日志",
    };
    let _ = items.status.set_text(format!("Orca-Tauri · {phase}"));
    let snapshot = if status.phase == CorePhase::Running {
        status.pid.and_then(|pid| state.tasks_for(pid))
    } else {
        None
    };
    let summary = snapshot.and_then(|snapshot| snapshot.summary);
    let task_title = match summary.filter(|summary| !summary.partial) {
        Some(summary) if summary.waiting + summary.blocked > 0 => {
            format!("Orca 任务 · {} 待处理", summary.waiting + summary.blocked)
        }
        Some(summary) if summary.working > 0 => format!("Orca 任务 · {} 执行中", summary.working),
        _ => "Orca 任务".into(),
    };
    let _ = items.tasks.set_text(task_title);
    let (agents, terminals) = task_labels(status.phase, snapshot);
    let _ = items.agent_status.set_text(agents);
    let _ = items.terminal_status.set_text(terminals);
    let detail = state.message();
    let detail = detail.replace(['\r', '\n', '&'], " ");
    let detail = if detail.chars().count() > 84 {
        format!("{}…", detail.chars().take(83).collect::<String>())
    } else {
        detail
    };
    let _ = items.app_progress.set_text(format!("应用进展 · {detail}"));
    let window_open = window::current(app).is_some();
    let _ = items.open.set_enabled(!busy || window_open);
    let _ = items.open_paired.set_enabled(!busy);
    let _ = items.close.set_enabled(window_open);
    let _ = items.restart.set_enabled(installed && !busy);
    let _ = items
        .stop
        .set_enabled(status.phase == CorePhase::Running && !busy);
    let core_updates = state
        .settings()
        .is_ok_and(|s| s.update_repository.is_some());
    let _ = items.update.set_enabled(!busy && core_updates);
    let _ = items.update.set_text(if core_updates {
        "检查并更新内核…"
    } else {
        "内核更新未配置 · 请编辑配置文件"
    });
    let app_updates = app.config().plugins.0.contains_key("updater");
    let _ = items.update_app.set_enabled(!busy && app_updates);
    let _ = items.update_app.set_text(if app_updates {
        "检查并安装应用更新…"
    } else {
        "应用更新未配置 · 请安装正式发布版"
    });
    let _ = items
        .rollback
        .set_enabled(!busy && state.updater.load_previous().ok().flatten().is_some());
    let _ = items.autostart.set_enabled(!busy);
    if let Ok(enabled) = app.autolaunch().is_enabled() {
        let _ = items.autostart.set_checked(enabled);
    }
    if let Some(tray) = app.tray_by_id("orca-tray") {
        let tooltip: String = state.message().chars().take(100).collect();
        let _ = tray.set_tooltip(Some(&tooltip));
    }
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

fn task_labels(phase: CorePhase, snapshot: Option<TaskSnapshot>) -> (String, String) {
    let unavailable = |reason| (format!("Agent · {reason}"), format!("终端 · {reason}"));
    match phase {
        CorePhase::Stopped => return unavailable("内核未启动"),
        CorePhase::Starting => return unavailable("内核正在启动"),
        CorePhase::Stopping => return unavailable("内核正在停止"),
        CorePhase::Failed => return unavailable("内核异常"),
        CorePhase::Running => {}
    }
    match snapshot {
        Some(snapshot) => match snapshot.summary {
            Some(summary) => task_summary_labels(summary),
            None => unavailable("暂时无法读取"),
        },
        None => unavailable("正在读取…"),
    }
}

fn task_summary_labels(summary: TaskSummary) -> (String, String) {
    let agents = [
        (summary.working, "执行中"),
        (summary.waiting, "等待输入"),
        (summary.blocked, "受阻"),
        (summary.monitoring, "监测中"),
        (summary.ended, "已结束"),
        (summary.unknown, "待确认"),
    ]
    .into_iter()
    .filter(|(count, _)| *count > 0)
    .map(|(count, label)| format!("{count} {label}"))
    .collect::<Vec<_>>()
    .join(" · ");
    let agents = if agents.is_empty() {
        "暂无记录"
    } else {
        &agents
    };
    let mut terminals = format!("{} 个已连接", summary.terminals);
    if summary.unverified_terminals > 0 {
        terminals.push_str(&format!(" · {} 个待确认", summary.unverified_terminals));
    }
    let scope = if summary.partial {
        "（部分工作区）"
    } else {
        ""
    };
    (
        format!("Agent · {agents}{scope}"),
        format!("终端 · {terminals}{scope}"),
    )
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
