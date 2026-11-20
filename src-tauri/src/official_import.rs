use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    ffi::CString,
    fs,
    io::Write,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::Path,
    process::{Command, Stdio},
};
use tempfile::TempDir;

#[cfg(feature = "desktop")]
pub const VERSION: &str = "v1.4.266";
const PROFILE_DB: &str = "profile-state.db";
const SESSION_DB: &str = "agent-session-domain.db";
const ATTACHMENTS: &str = "agent-session-attachments";
const INDEX: &str = "orca-profile-index.json";

// These are the project and worktree documents in v1.4.266's profile database.
const PROJECT_DOMAINS: &str = "('schemaVersion','repos','projects','projectHostSetups',
    'projectGroups','folderWorkspaces','sparsePresetsByRepo','retiredWorktreeNamesByRepo',
    'retiredWorktreeNamesByNamespace','worktreeMeta','worktreeMetaByIdentity',
    'worktreeIdentityAliases','worktreeLineageById','workspaceLineageByChildKey')";

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProfileIndex {
    schema_version: u32,
    active_profile_id: String,
    profiles: Vec<Profile>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Profile {
    id: String,
    name: String,
    avatar: serde_json::Value,
    kind: String,
    created_at: u64,
    updated_at: u64,
    last_opened_at: u64,
}

#[cfg(feature = "desktop")]
pub fn discover() -> Result<std::path::PathBuf> {
    let home = dirs::home_dir().context("无法定位用户目录")?;
    let system_app = Path::new("/Applications/Orca.app");
    let user_app = home.join("Applications/Orca.app");
    let app = [system_app, user_app.as_path()]
        .into_iter()
        .find(|path| path.is_dir())
        .context("请先安装官方 Orca")?;
    let output = Command::new("/usr/libexec/PlistBuddy")
        .args(["-c", "Print CFBundleShortVersionString"])
        .arg(app.join("Contents/Info.plist"))
        .output()
        .context("无法读取官方 Orca 版本")?;
    ensure!(
        output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == &VERSION[1..],
        "当前导入支持官方 Orca {VERSION}"
    );
    Ok(home.join("Library/Application Support/orca"))
}

pub fn prepare(source: &Path, target: &Path) -> Result<TempDir> {
    let resolved_source = source.canonicalize().context("未找到官方 Orca 数据目录")?;
    let resolved = target.canonicalize().context("无法定位 Tauri 数据目录")?;
    ensure!(
        target.symlink_metadata()?.is_dir()
            && !resolved_source.starts_with(&resolved)
            && !resolved.starts_with(&resolved_source),
        "官方 Orca 与 Tauri 必须使用独立的数据目录"
    );
    let mut index: ProfileIndex = serde_json::from_slice(
        &fs::read(source.join(INDEX)).context("未找到官方 Orca 的本地资料")?,
    )
    .context("无法读取官方 Orca 的资料索引")?;
    ensure!(index.schema_version == 1, "官方 Orca 的资料格式不受支持");
    index
        .profiles
        .retain(|profile| profile.id == index.active_profile_id);
    let [profile] = index.profiles.as_mut_slice() else {
        anyhow::bail!("未找到官方 Orca 当前使用的资料");
    };
    ensure!(
        profile.id.len() <= 128
            && profile
                .id
                .starts_with(|ch: char| ch.is_ascii_alphanumeric())
            && profile
                .id
                .bytes()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, b'_' | b'-')),
        "官方 Orca 的资料标识无效"
    );
    profile.kind = "local".into();

    let staged = tempfile::Builder::new()
        .prefix("orca-data-before-import-")
        .tempdir_in(target.parent().context("无法定位 Tauri 数据的父目录")?)?;
    let profile_path = Path::new("profiles").join(&profile.id);
    let destination = staged.path().join(&profile_path);
    fs::create_dir_all(&destination)?;
    snapshot(&source.join(&profile_path), &destination, PROFILE_DB, 3)?;
    sql(
        &destination,
        PROFILE_DB,
        false,
        &format!(
            "BEGIN;
            DELETE FROM profile_state_documents WHERE domain NOT IN {PROJECT_DOMAINS};
            DELETE FROM profile_state_document_parts WHERE domain NOT IN {PROJECT_DOMAINS};
            DELETE FROM profile_state_document_parts_meta WHERE domain NOT IN {PROJECT_DOMAINS};
            DELETE FROM profile_state_automation_runs;
            INSERT OR REPLACE INTO profile_state_automation_runs_meta
                (domain, presence, domain_version, revision, updated_at, content_hash)
                VALUES ('automationRuns', 'document', 1, 0, 0, '');
            COMMIT;
            VACUUM;"
        ),
    )
    .context("无法准备项目和工作树数据")?;
    remove_repo_cleanup_policy(&destination)?;
    fs::write(staged.path().join(INDEX), serde_json::to_vec(&index)?)?;

    if source.join(SESSION_DB).try_exists()? {
        snapshot(source, staged.path(), SESSION_DB, 2)?;
        import_history(source, staged.path(), target)?;
    } else {
        ensure!(
            !source.join("agent-session-journal.db").try_exists()?,
            "请先在官方 Orca 打开会话历史以完成升级，再重试导入"
        );
    }
    Ok(staged)
}

