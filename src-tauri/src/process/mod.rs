//! The shell owns a foreground core process, never an already-running server.
//!
//! On Windows ownership is an OS Job Object assigned *during* process creation.
//! On Unix ownership is a process group; deliberately detached descendants are
//! outside that guarantee. See `unix.rs` before making cross-platform claims.

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
use unix::PlatformProcess;
#[cfg(windows)]
use windows::PlatformProcess;

use anyhow::{anyhow, bail, ensure, Context, Result};
use reqwest::{Client, Method, Url};
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    net::{IpAddr, SocketAddr, TcpListener},
    path::{Path, PathBuf},
    sync::{mpsc, Mutex, MutexGuard},
    time::Duration,
};
use tokio::time::{sleep, Instant};

// A shell launched from Electron or another Node host must not accidentally
// reuse that host's executable or preload hooks. Explicit launch overrides are
// applied afterwards, so an intentional user NODE_OPTIONS remains supported.
pub(super) const HOST_ENV_BLOCKLIST: [&str; 3] = [
    "ELECTRON_RUN_AS_NODE",
    "ORCA_APP_EXECUTABLE",
    "NODE_OPTIONS",
];

/// `Http` is intended for the mock core/test harness. Production Orca packages
/// must use `OrcaJson`: a HTTP 200 alone does not establish process identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadinessMode {
    OrcaJson,
    Http,
}

#[derive(Debug)]
pub(crate) struct PortOccupied(u16);

impl std::fmt::Display for PortOccupied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Port {} is occupied; no existing process was adopted",
            self.0
        )
    }
}

impl std::error::Error for PortOccupied {}

#[derive(Clone)]
pub struct ShutdownRequest {
    pub url: String,
    pub method: String,
    pub headers: BTreeMap<String, String>,
}

/// Intentionally not `Debug`/`Serialize`: environment variables, arguments and
/// authenticated shutdown headers can contain user secrets.
#[derive(Clone)]
pub struct LaunchSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
    pub health_url: String,
    pub port: u16,
    pub startup_timeout: Duration,
    pub shutdown_timeout: Duration,
    pub shutdown: Option<ShutdownRequest>,
    pub log_dir: PathBuf,
    pub version: String,
    pub readiness: ReadinessMode,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CorePhase {
    Stopped,
    Starting,
    Running,
    Stopping,
    Failed,
}

#[derive(Clone, Debug, Serialize)]
pub struct CoreStatus {
    pub phase: CorePhase,
    pub pid: Option<u32>,
    pub version: Option<String>,
    pub detail: Option<String>,
}

struct Inner {
    process: Option<PlatformProcess>,
    spec: Option<LaunchSpec>,
    readiness_rx: Option<mpsc::Receiver<Value>>,
    status: CoreStatus,
    // Pairing offers are credentials. Keep this out of serializable status/logs.
    web_client_url: Option<String>,
}

/// All async lifecycle operations are serialized. The small synchronous mutex
/// keeps the OS ownership handle accessible to Tauri's final Exit callback.
pub struct CoreManager {
    operation: tokio::sync::Mutex<()>,
    inner: Mutex<Inner>,
    client: Client,
}

