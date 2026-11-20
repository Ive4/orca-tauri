use anyhow::{ensure, Context, Result};
use serde::Deserialize;
use std::{path::Path, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

const METADATA_LIMIT: u64 = 16 * 1024;
const RESPONSE_LIMIT: u64 = 8 * 1024 * 1024;
const REQUEST_ID: &str = "tray-tasks";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct TaskSummary {
    pub terminals: u64,
    pub unverified_terminals: u64,
    pub working: usize,
    pub monitoring: usize,
    pub waiting: usize,
    pub blocked: usize,
    pub ended: usize,
    pub unknown: usize,
    pub partial: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeMetadata {
    pid: u32,
    runtime_id: String,
    auth_token: String,
    transports: Vec<Transport>,
}

#[derive(Deserialize)]
struct Transport {
    kind: String,
    endpoint: String,
}

#[derive(Deserialize)]
struct Response {
    id: String,
    ok: bool,
    #[serde(rename = "_meta")]
    meta: ResponseMeta,
    result: Option<WorktreeListing>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResponseMeta {
    runtime_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorktreeListing {
    worktrees: Vec<Worktree>,
    total_count: usize,
    truncated: bool,
    host_scope: Option<HostScope>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HostScope {
    omitted_host_ids: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Worktree {
    live_terminal_count: u32,
    #[serde(default)]
    unverifiable_terminal_count: u32,
    agents: Vec<Agent>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Agent {
    state: AgentState,
    working_mode: Option<String>,
    #[serde(default)]
    restored_unconfirmed: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum AgentState {
    Working,
    Waiting,
    Blocked,
    Done,
    #[serde(other)]
    Unknown,
}

pub(crate) async fn read_owned(data: &Path, pid: u32) -> Result<TaskSummary> {
    tokio::time::timeout(Duration::from_secs(3), async {
        let path = data.join("orca-runtime.json");
        ensure!(
            tokio::fs::symlink_metadata(&path).await?.is_file(),
            "expected local Orca runtime metadata file"
        );
        let mut bytes = Vec::new();
        tokio::fs::File::open(path)
            .await?
            .take(METADATA_LIMIT + 1)
            .read_to_end(&mut bytes)
            .await?;
        ensure!(
            bytes.len() as u64 <= METADATA_LIMIT,
            "Orca metadata too large"
        );
        let metadata: RuntimeMetadata = serde_json::from_slice(&bytes)?;
        ensure!(
            metadata.pid == pid,
            "Orca metadata belongs to another process"
        );
        read_summary(metadata).await
    })
    .await?
}

async fn read_summary(metadata: RuntimeMetadata) -> Result<TaskSummary> {
    let mut stream = connect_owned(&metadata).await?;
    let mut request = serde_json::to_vec(&serde_json::json!({
        "id": REQUEST_ID,
        "authToken": metadata.auth_token,
        "method": "worktree.ps",
        "params": {"limit":256,"supportsWorktreeVisibilitySourceDefaults":true}
    }))?;
    request.push(b'\n');
    stream.write_all(&request).await?;
    let mut response = Vec::new();
    BufReader::new(stream.take(RESPONSE_LIMIT))
        .read_until(b'\n', &mut response)
        .await?;
    ensure!(response.ends_with(b"\n"), "incomplete Orca response");
    parse_response(&response, &metadata.runtime_id)
}

#[cfg(unix)]
async fn connect_owned(metadata: &RuntimeMetadata) -> Result<impl AsyncRead + AsyncWrite + Unpin> {
    let transport = metadata
        .transports
        .iter()
        .find(|t| t.kind == "unix")
        .context("missing Orca socket")?;
    let endpoint = Path::new(&transport.endpoint);
    ensure!(endpoint.is_absolute(), "expected a local Orca socket");
    let stream = tokio::net::UnixStream::connect(endpoint).await?;
    let peer_pid = stream
        .peer_cred()?
        .pid()
        .and_then(|pid| u32::try_from(pid).ok());
    // The metadata file can be stale. Check the actual listener before sending the token.
    ensure!(
        peer_pid == Some(metadata.pid),
        "Orca socket belongs to another process"
    );
    Ok(stream)
}

#[cfg(windows)]
async fn connect_owned(metadata: &RuntimeMetadata) -> Result<impl AsyncRead + AsyncWrite + Unpin> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::{Foundation::HANDLE, System::Pipes::GetNamedPipeServerProcessId};

    let transport = metadata
        .transports
        .iter()
        .find(|t| t.kind == "named-pipe")
        .context("missing Orca pipe")?;
    let prefix = format!(r"\\.\pipe\orca-{}-", metadata.pid);
    let suffix = transport
        .endpoint
        .strip_prefix(&prefix)
        .context("expected a local Orca pipe")?;
    ensure!(
        (1..=4).contains(&suffix.len())
            && suffix
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')),
        "invalid Orca pipe name"
    );
    // Tokio opens the client with SECURITY_IDENTIFICATION by default.
    let stream = tokio::net::windows::named_pipe::ClientOptions::new().open(&transport.endpoint)?;
    let mut peer_pid = 0;
    // The pipe handle stays open and peer_pid is writable for the call.
    unsafe { GetNamedPipeServerProcessId(HANDLE(stream.as_raw_handle()), &mut peer_pid)? };
    ensure!(
        peer_pid == metadata.pid,
        "Orca pipe belongs to another process"
    );
    Ok(stream)
}

fn parse_response(bytes: &[u8], runtime_id: &str) -> Result<TaskSummary> {
    let response: Response = serde_json::from_slice(bytes)?;
    ensure!(
        response.ok && response.id == REQUEST_ID && response.meta.runtime_id == runtime_id,
        "unexpected Orca task response"
    );
    let listing = response.result.context("missing Orca task listing")?;
    let mut summary = TaskSummary {
        partial: listing.truncated
            || listing.total_count > listing.worktrees.len()
            || listing.host_scope.is_some_and(|scope| {
                scope
                    .omitted_host_ids
                    .iter()
                    .any(|id| !id.starts_with("runtime:"))
            }),
        ..TaskSummary::default()
    };
    for worktree in listing.worktrees {
        summary.terminals += u64::from(worktree.live_terminal_count);
        summary.unverified_terminals += u64::from(worktree.unverifiable_terminal_count);
        for agent in worktree.agents {
            if agent.restored_unconfirmed {
                summary.unknown += 1;
                continue;
            }
            match agent.state {
                AgentState::Working if agent.working_mode.as_deref() == Some("monitoring") => {
                    summary.monitoring += 1;
                }
                AgentState::Working => summary.working += 1,
                AgentState::Waiting => summary.waiting += 1,
                AgentState::Blocked => summary.blocked += 1,
                AgentState::Done => summary.ended += 1,
                AgentState::Unknown => summary.unknown += 1,
            }
        }
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_runtime_reported_states_without_treating_restored_rows_as_active() {
        let response = br#"{
          "id":"tray-tasks","ok":true,"_meta":{"runtimeId":"rt_test"},
          "result":{"truncated":false,"totalCount":1,"hostScope":{"omittedHostIds":[]},
            "worktrees":[{"liveTerminalCount":2,"unverifiableTerminalCount":1,"agents":[
              {"state":"working"},
              {"state":"working","workingMode":"monitoring"},
              {"state":"waiting"},
              {"state":"blocked"},
              {"state":"done"},
              {"state":"working","restoredUnconfirmed":true},
              {"state":"future-state"}
            ]}]}
        }"#;
        let summary = parse_response(response, "rt_test").unwrap();
        assert_eq!(summary.terminals, 2);
        assert_eq!(summary.unverified_terminals, 1);
        assert_eq!(summary.working, 1);
        assert_eq!(summary.monitoring, 1);
        assert_eq!(summary.waiting, 1);
        assert_eq!(summary.blocked, 1);
        assert_eq!(summary.ended, 1);
        assert_eq!(summary.unknown, 2);
        assert!(!summary.partial);
        assert!(parse_response(response, "rt_other").is_err());
        assert!(parse_response(br#"{"id":"tray-tasks","ok":false}"#, "rt_test").is_err());
    }

    #[tokio::test]
    async fn rejects_metadata_for_a_different_core() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("orca-runtime.json"),
            r#"{"pid":42,"runtimeId":"rt_test","authToken":"token","transports":[]}"#,
        )
        .unwrap();
        assert!(read_owned(dir.path(), 43).await.is_err());
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn refuses_to_send_credentials_to_a_socket_owned_by_another_pid() {
        use tokio::net::UnixListener;

        let dir = tempfile::tempdir().unwrap();
        let endpoint = dir.path().join("runtime.sock");
        let listener = UnixListener::bind(&endpoint).unwrap();
        let pid = std::process::id() + 1;
        let metadata = serde_json::json!({
            "pid": pid, "runtimeId": "rt_test", "authToken": "test-token",
            "transports": [{"kind":"unix","endpoint":endpoint}]
        });
        std::fs::write(dir.path().join("orca-runtime.json"), metadata.to_string()).unwrap();
        assert!(read_owned(dir.path(), pid).await.is_err());
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buf = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut buf)
            .await
            .unwrap();
        assert!(buf.is_empty());
    }
}