fn remove_repo_cleanup_policy(directory: &Path) -> Result<()> {
    let payload = sql(
        directory,
        PROFILE_DB,
        true,
        "SELECT payload FROM profile_state_documents WHERE domain='repos';",
    )?;
    if payload.is_empty() {
        return Ok(());
    }
    let mut repos: Vec<serde_json::Map<String, serde_json::Value>> =
        serde_json::from_str(&payload)?;
    let mut changed = false;
    for repo in &mut repos {
        changed |= repo.remove("workspaceCleanupPolicy").is_some();
    }
    if changed {
        let payload = serde_json::to_string(&repos)?;
        let hash = hex::encode(Sha256::digest(payload.as_bytes()));
        let escaped = payload.replace('\'', "''");
        sql(directory, PROFILE_DB, false, &format!(
            "PRAGMA secure_delete=ON;
             UPDATE profile_state_documents SET payload='{escaped}',content_hash='{hash}' WHERE domain='repos';"
        ))?;
    }
    Ok(())
}

fn import_history(source: &Path, staged: &Path, target: &Path) -> Result<()> {
    let attachments = source.join(ATTACHMENTS);
    if attachments.try_exists()? {
        copy_attachments(&attachments, &staged.join(ATTACHMENTS))?;
    }
    let from = attachment_prefix(&attachments)?;
    let to = attachment_prefix(&target.join(ATTACHMENTS))?;
    let source_root = attachment_prefix(source)?;
    let target_root = attachment_prefix(target)?;

    // Preserve saved messages and identity fences. Live PIDs, queued commands
    // and operation receipts belong to the source runtime.
    sql(
        staged,
        SESSION_DB,
        false,
        &format!(
            "BEGIN;
             UPDATE agent_session_records SET record_json = json_set(
                 json_remove(replace(record_json, {source_root}, {target_root}),
                     '$.rewind', '$.conversationCommand', '$.usageLimitContinueCancel', '$.launchArgs'),
                 '$.lease.claimStatus', 'released', '$.lease.ownerProcess', NULL,
                 '$.lease.reservedSpawnToken', NULL, '$.lease.handoffStage', NULL,
                 '$.lease.handoffOperationId', NULL, '$.lease.deathEvidence', NULL,
                 '$.lease.unreconciled', json('false'));
             DELETE FROM agent_session_operations;
             DELETE FROM agent_session_retired_claim_keys;
             DELETE FROM journal_queue_cards;
             DELETE FROM journal_edit_leases;
             DELETE FROM journal_submission_attempts;
             UPDATE journal_items SET
                 payload_json = replace(payload_json, {from}, {to}),
                 wire_json = replace(wire_json, {from}, {to}),
                 detail_json = replace(detail_json, {from}, {to}),
                 wire_bytes = wire_bytes + coalesce(
                     length(CAST(replace(wire_json, {from}, {to}) AS BLOB))
                     - length(CAST(wire_json AS BLOB)), 0)
                 WHERE instr(payload_json, {from}) OR instr(wire_json, {from}) OR instr(detail_json, {from});
             UPDATE journal_events SET changes_json = replace(changes_json, {from}, {to})
                 WHERE instr(changes_json, {from});
             COMMIT;
             VACUUM;"
        ),
    )
    .context("无法准备历史对话")?;
    Ok(())
}

fn attachment_prefix(path: &Path) -> Result<String> {
    let path = path.to_str().context("附件路径编码无效")?;
    let json = serde_json::to_string(&format!("{path}/"))?;
    Ok(format!("'{}'", json[1..json.len() - 1].replace('\'', "''")))
}