impl CoreManager {
    pub fn new() -> Result<Self> {
        Ok(Self {
            operation: tokio::sync::Mutex::new(()),
            inner: Mutex::new(Inner {
                process: None,
                spec: None,
                readiness_rx: None,
                status: CoreStatus {
                    phase: CorePhase::Stopped,
                    pid: None,
                    version: None,
                    detail: None,
                },
                web_client_url: None,
            }),
            // Local readiness must never pass through a configured system proxy.
            client: Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(1))
                .timeout(Duration::from_secs(2))
                .build()?,
        })
    }

    fn inner(&self) -> MutexGuard<'_, Inner> {
        // A panic must not prevent closing the Job Object on the exit path.
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    pub fn status(&self) -> CoreStatus {
        let mut inner = self.inner();
        if matches!(inner.status.phase, CorePhase::Starting | CorePhase::Running) {
            let exited = inner.process.as_mut().map(PlatformProcess::try_wait);
            let failure = match exited {
                Some(Ok(Some(exit))) => Some(format!("Core exited: {exit}")),
                Some(Err(_)) => Some("Could not query core process state".to_owned()),
                _ => None,
            };
            if let Some(detail) = failure {
                // The root exiting does not imply that its descendants exited.
                if let Some(process) = inner.process.as_mut() {
                    let _ = process.kill_tree();
                }
                inner.readiness_rx = None;
                inner.web_client_url = None;
                inner.status.phase = CorePhase::Failed;
                inner.status.pid = None;
                inner.status.detail = Some(detail);
            }
        }
        inner.status.clone()
    }

    /// Returns a secret-bearing URL for a native WebView navigation only.
    /// Never emit it to frontend IPC, notifications, telemetry or log output.
    pub fn web_client_offer(&self) -> Option<(String, u16)> {
        if self.status().phase != CorePhase::Running {
            return None;
        }
        let inner = self.inner();
        Some((inner.web_client_url.clone()?, inner.spec.as_ref()?.port))
    }

    pub async fn start(&self, spec: LaunchSpec) -> Result<CoreStatus> {
        let _operation = self.operation.lock().await;
        self.start_locked(spec).await
    }

    pub async fn restart(&self, spec: LaunchSpec) -> Result<CoreStatus> {
        let _operation = self.operation.lock().await;
        self.stop_locked().await?;
        self.start_locked(spec).await
    }

    pub async fn stop(&self) -> Result<()> {
        let _operation = self.operation.lock().await;
        self.stop_locked().await
    }

    async fn start_locked(&self, spec: LaunchSpec) -> Result<CoreStatus> {
        if self.status().phase == CorePhase::Failed {
            self.reap_failed_tree().await?;
        }
        let result = self.start_attempt(spec).await;
        if result.is_err() && self.status().phase == CorePhase::Failed {
            if let Err(cleanup_error) = self.reap_failed_tree().await {
                return Err(result.err().unwrap()).context(format!(
                    "Core startup failed and its tree is still terminating: {cleanup_error}"
                ));
            }
        }
        result
    }

    /// Failed/cancelled startup retains its ownership handle until all owned
    /// processes have exited. In particular rollback cannot race a candidate
    /// still releasing the same TCP port after TerminateJobObject returned.
    async fn reap_failed_tree(&self) -> Result<()> {
        {
            let mut inner = self.inner();
            if let Some(process) = inner.process.as_mut() {
                process.kill_tree()?;
            } else {
                return Ok(());
            }
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let empty = self
                .inner()
                .process
                .as_mut()
                .map(PlatformProcess::tree_empty)
                .transpose()?
                .unwrap_or(true);
            if empty {
                let mut inner = self.inner();
                inner.process = None;
                inner.readiness_rx = None;
                inner.web_client_url = None;
                return Ok(());
            }
            ensure!(
                Instant::now() < deadline,
                "Previous core tree has not exited; restart is blocked"
            );
            sleep(Duration::from_millis(25)).await;
        }
    }

    fn abort_pending(&self, detail: &'static str) {
        let mut inner = self.inner();
        if let Some(process) = inner.process.as_mut() {
            let _ = process.kill_tree();
        }
        inner.readiness_rx = None;
        inner.web_client_url = None;
        inner.status.phase = CorePhase::Failed;
        inner.status.pid = None;
        inner.status.detail = Some(detail.to_owned());
    }

    async fn start_attempt(&self, spec: LaunchSpec) -> Result<CoreStatus> {
        let current = self.status();
        if current.phase == CorePhase::Running {
            let inner = self.inner();
            let same = inner.spec.as_ref().is_some_and(|old| {
                old.version == spec.version
                    && old.program == spec.program
                    && old.health_url == spec.health_url
            });
            ensure!(same, "A different core is already running; stop it first");
            return Ok(current);
        }
        validate_spec(&spec)?;
        // Refuse to attach to a service that we did not launch. Releasing this
        // socket before spawn is necessarily a TOCTOU boundary; the owned
        // stdout readiness record additionally proves PID/version/bind identity.
        let url = Url::parse(&spec.health_url)?;
        let address: IpAddr = url
            .host_str()
            .unwrap_or_default()
            .trim_matches(['[', ']'])
            .parse()?;
        let probe = TcpListener::bind(SocketAddr::new(address, spec.port)).map_err(|error| {
            if error.kind() == std::io::ErrorKind::AddrInUse {
                anyhow::Error::new(PortOccupied(spec.port))
            } else {
                anyhow::Error::new(error).context(format!("Cannot bind core port {}", spec.port))
            }
        })?;
        drop(probe);

        {
            let mut inner = self.inner();
            inner.status = CoreStatus {
                phase: CorePhase::Starting,
                pid: None,
                version: Some(spec.version.clone()),
                detail: None,
            };
            inner.web_client_url = None;
            inner.spec = Some(spec.clone());
        }
        let mut guard = StartupGuard {
            manager: self,
            armed: true,
        };
        let process = PlatformProcess::spawn(&spec).context("Could not launch the owned core")?;
        let pid = process.id();
        {
            let mut inner = self.inner();
            inner.process = Some(process);
            inner.status.pid = Some(pid);
        }
        let reader = self
            .inner()
            .process
            .as_mut()
            .and_then(PlatformProcess::take_stdout)
            .context("Core stdout pipe was not created")?;
        let stderr_reader = self
            .inner()
            .process
            .as_mut()
            .and_then(PlatformProcess::take_stderr)
            .context("Core stderr pipe was not created")?;
        let (tx, rx) = mpsc::sync_channel(1);
        let stdout_log = open_log(&spec.log_dir, "core.stdout.log")?;
        std::thread::Builder::new()
            .name("orca-core-stdout".to_owned())
            .spawn(move || read_stdout(reader, stdout_log, tx))
            .context("Could not start the core stdout reader")?;
        let stderr_log = open_log(&spec.log_dir, "core.stderr.log")?;
        std::thread::Builder::new()
            .name("orca-core-stderr".to_owned())
            .spawn(move || read_stderr(stderr_reader, stderr_log))
            .context("Could not start the core stderr reader")?;
        {
            let mut inner = self.inner();
            inner.readiness_rx = Some(rx);
        }

        let deadline = Instant::now() + spec.startup_timeout;
        let mut ready = spec.readiness == ReadinessMode::Http;
        let mut ready_url = None;
        let mut ready_detail = None;
        while Instant::now() < deadline {
            ensure!(
                self.status().phase == CorePhase::Starting,
                "Core exited before becoming ready"
            );
            if !ready {
                let event = self
                    .inner()
                    .readiness_rx
                    .as_ref()
                    .and_then(|rx| rx.try_recv().ok());
                if let Some(event) = event {
                    let verified = validate_readiness(&event, &spec, pid)?;
                    ready_url = verified.web_client_url;
                    ready_detail = verified.detail;
                    ready = true;
                }
            }
            if ready {
                let response =
                    tokio::time::timeout_at(deadline, self.client.get(&spec.health_url).send())
                        .await;
                if let Ok(Ok(response)) = response {
                    if response.status().is_success() {
                        ensure!(
                            self.status().phase == CorePhase::Starting,
                            "Core exited during readiness verification"
                        );
                        let mut inner = self.inner();
                        inner.status.phase = CorePhase::Running;
                        inner.status.detail = ready_detail;
                        inner.web_client_url = ready_url.or_else(|| {
                            (spec.readiness == ReadinessMode::Http).then(|| spec.health_url.clone())
                        });
                        let status = inner.status.clone();
                        guard.armed = false;
                        return Ok(status);
                    }
                }
            }
            sleep(Duration::from_millis(100)).await;
        }
        bail!("Core readiness timed out; check core.stderr.log and the pinned packaging contract")
    }

    async fn stop_locked(&self) -> Result<()> {
        let spec = {
            let mut inner = self.inner();
            if inner.process.is_none() {
                inner.status.phase = CorePhase::Stopped;
                inner.status.pid = None;
                inner.status.detail = None;
                inner.web_client_url = None;
                return Ok(());
            }
            inner.status.phase = CorePhase::Stopping;
            inner.web_client_url = None;
            inner
                .spec
                .clone()
                .context("Running core has no launch specification")?
        };
        // Cancellation or a failing graceful protocol must not strand the tree.
        let mut cleanup = StopGuard {
            manager: self,
            armed: true,
        };
        let mut graceful = false;
        if let Some(request) = &spec.shutdown {
            let method = Method::from_bytes(request.method.as_bytes())?;
            let mut builder = self.client.request(method, &request.url);
            for (name, value) in &request.headers {
                builder = builder.header(name, value);
            }
            graceful = builder
                .send()
                .await
                .is_ok_and(|response| response.status().is_success());
        }
        if !graceful {
            graceful = self
                .inner()
                .process
                .as_mut()
                .map(PlatformProcess::request_graceful_stop)
                .transpose()?
                .unwrap_or(false);
        }
        if graceful {
            let deadline = Instant::now() + spec.shutdown_timeout;
            while Instant::now() < deadline {
                let exited = self
                    .inner()
                    .process
                    .as_mut()
                    .map(PlatformProcess::try_wait)
                    .transpose()?
                    .flatten()
                    .is_some();
                if exited {
                    break;
                }
                sleep(Duration::from_millis(100)).await;
            }
        }
        // Always reap the whole owned tree, even when orcad handled its stop.
        {
            let mut inner = self.inner();
            if let Some(process) = inner.process.as_mut() {
                process.kill_tree()?;
            }
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let empty = self
                .inner()
                .process
                .as_mut()
                .map(PlatformProcess::tree_empty)
                .transpose()?
                .unwrap_or(true);
            if empty {
                break;
            }
            if Instant::now() >= deadline {
                bail!("Core process tree did not finish terminating within five seconds");
            }
            sleep(Duration::from_millis(25)).await;
        }
        self.emergency_stop_sync();
        cleanup.armed = false;
        Ok(())
    }

    /// Synchronous final fallback, safe to call repeatedly from Exit/Drop only.
    /// It does not wait for the OS to finish terminating. Use `stop().await` for
    /// restart/update paths, which must retain ownership until the port is free.
    /// Windows additionally enforces cleanup if this code never runs at all.
    pub fn emergency_stop_sync(&self) {
        let mut inner = self.inner();
        if let Some(mut process) = inner.process.take() {
            let _ = process.kill_tree();
        }
        inner.readiness_rx = None;
        inner.web_client_url = None;
        inner.status.phase = CorePhase::Stopped;
        inner.status.pid = None;
        inner.status.detail = None;
    }
}

