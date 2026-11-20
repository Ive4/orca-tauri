use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{path::Path, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

const METADATA_LIMIT: u64 = 16 * 1024;
const RESPONSE_LIMIT: u64 = 8 * 1024 * 1024;
const REQUEST_ID: &str = "tray-tasks";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub(crate) struct TaskSummary {
    pub rows: Vec<TaskRow>,
    pub partial: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskRow {
    pub id: String,
    pub title: String,
    pub workspace: String,
    pub provider: Option<String>,
    pub status: TaskStatus,
    pub started_at: Option<u64>,
    pub updated_at: Option<u64>,
    #[serde(skip)]
    pub target: TaskTarget,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskTarget {
    pub worktree_id: String,
    pub tab_id: String,
    pub leaf_id: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TaskStatus {
    Waiting,
    Blocked,
    Working,
    Monitoring,
    Unknown,
    Done,
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
    #[serde(default)]
    worktree_id: String,
    display_name: Option<String>,
    host_id: Option<String>,
    agents: Vec<Agent>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Agent {
    #[serde(default)]
    pane_key: String,
    state: AgentState,
    task_title: Option<String>,
    conversation_name: Option<String>,
    display_name: Option<String>,
    agent_type: Option<String>,
    state_started_at: Option<u64>,
    updated_at: Option<u64>,
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
        let workspace = label(worktree.display_name.as_deref()).unwrap_or_else(|| "工作区".into());
        for agent in worktree.agents {
            let status = if agent.restored_unconfirmed {
                TaskStatus::Unknown
            } else {
                match agent.state {
                    AgentState::Working if agent.working_mode.as_deref() == Some("monitoring") => {
                        TaskStatus::Monitoring
                    }
                    AgentState::Working => TaskStatus::Working,
                    AgentState::Waiting => TaskStatus::Waiting,
                    AgentState::Blocked => TaskStatus::Blocked,
                    AgentState::Done => TaskStatus::Done,
                    AgentState::Unknown => TaskStatus::Unknown,
                }
            };
            let title = [
                agent.task_title,
                agent.conversation_name,
                agent.display_name,
            ]
            .iter()
            .find_map(|candidate| label(candidate.as_deref()))
            .unwrap_or_else(|| "未命名任务".into());
            let (tab_id, leaf_id) = agent
                .pane_key
                .rsplit_once(':')
                .map_or((agent.pane_key.as_str(), None), |(tab, leaf)| {
                    (tab, Some(leaf.to_owned()))
                });
            let key =
                serde_json::to_vec(&(&worktree.worktree_id, &worktree.host_id, &agent.pane_key))?;
            let id = hex::encode(Sha256::digest(key));
            summary.rows.push(TaskRow {
                id,
                title,
                workspace: workspace.clone(),
                provider: label(agent.agent_type.as_deref()),
                status,
                started_at: agent.state_started_at,
                updated_at: agent.updated_at,
                target: TaskTarget {
                    worktree_id: worktree.worktree_id.clone(),
                    tab_id: tab_id.to_owned(),
                    leaf_id,
                },
            });
        }
    }
    // Within each group, order by the start of the state, not ongoing output.
    summary.rows.sort_by_key(|row| {
        let group = match row.status {
            TaskStatus::Waiting | TaskStatus::Blocked => 0,
            TaskStatus::Done => 2,
            _ => 1,
        };
        (group, std::cmp::Reverse(row.started_at.or(row.updated_at)))
    });
    Ok(summary)
}

fn label(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    if value.is_empty() {
        return None;
    }
    // The browser offer may contain another URL encoded inside a fragment.
    // Check the original and each decoded layer before clipping display text.
    let mut decoded = value.to_ascii_lowercase();
    for _ in 0..4 {
        if [
            "pairing=",
            "orca-pair:",
            "orcapair:",
            "orca://",
            "bearertoken",
        ]
        .iter()
        .any(|needle| decoded.contains(needle))
        {
            return None;
        }
        if !decoded.contains('%') {
            break;
        }
        let next = url::form_urlencoded::parse(decoded.as_bytes())
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join("&")
            .to_ascii_lowercase();
        if next == decoded || next.strip_suffix('=') == Some(decoded.as_str()) {
            break;
        }
        decoded = next;
    }
    if decoded.contains("%25") || decoded.contains("orca://") || decoded.contains("pairing=") {
        return None;
    }
    let mut chars = value
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch });
    let mut title: String = chars.by_ref().take(120).collect();
    if chars.next().is_some() {
        title.push('…');
    }
    Some(title)
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
        let status = |status| {
            summary
                .rows
                .iter()
                .filter(|row| row.status == status)
                .count()
        };
        assert_eq!(status(TaskStatus::Working), 1);
        assert_eq!(status(TaskStatus::Monitoring), 1);
        assert_eq!(status(TaskStatus::Waiting), 1);
        assert_eq!(status(TaskStatus::Blocked), 1);
        assert_eq!(status(TaskStatus::Done), 1);
        assert_eq!(status(TaskStatus::Unknown), 2);
        assert!(!summary.partial);
        assert!(parse_response(response, "rt_other").is_err());
        assert!(parse_response(br#"{"id":"tray-tasks","ok":false}"#, "rt_test").is_err());
    }

    #[test]
    fn popup_projects_sanitized_rows_without_prompts_or_pairing_credentials() {
        let response = br#"{
          "id":"tray-tasks","ok":true,"_meta":{"runtimeId":"rt_test"},
          "result":{"truncated":false,"totalCount":1,
            "worktrees":[{"worktreeId":"ws-id","displayName":"Build","liveTerminalCount":2,"agents":[
              {"paneKey":"tab:leaf","state":"working","taskTitle":"Build task","agentType":"codex","stateStartedAt":100},
              {"paneKey":"tab2:leaf","state":"waiting","conversationName":"orca%253A%252F%252Fpair%253Fcode%253Dsecret","displayName":"Review","prompt":"never shown"}
            ]}]}
        }"#;
        let summary = parse_response(response, "rt_test").unwrap();
        assert_eq!(summary.rows[0].title, "Review");
        assert_eq!(summary.rows[0].status, TaskStatus::Waiting);
        assert_eq!(summary.rows[1].title, "Build task");
        assert_eq!(summary.rows[1].target.tab_id, "tab");
        assert_eq!(summary.rows[1].workspace, "Build");
        let payload = serde_json::to_string(&summary).unwrap();
        for private in ["secret", "never shown", "ws-id", "tab:leaf"] {
            assert!(!payload.contains(private));
        }
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