fn copy_attachments(source: &Path, target: &Path) -> Result<()> {
    if !source.symlink_metadata()?.is_dir() {
        return Ok(());
    }
    for upload in fs::read_dir(source)? {
        let upload = upload?;
        if !upload.file_type()?.is_dir() {
            continue;
        }
        for file in fs::read_dir(upload.path())? {
            let file = file?;
            if !file.file_type()?.is_file() || file.file_name() == ".upload.part" {
                continue;
            }
            let directory = target.join(upload.file_name());
            fs::create_dir_all(&directory)?;
            fs::copy(file.path(), directory.join(file.file_name()))?;
        }
    }
    Ok(())
}

fn snapshot(source: &Path, target: &Path, name: &str, version: u32) -> Result<()> {
    let database = source.join(name);
    let before = database.symlink_metadata()?;
    ensure!(before.is_file(), "官方 Orca 数据库不可读取");
    let wal = source.join(format!("{name}-wal"));
    let had_wal = wal.try_exists()?;
    // SQLite's backup API includes committed WAL data, even while Orca runs.
    if let Err(error) = run_sqlite(&database, target, true, &format!(".backup {name}")) {
        // macOS SQLite cannot open a closed WAL database read-only when both
        // sidecars are absent. Copy only if no WAL or database change is seen.
        ensure!(
            !had_wal && !wal.try_exists()?,
            "无法创建 {name} 快照 · {error}"
        );
        fs::copy(&database, target.join(name))?;
        let after = database.symlink_metadata()?;
        ensure!(
            !wal.try_exists()?
                && before.ino() == after.ino()
                && before.ctime() == after.ctime()
                && before.ctime_nsec() == after.ctime_nsec()
                && before.len() == after.len(),
            "官方 Orca 数据刚刚发生变化，请重试"
        );
    }
    ensure!(
        sql(
            target,
            name,
            false,
            "PRAGMA journal_mode=DELETE; PRAGMA quick_check; PRAGMA user_version;"
        )? == format!("delete\nok\n{version}"),
        "官方 Orca 的 {name} 格式不受支持或完整性检查失败"
    );
    Ok(())
}

fn sql(directory: &Path, name: &str, readonly: bool, statement: &str) -> Result<String> {
    run_sqlite(&directory.join(name), directory, readonly, statement)
}