impl Drop for CoreManager {
    fn drop(&mut self) {
        self.emergency_stop_sync();
    }
}

struct StartupGuard<'a> {
    manager: &'a CoreManager,
    armed: bool,
}

impl Drop for StartupGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.manager.abort_pending(
                "Core startup failed or was cancelled; inspect the returned error and core logs",
            );
        }
    }
}

struct StopGuard<'a> {
    manager: &'a CoreManager,
    armed: bool,
}
impl Drop for StopGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.manager
                .abort_pending("Core stop was interrupted; the owned tree is terminating");
        }
    }
}

fn validate_spec(spec: &LaunchSpec) -> Result<()> {
    ensure!(
        spec.program.is_absolute() && spec.program.is_file(),
        "Core program must be an existing absolute file path"
    );
    ensure!(
        spec.cwd.is_absolute() && spec.cwd.is_dir(),
        "Core working directory must exist and be absolute"
    );
    ensure!(spec.port != 0, "Core port must be nonzero");
    ensure!(
        !spec.startup_timeout.is_zero(),
        "Core startup timeout must be positive"
    );
    validate_loopback_url(&spec.health_url, "http", spec.port)?;
    if let Some(request) = &spec.shutdown {
        validate_loopback_url(&request.url, "http", spec.port)?;
        Method::from_bytes(request.method.as_bytes()).context("Invalid shutdown HTTP method")?;
    }
    for (key, value) in &spec.env {
        ensure!(
            !key.is_empty() && !key.contains(['=', '\0']) && !value.contains('\0'),
            "Invalid environment entry"
        );
    }
    ensure!(
        spec.args.iter().all(|arg| !arg.contains('\0')),
        "Core arguments cannot contain NUL"
    );
    Ok(())
}

