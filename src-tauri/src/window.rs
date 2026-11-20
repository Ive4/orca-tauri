use crate::{desktop::AppState, task_status::TaskTarget};
use anyhow::{ensure, Context, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use tauri::{
    webview::{NewWindowResponse, PageLoadEvent},
    AppHandle, Manager, WebviewUrl, WebviewWindowBuilder,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Local,
    Paired,
}

impl Target {
    fn label(self) -> &'static str {
        match self {
            Self::Local => "orca",
            Self::Paired => "orca-paired",
        }
    }
}

pub fn current(app: &AppHandle) -> Option<Target> {
    [Target::Local, Target::Paired]
        .into_iter()
        .find(|target| app.get_webview_window(target.label()).is_some())
}

/// Call from an async task, never directly from the tray/single-instance callback:
/// WebView2's synchronous builder can deadlock inside native event handlers.
pub async fn show(app: &AppHandle, state: &AppState, target: Target) -> Result<()> {
    show_with_task(app, state, target, None).await
}

pub async fn show_task(app: &AppHandle, state: &AppState, task: &TaskTarget) -> Result<()> {
    show_with_task(app, state, Target::Local, Some(task)).await
}

/// The download-time fast path may use a live window but can never replace one
/// which was destroyed while waiting for the window lock.
pub async fn reveal_existing_task(
    app: &AppHandle,
    state: &AppState,
    task: &TaskTarget,
) -> Result<()> {
    let _guard = state.window_operation.lock().await;
    if let Some(window) = app.get_webview_window(Target::Local.label()) {
        window.unminimize()?;
        window.set_focus()?;
        window.eval(reveal_script(task)?)?;
    }
    Ok(())
}

fn reveal_script(task: &TaskTarget) -> Result<String> {
    Ok(format!(
        "window.__orcaTrayReveal?.({})",
        serde_json::to_string(task)?
    ))
}

async fn show_with_task(
    app: &AppHandle,
    state: &AppState,
    target: Target,
    task: Option<&TaskTarget>,
) -> Result<()> {
    let reveal = task.map(reveal_script).transpose()?;
    let _guard = state.window_operation.lock().await;
    if state.quitting.load(Ordering::SeqCst) {
        return Ok(());
    }
    if let Some(window) = app.get_webview_window(target.label()) {
        window.unminimize()?;
        if window.is_visible()? {
            window.set_focus()?;
        }
        if let Some(script) = reveal {
            window.eval(script)?;
        }
        return Ok(());
    }
    let (value, port) = state.core.web_client_offer().context(
        "the core has no usable browser pairing offer; inspect logs and restart the core to create a new offer",
    )?;
    let mut url = url::Url::parse(&value).context("invalid core browser URL")?;
    ensure!(
        url.scheme() == "http"
            && url.host_str() == Some("127.0.0.1")
            && url.port() == Some(port)
            && url.username().is_empty()
            && url.password().is_none(),
        "core browser URL is outside the configured loopback origin"
    );
    let title = match target {
        Target::Local => "Orca",
        Target::Paired => {
            // The upstream web client keeps one pairing per origin. Serve the
            // same owned bundle at localhost with an independent pairing store.
            url.set_host(Some("localhost"))?;
            url.set_path("/web-index.html");
            url.set_query(None);
            url.set_fragment(None);
            "Orca · 已配对服务器"
        }
    };
    destroy_current(app).await?;
    let origin = url.origin();
    let startup_origin = origin.clone();
    let first_load = AtomicBool::new(true);
    let builder = WebviewWindowBuilder::new(app, target.label(), WebviewUrl::External(url))
        .title(title)
        .inner_size(1280.0, 840.0)
        .min_inner_size(720.0, 480.0)
        // WebKit otherwise exposes its empty white surface before document-start JS.
        .visible(false)
        .data_directory(state.paths.webview_data.clone())
        .initialization_script(include_str!("../../assets/window-init.js"))
        // Inspect the client's actual persisted identity before upstream code
        // runs. Creating a window alone is not proof that pairing was saved.
        .initialization_script(include_str!("../../assets/pairing-init.js"))
        .initialization_script(concat!(
            include_str!("../../assets/bots-i18n.js"),
            "(\n",
            include_str!("../../assets/bots-i18n.zh.json"),
            "\n);"
        ))
        .disable_drag_drop_handler()
        .devtools(cfg!(debug_assertions))
        .on_page_load(move |window, payload| {
            if payload.event() == PageLoadEvent::Finished
                && payload.url().origin() == startup_origin
                && first_load.swap(false, Ordering::Relaxed)
            {
                let _ = window.show();
                let _ = window.set_focus();
            }
        })
        .on_navigation(move |url| {
            if url.origin() == origin {
                return true;
            }
            open_external(url);
            false
        })
        .on_new_window(|url, _features| {
            open_external(&url);
            NewWindowResponse::Deny
        });
    let builder = if target == Target::Local {
        builder.initialization_script(include_str!("../../assets/tray-reveal.js"))
    } else {
        builder
    };
    let builder = if let Some(reveal) = reveal {
        builder.initialization_script(reveal)
    } else {
        builder
    };
    builder.build().context("create Orca webview window")?;
    Ok(())
}

pub async fn destroy(app: &AppHandle, state: &AppState) -> Result<()> {
    let _guard = state.window_operation.lock().await;
    destroy_current(app).await
}

async fn destroy_current(app: &AppHandle) -> Result<()> {
    if let Some(target) = current(app) {
        let label = target.label();
        let Some(window) = app.get_webview_window(label) else {
            return Ok(());
        };
        // destroy(), not hide(): drops the native window and webview controller.
        window.destroy()?;
        // Destruction is queued to the native event loop. Wait for the old label
        // to disappear before a rapid shortcut/restart can create another one.
        for _ in 0..100 {
            if app.get_webview_window(label).is_none() {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        ensure!(
            app.get_webview_window(label).is_none(),
            "window destruction is still pending"
        );
    }
    Ok(())
}

fn open_external(url: &url::Url) {
    // Keep external web pages out of the Orca origin and never pass executable,
    // file, shell, or custom schemes to the OS handler.
    if matches!(url.scheme(), "http" | "https") && url.host_str().is_some() {
        let value = url.to_string();
        std::thread::spawn(move || {
            let _ = open::that(value);
        });
    }
}