fn run_sqlite(
    database: &Path,
    directory: &Path,
    readonly: bool,
    statement: &str,
) -> Result<String> {
    let mut command = Command::new("/usr/bin/sqlite3");
    command.args([
        "-batch",
        "-bail",
        "-init",
        "/dev/null",
        "-cmd",
        ".timeout 3000",
    ]);
    if readonly {
        command.arg("-readonly");
    }
    let mut child = command
        .current_dir(directory)
        .arg(database)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("无法启动 macOS SQLite 工具")?;
    let mut input = child.stdin.take().context("SQLite 输入管道不可用")?;
    let written = writeln!(input, "{statement}");
    drop(input);
    let output = child.wait_with_output()?;
    ensure!(
        output.status.success(),
        "SQLite 处理失败 · {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    written?;
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

pub fn swap(active: &Path, backup: &Path) -> Result<()> {
    let active = CString::new(active.as_os_str().as_bytes())?;
    let backup = CString::new(backup.as_os_str().as_bytes())?;
    // SAFETY: both C strings live for the whole call. RENAME_SWAP keeps one
    // complete directory at each name even if the application exits mid-import.
    let result = unsafe {
        libc::renameatx_np(
            libc::AT_FDCWD,
            active.as_ptr(),
            libc::AT_FDCWD,
            backup.as_ptr(),
            libc::RENAME_SWAP,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context("无法切换 Tauri 数据目录");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imports_saved_data_and_keeps_the_previous_runtime_data() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("official");
        let target = temp.path().join("orca-data");
        let profile = source.join("profiles/profile-1");
        fs::create_dir_all(&profile)?;
        fs::create_dir(&target)?;
        fs::write(target.join("existing"), "keep this")?;
        fs::write(source.join("orca-devices.json"), "private")?;
        fs::write(
            source.join("orca-profile-index.json"),
            r#"{"schemaVersion":1,"activeProfileId":"profile-1","profiles":[{"id":"profile-1","name":"Work","avatar":{"kind":"initials","initials":"W","color":"neutral"},"kind":"cloud-linked","cloud":{},"createdAt":1,"updatedAt":1,"lastOpenedAt":1}]}"#,
        )?;
        sql(
            &profile,
            PROFILE_DB,
            false,
            "
            PRAGMA user_version=3;
            PRAGMA journal_mode=WAL;
            CREATE TABLE profile_state_documents(domain TEXT, payload TEXT, content_hash TEXT);
            CREATE TABLE profile_state_document_parts(domain TEXT);
            CREATE TABLE profile_state_document_parts_meta(domain TEXT);
            CREATE TABLE profile_state_automation_runs(run TEXT);
            CREATE TABLE profile_state_automation_runs_meta(
                domain TEXT PRIMARY KEY, presence TEXT, domain_version INTEGER,
                revision INTEGER, updated_at INTEGER, content_hash TEXT
            );
            INSERT INTO profile_state_automation_runs_meta VALUES ('automationRuns','array',1,1,1,'abc');
            INSERT INTO profile_state_documents VALUES ('projects','[{}]','a'),('settings','{}','b'),
               ('repos','[{\"workspaceCleanupPolicy\":{},\"path\":\"/tmp/example\"}]','c');
        ",
        )?;
        sql(&source, SESSION_DB, false, "
            PRAGMA user_version=2;
            CREATE TABLE agent_session_records(session_id TEXT, record_json TEXT);
            INSERT INTO agent_session_records VALUES ('s', '{\"lease\":{\"ownerProcess\":{\"pid\":10},\"claimStatus\":\"held\"}}');
            CREATE TABLE agent_session_operations(op TEXT);
            CREATE TABLE agent_session_retired_claim_keys(key TEXT);
            CREATE TABLE journal_queue_cards(card TEXT);
            CREATE TABLE journal_edit_leases(lease TEXT);
            CREATE TABLE journal_submission_attempts(attempt TEXT);
            CREATE TABLE journal_items(payload_json TEXT, wire_json TEXT, wire_bytes INTEGER, detail_json TEXT);
            CREATE TABLE journal_events(changes_json TEXT);
            INSERT INTO journal_queue_cards VALUES ('pending');
        ")?;
        let upload_id = "00000000-0000-0000-0000-000000000001";
        let upload = source.join(ATTACHMENTS).join(upload_id);
        fs::create_dir_all(&upload)?;
        fs::write(upload.join("image.png"), "saved attachment")?;
        let original = attachment_prefix(&upload)?;
        sql(
            &source,
            SESSION_DB,
            false,
            &format!(
                "INSERT INTO journal_items VALUES ({original} || 'image.png', NULL, NULL, NULL);"
            ),
        )?;

        let staged = prepare(&source, &target)?;
        assert!(target.join("existing").is_file());
        let backup = staged.keep();
        swap(&target, &backup)?;
        assert!(backup.join("existing").is_file());
        assert!(!target.join("orca-devices.json").exists());
        assert!(source.join("orca-devices.json").is_file());
        assert!(target
            .join(ATTACHMENTS)
            .join(upload_id)
            .join("image.png")
            .is_file());
        assert_eq!(
            sql(
                &target,
                SESSION_DB,
                true,
                "SELECT payload_json FROM journal_items;"
            )?,
            target
                .join(ATTACHMENTS)
                .join(upload_id)
                .join("image.png")
                .to_string_lossy()
        );
        assert_eq!(
            sql(
                &target.join("profiles/profile-1"),
                PROFILE_DB,
                true,
                "SELECT domain FROM profile_state_documents ORDER BY domain;"
            )?,
            "projects\nrepos"
        );
        assert_eq!(
            sql(
                &target.join("profiles/profile-1"),
                PROFILE_DB,
                true,
                "SELECT payload FROM profile_state_documents WHERE domain='repos';"
            )?,
            r#"[{"path":"/tmp/example"}]"#
        );
        assert_eq!(
            sql(&target, SESSION_DB, true, "SELECT json_extract(record_json,'$.lease.claimStatus') FROM agent_session_records; SELECT count(*) FROM journal_queue_cards;")?,
            "released\n0"
        );
        assert_eq!(
            sql(&target.join("profiles/profile-1"), PROFILE_DB, true,
                "SELECT presence FROM profile_state_automation_runs_meta WHERE domain='automationRuns';")?,
            "document"
        );
        swap(&target, &backup)?;
        assert!(target.join("existing").is_file());
        Ok(())
    }
}