fn validate_loopback_url(value: &str, scheme: &str, port: u16) -> Result<Url> {
    // Error messages intentionally do not echo a possibly secret-bearing URL.
    let url = Url::parse(value).map_err(|_| anyhow!("Invalid core URL"))?;
    ensure!(
        url.scheme() == scheme,
        "Core URL uses an unexpected transport"
    );
    let host = url.host_str().unwrap_or_default().trim_matches(['[', ']']);
    ensure!(
        host == "127.0.0.1" || host == "::1",
        "Core URL must use a literal loopback address"
    );
    ensure!(
        url.port_or_known_default() == Some(port),
        "Core URL port does not match the configured port"
    );
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "Core URL cannot contain user credentials"
    );
    Ok(url)
}

struct VerifiedReadiness {
    web_client_url: Option<String>,
    detail: Option<String>,
}

fn validate_readiness(event: &Value, spec: &LaunchSpec, pid: u32) -> Result<VerifiedReadiness> {
    ensure!(
        event.get("type").and_then(Value::as_str) == Some("orca_server_ready"),
        "Unexpected core readiness event"
    );
    ensure!(
        event.get("schemaVersion").and_then(Value::as_u64) == Some(1),
        "Unsupported Orca readiness schema"
    );
    let endpoint = event
        .get("boundEndpoint")
        .and_then(Value::as_str)
        .context("Orca did not publish a bound WebSocket endpoint")?;
    validate_loopback_url(endpoint, "ws", spec.port)?;
    let health = event
        .get("health")
        .context("Orca readiness omitted health diagnostics")?;
    let version = health
        .get("buildVersion")
        .and_then(Value::as_str)
        .context("Orca readiness omitted its build version")?;
    ensure!(
        version.trim_start_matches('v') == spec.version.trim_start_matches('v'),
        "Running Orca version does not match the selected package"
    );
    ensure!(
        health.get("pid").and_then(Value::as_u64) == Some(u64::from(pid)),
        "Orca health PID does not match the owned process"
    );
    ensure!(
        health
            .pointer("/terminalDaemon/selfTest/ok")
            .and_then(Value::as_bool)
            == Some(true),
        "Orca terminal daemon self-test failed or was not reported"
    );
    let pairing = event
        .get("pairing")
        .context("Orca readiness omitted pairing availability")?;
    if pairing.get("available").and_then(Value::as_bool) == Some(true) {
        let url = pairing
            .get("webClientUrl")
            .and_then(Value::as_str)
            .context("Orca did not provide a paired Web client URL")?;
        validate_loopback_url(url, "http", spec.port)?;
        Ok(VerifiedReadiness {
            web_client_url: Some(url.to_owned()),
            detail: None,
        })
    } else {
        ensure!(
            pairing.get("available").and_then(Value::as_bool) == Some(false),
            "Orca pairing availability is invalid"
        );
        Ok(VerifiedReadiness {
            web_client_url: None,
            detail: Some("Core is ready, but pairing is unavailable. Review the core pairing configuration before opening a window.".to_owned()),
        })
    }
}

pub(super) fn open_log(directory: &Path, name: &str) -> Result<File> {
    fs::create_dir_all(directory).context("Could not create the core log directory")?;
    let path = directory.join(name);
    if fs::metadata(&path).is_ok_and(|metadata| metadata.len() > 10 * 1024 * 1024) {
        let previous = directory.join(format!("{name}.previous"));
        if previous.exists() {
            fs::remove_file(&previous)?;
        }
        fs::rename(&path, previous)?;
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .context("Could not open core log")
}

fn read_stdout(mut reader: os_pipe::PipeReader, mut log: File, sender: mpsc::SyncSender<Value>) {
    let mut chunk = [0u8; 8192];
    let mut line = Vec::new();
    let mut overflow = false;
    let mut sent = false;
    loop {
        let count = match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        for byte in &chunk[..count] {
            if *byte == b'\n' {
                if !overflow && !sent {
                    if let Ok(event) = serde_json::from_slice::<Value>(&line) {
                        if event.get("type").and_then(Value::as_str) == Some("orca_server_ready") {
                            sent = true;
                            let _ = writeln!(
                                log,
                                "Core readiness received; pairing credentials omitted."
                            );
                            if sender.send(event).is_err() {
                                return;
                            }
                        }
                    }
                }
                // Suppress raw stdout, including malformed JSON or a future
                // human-mode pairing URL. Diagnostics remain in stderr.
                line.clear();
                overflow = false;
            } else if !overflow {
                if line.len() == 1024 * 1024 {
                    overflow = true;
                    line.clear();
                    let _ = writeln!(log, "Oversized core stdout record omitted.");
                } else {
                    line.push(*byte);
                }
            }
        }
    }
}

fn redact_stderr_line(line: &[u8]) -> String {
    let text = String::from_utf8_lossy(line);
    let lowercase = text.to_ascii_lowercase();
    // Conservatively omit the whole diagnostic if it names credentials. This
    // also catches the standard Orca pairing offer/scheme. This is not a claim
    // to recognize every third-party CLI's arbitrary secret-printing format.
    if [
        "pairing",
        "offer",
        "orca://",
        "token",
        "secret",
        "password",
        "authorization",
        "api_key",
        "apikey",
        "credential",
    ]
    .iter()
    .any(|marker| lowercase.contains(marker))
    {
        return "[sensitive core diagnostic omitted]".to_owned();
    }
    let mut remaining = text.as_ref();
    let mut output = String::new();
    loop {
        let next = ["https://", "http://", "wss://", "ws://"]
            .iter()
            .filter_map(|prefix| remaining.find(prefix))
            .min();
        let Some(start) = next else {
            output.push_str(remaining);
            break;
        };
        output.push_str(&remaining[..start]);
        let candidate = &remaining[start..];
        let end = candidate
            .find(|ch: char| ch.is_whitespace() || matches!(ch, '"' | '\'' | '<' | '>'))
            .unwrap_or(candidate.len());
        let candidate = &candidate[..end];
        if let Ok(mut url) = Url::parse(candidate) {
            let _ = url.set_username("");
            let _ = url.set_password(None);
            url.set_query(None);
            url.set_fragment(None);
            output.push_str(url.as_str());
        } else {
            output.push_str(
                candidate
                    .split(['?', '#'])
                    .next()
                    .unwrap_or("[URL omitted]"),
            );
        }
        remaining = &remaining[start + end..];
    }
    output
}

fn read_stderr(mut reader: os_pipe::PipeReader, mut log: File) {
    let mut chunk = [0u8; 8192];
    let mut line = Vec::new();
    let mut overflow = false;
    loop {
        let count = match reader.read(&mut chunk) {
            Ok(0) => {
                if !line.is_empty() && !overflow {
                    let _ = writeln!(log, "{}", redact_stderr_line(&line));
                }
                return;
            }
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        };
        for byte in &chunk[..count] {
            if *byte == b'\n' {
                if !overflow {
                    let _ = writeln!(log, "{}", redact_stderr_line(&line));
                }
                line.clear();
                overflow = false;
            } else if !overflow {
                if line.len() == 64 * 1024 {
                    overflow = true;
                    line.clear();
                    let _ = writeln!(log, "[oversized core diagnostic omitted]");
                } else {
                    line.push(*byte);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> LaunchSpec {
        LaunchSpec {
            program: PathBuf::new(),
            args: vec![],
            cwd: PathBuf::new(),
            env: BTreeMap::new(),
            health_url: "http://127.0.0.1:9876/web-index.html".to_owned(),
            port: 9876,
            startup_timeout: Duration::from_secs(2),
            shutdown_timeout: Duration::from_secs(1),
            shutdown: None,
            log_dir: PathBuf::new(),
            version: "v1.4.264".to_owned(),
            readiness: ReadinessMode::OrcaJson,
        }
    }

    fn readiness() -> Value {
        serde_json::json!({
            "type": "orca_server_ready", "schemaVersion": 1,
            "boundEndpoint": "ws://127.0.0.1:9876", "runtimeId": "test-runtime",
            "health": {"pid": 42, "buildVersion": "1.4.264", "terminalDaemon": {"selfTest": {"ok": true}}},
            "pairing": {"available": true, "webClientUrl": "http://127.0.0.1:9876/web-index.html#pairing=SECRET"}
        })
    }

    #[test]
    fn rejects_remote_redirects_and_wrong_versions_without_leaking_pairing() {
        let config = spec();
        assert!(validate_readiness(&readiness(), &config, 42).is_ok());
        for bad_url in [
            "https://127.0.0.1:9876/",
            "http://example.com:9876/",
            "http://127.0.0.1:1234/",
        ] {
            let mut value = readiness();
            value["pairing"]["webClientUrl"] = Value::String(format!("{bad_url}#SECRET"));
            let error = validate_readiness(&value, &config, 42)
                .err()
                .unwrap()
                .to_string();
            assert!(!error.contains("SECRET"));
        }
        let mut value = readiness();
        value["health"]["buildVersion"] = "1.4.263".into();
        assert!(validate_readiness(&value, &config, 42).is_err());
        assert!(validate_readiness(&readiness(), &config, 7).is_err());
    }

    #[test]
    fn absence_of_terminal_self_test_is_not_health() {
        let mut value = readiness();
        value["health"]["terminalDaemon"] = serde_json::json!({});
        assert!(validate_readiness(&value, &spec(), 42).is_err());
    }

    #[test]
    fn unavailable_pairing_never_fabricates_a_url() {
        let mut value = readiness();
        value["pairing"] =
            serde_json::json!({"available": false, "reason": "disabled_by_operator"});
        let result = validate_readiness(&value, &spec(), 42).unwrap();
        assert!(result.web_client_url.is_none());
        assert!(result.detail.is_some());
    }

    #[test]
    fn stderr_redacts_pairing_and_url_credentials() {
        assert_eq!(
            redact_stderr_line(b"pairing offer: TEST_VALUE"),
            "[sensitive core diagnostic omitted]"
        );
        let redacted =
            redact_stderr_line(b"failed at https://user:pass@example.test/path?code=ABC#DEF next");
        assert_eq!(redacted, "failed at https://example.test/path next");
        assert_eq!(
            redact_stderr_line(b"PTY executable could not be found"),
            "PTY executable could not be found"
        );
    }

    // These fixtures release an ephemeral reservation before the real child
    // binds. Serialize only lifecycle tests so their reservations cannot race
    // each other's spawns; production lifecycle synchronization is unchanged.
    static FIXTURE_GATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn test_directory() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("orca-process-test-")
            .tempdir()
            .unwrap()
    }

    fn available_port() -> u16 {
        TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    fn fixture_spec(directory: &Path, port: u16) -> LaunchSpec {
        LaunchSpec {
            program: std::env::current_exe().unwrap(),
            args: vec![
                "--ignored".into(),
                "--exact".into(),
                "process::tests::mock_core_process".into(),
                "--nocapture".into(),
            ],
            cwd: directory.to_path_buf(),
            env: BTreeMap::from([
                ("ORCA_TEST_ROLE".into(), "core".into()),
                (
                    "ORCA_TEST_DIRECTORY".into(),
                    directory.to_string_lossy().into_owned(),
                ),
                ("ORCA_TEST_PORT".into(), port.to_string()),
            ]),
            health_url: format!("http://127.0.0.1:{port}/web-index.html"),
            port,
            startup_timeout: Duration::from_secs(10),
            shutdown_timeout: Duration::from_millis(200),
            shutdown: None,
            log_dir: directory.to_path_buf(),
            version: "v1.4.264".into(),
            readiness: ReadinessMode::OrcaJson,
        }
    }

    /// A real child process running from the test binary. It deliberately starts
    /// a grandchild so cleanup tests cover inherited jobs/groups, not just PID 1.
    #[test]
    #[ignore = "subprocess fixture, invoked only by lifecycle tests"]
    // This fixture deliberately never waits: its owner must kill the entire
    // job/group, including this live descendant, for the regression to pass.
    #[allow(clippy::zombie_processes)]
    fn mock_core_process() {
        let Ok(role) = std::env::var("ORCA_TEST_ROLE") else {
            return;
        };
        if role == "leaf" {
            loop {
                std::thread::sleep(Duration::from_secs(1));
            }
        }
        let directory = PathBuf::from(std::env::var_os("ORCA_TEST_DIRECTORY").unwrap());
        let port: u16 = std::env::var("ORCA_TEST_PORT").unwrap().parse().unwrap();
        if role == "owner" {
            let mut process = PlatformProcess::spawn(&fixture_spec(&directory, port)).unwrap();
            loop {
                assert!(process.try_wait().unwrap().is_none());
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        assert_eq!(role, "core");
        let server = TcpListener::bind(("127.0.0.1", port)).unwrap();
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "process::tests::mock_core_process",
                "--nocapture",
            ])
            .env("ORCA_TEST_ROLE", "leaf")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        fs::write(
            directory.join("pids.json"),
            serde_json::to_vec(&vec![std::process::id(), child.id()]).unwrap(),
        )
        .unwrap();
        let healthy = std::env::var_os("ORCA_TEST_FAIL_HEALTH").is_none();
        println!(
            "\n{}",
            serde_json::json!({
                "type":"orca_server_ready", "schemaVersion":1, "runtimeId":"fixture",
                "boundEndpoint":format!("ws://127.0.0.1:{port}"),
                "health":{"pid":std::process::id(),"buildVersion":"1.4.264","terminalDaemon":{"selfTest":{"ok":healthy}}},
                "pairing":{"available":true,"webClientUrl":format!("http://127.0.0.1:{port}/web-index.html#pairing=TEST_SECRET_MUST_NOT_APPEAR_IN_LOGS")}
            })
        );
        std::io::stdout().flush().unwrap();
        for stream in server.incoming() {
            let mut stream = stream.unwrap();
            let mut request = [0u8; 1024];
            let _ = stream.read(&mut request);
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
        }
    }

    fn process_is_running(pid: u32) -> bool {
        #[cfg(windows)]
        {
            use ::windows::Win32::{
                Foundation::CloseHandle,
                System::Threading::{
                    GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
                },
            };
            let Ok(handle) =
                (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) })
            else {
                return false;
            };
            let mut code = 0;
            let ok = unsafe { GetExitCodeProcess(handle, &mut code) }.is_ok();
            let _ = unsafe { CloseHandle(handle) };
            ok && code == 259
        }
        #[cfg(unix)]
        {
            #[cfg(target_os = "linux")]
            if let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) {
                // In a container PID 1 can retain orphan zombies. Zombies have
                // no running code or address space and are not surviving cores.
                if stat
                    .rsplit_once(") ")
                    .is_some_and(|(_, fields)| fields.starts_with('Z') || fields.starts_with('X'))
                {
                    return false;
                }
            }
            unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
        }
    }

    async fn assert_processes_gone(pids: &[u32]) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while pids.iter().any(|pid| process_is_running(*pid)) && Instant::now() < deadline {
            sleep(Duration::from_millis(25)).await;
        }
        assert!(
            pids.iter().all(|pid| !process_is_running(*pid)),
            "a core descendant survived cleanup"
        );
    }

    #[tokio::test]
    async fn stop_reaps_real_grandchild_and_pairing_is_never_logged() {
        let _fixture = FIXTURE_GATE.lock().await;
        let directory = test_directory();
        let manager = CoreManager::new().unwrap();
        let status = manager
            .start(fixture_spec(directory.path(), available_port()))
            .await
            .unwrap();
        assert_eq!(status.phase, CorePhase::Running);
        assert!(manager
            .web_client_offer()
            .unwrap()
            .0
            .contains("TEST_SECRET_MUST_NOT_APPEAR_IN_LOGS"));
        let pids: Vec<u32> =
            serde_json::from_slice(&fs::read(directory.path().join("pids.json")).unwrap()).unwrap();
        assert_eq!(pids.len(), 2);
        assert!(pids.iter().all(|pid| process_is_running(*pid)));
        manager.stop().await.unwrap();
        assert_processes_gone(&pids).await;
        assert_eq!(manager.status().phase, CorePhase::Stopped);
        assert!(manager.web_client_offer().is_none());
        for filename in ["core.stdout.log", "core.stderr.log"] {
            let log = fs::read_to_string(directory.path().join(filename)).unwrap();
            assert!(!log.contains("TEST_SECRET_MUST_NOT_APPEAR_IN_LOGS"));
        }
    }

    #[tokio::test]
    async fn failed_readiness_kills_the_entire_owned_tree() {
        let _fixture = FIXTURE_GATE.lock().await;
        let directory = test_directory();
        let manager = CoreManager::new().unwrap();
        let mut spec = fixture_spec(directory.path(), available_port());
        spec.env.insert("ORCA_TEST_FAIL_HEALTH".into(), "1".into());
        let error = manager
            .start(spec)
            .await
            .expect_err("fixture must fail its PTY self-test");
        assert_eq!(
            error.to_string(),
            "Orca terminal daemon self-test failed or was not reported",
            "unexpected fixture startup error: {error:#}"
        );
        let pids: Vec<u32> =
            serde_json::from_slice(&fs::read(directory.path().join("pids.json")).unwrap()).unwrap();
        assert_processes_gone(&pids).await;
        assert_eq!(manager.status().phase, CorePhase::Failed);
    }

    #[tokio::test]
    async fn failed_candidate_can_be_replaced_on_the_same_port_immediately() {
        let _fixture = FIXTURE_GATE.lock().await;
        let directory = test_directory();
        let manager = CoreManager::new().unwrap();
        let port = available_port();
        let mut bad = fixture_spec(directory.path(), port);
        bad.env.insert("ORCA_TEST_FAIL_HEALTH".into(), "1".into());
        let error = manager
            .start(bad)
            .await
            .expect_err("candidate must fail its PTY self-test");
        assert_eq!(
            error.to_string(),
            "Orca terminal daemon self-test failed or was not reported",
            "unexpected candidate startup error: {error:#}"
        );
        // No sleep or retry in the caller: the failed start itself must finish
        // tree cleanup before rollback attempts to bind its configured port.
        let status = manager
            .start(fixture_spec(directory.path(), port))
            .await
            .unwrap();
        assert_eq!(status.phase, CorePhase::Running);
        manager.stop().await.unwrap();
    }

    #[tokio::test]
    async fn occupied_port_is_not_adopted() {
        let _fixture = FIXTURE_GATE.lock().await;
        let directory = test_directory();
        let manager = CoreManager::new().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let spec = fixture_spec(directory.path(), listener.local_addr().unwrap().port());
        assert!(manager.start(spec).await.unwrap_err().is::<PortOccupied>());
        assert!(!directory.path().join("pids.json").exists());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn killing_the_owner_closes_job_and_kills_root_and_grandchild() {
        let _fixture = FIXTURE_GATE.lock().await;
        struct ChildGuard(std::process::Child);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let directory = test_directory();
        let mut owner = ChildGuard(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--ignored",
                    "--exact",
                    "process::tests::mock_core_process",
                    "--nocapture",
                ])
                .env("ORCA_TEST_ROLE", "owner")
                .env("ORCA_TEST_DIRECTORY", directory.path())
                .env("ORCA_TEST_PORT", available_port().to_string())
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
        let path = directory.path().join("pids.json");
        let deadline = Instant::now() + Duration::from_secs(10);
        let pids: Vec<u32> = loop {
            if let Ok(bytes) = fs::read(&path) {
                if let Ok(pids) = serde_json::from_slice(&bytes) {
                    break pids;
                }
            }
            assert!(Instant::now() < deadline, "owner fixture did not start");
            sleep(Duration::from_millis(25)).await;
        };
        // No graceful Rust/Tauri destructor runs in the owner. The OS closes
        // its last Job handle, proving that the core did not inherit that handle.
        owner.0.kill().unwrap();
        owner.0.wait().unwrap();
        assert_processes_gone(&pids).await;
    }
}
