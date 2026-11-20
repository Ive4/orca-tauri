//! Versioned, transactional core updates. The caller starts and health-checks a
//! prepared core *before* `UpdateSession::commit` changes the active pointer.
//! A crash while downloading, extracting or testing therefore keeps the old core.
//!
//! Checksums detect corrupt downloads. They are not signatures: the configured
//! GitHub repository and its release publishers remain part of the trust boundary.

use anyhow::{anyhow, bail, ensure, Context, Result};
use reqwest::{redirect::Policy, Client, Response};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tempfile::NamedTempFile;
use tokio::{
    io::AsyncWriteExt,
    sync::{Mutex, OwnedMutexGuard},
};
use zip::ZipArchive;

const MANIFEST_NAME: &str = "core-manifest.json";
const RECEIPT_NAME: &str = ".orca-tauri-install.json";
const ACTIVE_NAME: &str = "active.json";
const MAX_DOWNLOAD_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_FILE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_UNPACKED_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: u64 = 80_000;
const MAX_METADATA_BYTES: usize = 8 * 1024 * 1024;
const MAX_MANIFEST_BYTES: usize = 64 * 1024;
const MAX_CHECKSUM_BYTES: usize = 16 * 1024;
const MAX_RELEASE_PAGES: u32 = 10;
type CoreVersionKey = (Version, Version);

/// All paths refer to the ZIP root, not the shell's current working directory.
/// Only args/environment are expanded by the launcher; executable is a literal
/// package-relative path. Unknown metadata is allowed for release provenance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoreManifest {
    pub schema_version: u32,
    /// Official Orca release tag.
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package_revision: Option<String>,
    pub target: String,
    pub executable: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "default_working_directory")]
    pub working_directory: String,
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    #[serde(default = "default_health_path")]
    pub health_path: String,
    #[serde(default = "default_ui_path")]
    pub ui_path: String,
}

impl CoreManifest {
    pub fn display_version(&self) -> String {
        display_core_version(&self.version, self.package_revision.as_deref())
    }

    pub(crate) fn is_older_than(&self, version: &str, revision: Option<&str>) -> Result<bool> {
        Ok(
            core_version_key(&self.version, self.package_revision.as_deref())?
                < core_version_key(version, revision)?,
        )
    }

    /// The orcad slot identifies itself with this build version in readiness.
    pub fn runtime_slot_version(&self) -> Option<&str> {
        self.environment.get("ORCA_VERSION").map(String::as_str)
    }
}

fn default_working_directory() -> String {
    ".".into()
}
// Orca --json stdout readiness is authoritative. This HTTP path is only an
// additional static-resource reachability probe, not an authenticated health API.
fn default_health_path() -> String {
    "/web-index.html".into()
}
fn default_ui_path() -> String {
    "/web-index.html".into()
}

#[derive(Debug, Clone)]
pub struct InstalledCore {
    pub directory: PathBuf,
    pub manifest: CoreManifest,
    pub archive_sha256: String,
}

#[derive(Debug, Clone)]
pub struct CoreRelease {
    pub version: String,
    pub tag: String,
    package_revision: Option<String>,
    archive: ReleaseAsset,
    checksum: ReleaseAsset,
}

impl CoreRelease {
    pub fn display_version(&self) -> String {
        display_core_version(&self.version, self.package_revision.as_deref())
    }

    pub fn is_newer_than(&self, installed: &CoreManifest) -> Result<bool> {
        installed.is_older_than(&self.version, self.package_revision.as_deref())
    }
}

#[derive(Debug, Clone, Deserialize)]
struct ReleaseAsset {
    name: String,
    size: u64,
}

#[derive(Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<ReleaseAsset>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CoreReference {
    directory: String,
    version: String,
    archive_sha256: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct ActiveState {
    schema_version: u32,
    current: CoreReference,
    #[serde(default)]
    previous: Option<CoreReference>,
}

#[derive(Debug, Serialize, Deserialize)]
struct InstallReceipt {
    schema_version: u32,
    version: String,
    target: String,
    archive_sha256: String,
}

pub struct CoreUpdater {
    root: PathBuf,
    repository: Option<String>,
    target: String,
    client: Client,
    gate: Arc<Mutex<()>>,
}

/// Hold this session until candidate startup, health check and commit/rollback
/// have all finished. A dropped, uncommitted session leaves active.json intact.
pub struct UpdateSession<'a> {
    updater: &'a CoreUpdater,
    _guard: Arc<OwnedMutexGuard<()>>,
}

impl CoreUpdater {
    pub fn new(root: PathBuf, repository: Option<String>, target: String) -> Result<Self> {
        if let Some(repository) = repository.as_deref() {
            validate_repository(repository)?;
        }
        validate_target(&target)?;
        let client = release_client(None)?;
        Ok(Self {
            root,
            repository,
            target,
            client,
            gate: Arc::new(Mutex::new(())),
        })
    }

    /// Applies only to external release downloads. The process manager uses a
    /// separate no_proxy() client for loopback health checks. None preserves
    /// reqwest's normal environment/system proxy behavior.
    pub fn with_proxy(mut self, proxy: Option<&str>) -> Result<Self> {
        self.client = release_client(proxy)?;
        Ok(self)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn load_active(&self) -> Result<Option<InstalledCore>> {
        self.read_state()?
            .map(|state| self.load_reference(&state.current))
            .transpose()
    }

    pub fn load_previous(&self) -> Result<Option<InstalledCore>> {
        self.read_state()?
            .and_then(|state| state.previous)
            .map(|reference| self.load_reference(&reference))
            .transpose()
    }

    #[cfg(feature = "desktop")]
    pub(crate) async fn latest_official_version(&self) -> Result<Version> {
        let response = self
            .client
            .get("https://api.github.com/repos/stablyai/orca/releases/latest")
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .context("无法连接 Orca 官方发布源")?;
        let bytes = bounded_response(response, MAX_METADATA_BYTES)
            .await
            .context("无法读取 Orca 官方发布信息")?;
        let release: GithubRelease =
            serde_json::from_slice(&bytes).context("无法解析官方发布信息")?;
        let version = validate_version(&release.tag_name).context("无法识别官方版本号")?;
        ensure!(
            !release.draft && !release.prerelease && version.pre.is_empty(),
            "官方发布源未返回稳定版"
        );
        Ok(version)
    }

    pub async fn begin_update(&self) -> UpdateSession<'_> {
        UpdateSession {
            updater: self,
            _guard: Arc::new(self.gate.clone().lock_owned().await),
        }
    }

    fn initialize_directories(&self) -> Result<()> {
        ensure_private_directory(&self.root)?;
        ensure_private_directory(&self.root.join("versions"))?;
        ensure_private_directory(&self.root.join("staging"))?;
        Ok(())
    }

    fn read_state(&self) -> Result<Option<ActiveState>> {
        let path = self.root.join(ACTIVE_NAME);
        let bytes = match read_bounded_file(&path, MAX_MANIFEST_BYTES) {
            Ok(bytes) => bytes,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                return Ok(None)
            }
            Err(error) => return Err(error).context("read active core pointer"),
        };
        let state: ActiveState =
            serde_json::from_slice(&bytes).context("parse active core pointer")?;
        ensure!(
            state.schema_version == 1,
            "unsupported active core pointer schema"
        );
        Ok(Some(state))
    }

    fn load_reference(&self, reference: &CoreReference) -> Result<InstalledCore> {
        validate_version(&reference.version)?;
        validate_digest(&reference.archive_sha256)?;
        let expected_name =
            install_directory_name(&reference.version, &self.target, &reference.archive_sha256);
        ensure!(
            reference.directory == expected_name,
            "invalid installed-core directory in active pointer"
        );
        let directory = self.root.join("versions").join(&reference.directory);
        let core = inspect_installed(&directory, &self.target)?;
        ensure!(
            core.manifest.version == reference.version,
            "active core version differs from manifest"
        );
        ensure!(
            core.archive_sha256 == reference.archive_sha256,
            "active core digest differs from receipt"
        );
        Ok(core)
    }
}

impl UpdateSession<'_> {
    /// Returns the newest stable compatible core release, even when it is already
    /// installed. The caller decides whether to update or request a manual rollback.
    /// Shell releases are deliberately ignored; `/releases/latest` is not used.
    pub async fn latest_release(&self) -> Result<Option<CoreRelease>> {
        let repository = self.updater.repository.as_deref().context(
            "core release repository is not configured; set update_repository to OWNER/REPO",
        )?;
        let mut best: Option<(CoreVersionKey, CoreRelease)> = None;
        for page in 1..=MAX_RELEASE_PAGES {
            let response = self
                .updater
                .client
                .get(format!(
                    "https://api.github.com/repos/{repository}/releases"
                ))
                .query(&[("per_page", 100), ("page", page)])
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28")
                .send()
                .await
                .context("request core releases")?;
            let bytes = bounded_response(response, MAX_METADATA_BYTES).await?;
            let releases: Vec<GithubRelease> =
                serde_json::from_slice(&bytes).context("decode GitHub release metadata")?;
            let page_complete = releases.len() < 100;
            for release in releases {
                if let Some((version, release)) = compatible_release(release, &self.updater.target)?
                {
                    if best.as_ref().is_none_or(|(current, _)| version > *current) {
                        best = Some((version, release));
                    }
                }
            }
            if page_complete {
                return Ok(best.map(|(_, release)| release));
            }
        }
        // Do not silently claim this is the newest release after truncating the
        // query. Separate a very busy shell release feed into its own repository.
        bail!(
            "release feed exceeds {MAX_RELEASE_PAGES} pages; narrow the configured core repository"
        )
    }

    pub async fn prepare_release(&self, release: &CoreRelease) -> Result<InstalledCore> {
        let repository = self
            .updater
            .repository
            .as_deref()
            .context("core release repository is not configured")?;
        validate_version(&release.version)?;
        let revision = release.package_revision.as_deref();
        let expected_archive =
            core_release_archive(&release.version, revision, &self.updater.target);
        ensure!(
            core_tag_identity(&release.tag, &self.updater.target)
                .is_some_and(|(version, tagged_revision)| version == release.version
                    && tagged_revision == revision),
            "unexpected release tag"
        );
        ensure!(
            release.archive.name == expected_archive,
            "unexpected core asset name"
        );
        ensure!(
            release.checksum.name == format!("{expected_archive}.sha256"),
            "unexpected checksum asset name"
        );
        ensure!(
            release.archive.size > 0 && release.archive.size <= MAX_DOWNLOAD_BYTES,
            "core archive exceeds size limit"
        );
        ensure!(
            release.checksum.size > 0 && release.checksum.size <= MAX_CHECKSUM_BYTES as u64,
            "checksum asset exceeds size limit"
        );
        self.updater.initialize_directories()?;
        // Construct URLs locally. API-provided browser_download_url values are
        // intentionally not trusted or followed as arbitrary download endpoints.
        let base_url = format!(
            "https://github.com/{repository}/releases/download/{}",
            release.tag
        );
        let checksum_response = self
            .updater
            .client
            .get(format!("{base_url}/{}", release.checksum.name))
            .send()
            .await
            .context("download core checksum")?;
        let checksum_bytes = bounded_response(checksum_response, MAX_CHECKSUM_BYTES).await?;
        let expected_digest = parse_checksum(&checksum_bytes, &expected_archive)?;
        let archive = self
            .download_archive(
                &format!("{base_url}/{}", release.archive.name),
                release.archive.size,
                &expected_digest,
            )
            .await?;
        let archive_path = archive.path().to_path_buf();
        let root = self.updater.root.clone();
        let target = self.updater.target.clone();
        let version = release.version.clone();
        let revision = release.package_revision.clone();
        let guard = self._guard.clone();
        let prepared = tokio::task::spawn_blocking(move || {
            // If the async caller is cancelled, extraction still owns the gate
            // and download file until this non-cancellable worker has finished.
            let _guard = guard;
            let _download = archive;
            prepare_archive(
                &root,
                &archive_path,
                &version,
                &target,
                &expected_digest,
                revision.as_deref(),
            )
        })
        .await
        .context("core extraction task failed")??;
        Ok(prepared)
    }

    /// Explicit offline import. The supplied digest must come from the release
    /// publisher through a trusted channel; it is verified before ZIP parsing.
    pub async fn prepare_local_archive(
        &self,
        archive: &Path,
        expected_version: &str,
        sha256: &str,
    ) -> Result<InstalledCore> {
        validate_version(expected_version)?;
        validate_digest(sha256)?;
        self.updater.initialize_directories()?;
        let root = self.updater.root.clone();
        let target = self.updater.target.clone();
        let version = expected_version.to_owned();
        let digest = sha256.to_ascii_lowercase();
        let archive = archive.to_path_buf();
        let guard = self._guard.clone();
        tokio::task::spawn_blocking(move || {
            let _guard = guard;
            let metadata = fs::metadata(&archive).context("inspect offline archive")?;
            ensure!(
                metadata.is_file() && metadata.len() <= MAX_DOWNLOAD_BYTES,
                "offline core archive exceeds size limit"
            );
            ensure!(
                hash_file(&archive)? == digest,
                "offline core archive SHA-256 mismatch"
            );
            prepare_archive(&root, &archive, &version, &target, &digest, None)
        })
        .await
        .context("offline core preparation task failed")?
    }

    /// Call only after the candidate has started and passed health checks. If
    /// startup fails, stop it and restart load_active(); no pointer rollback is
    /// needed. This deliberately has no Drop-based commit or pending state.
    pub fn commit(&self, core: &InstalledCore) -> Result<()> {
        self.updater.initialize_directories()?;
        validate_version(&core.manifest.version)?;
        validate_digest(&core.archive_sha256)?;
        let directory = install_directory_name(
            &core.manifest.version,
            &self.updater.target,
            &core.archive_sha256,
        );
        let expected_path = self.updater.root.join("versions").join(&directory);
        ensure!(
            core.directory == expected_path,
            "cannot activate a core outside the updater's versions directory"
        );
        let inspected = inspect_installed(&expected_path, &self.updater.target)?;
        ensure!(
            inspected.manifest.version == core.manifest.version
                && inspected.manifest.package_revision == core.manifest.package_revision
                && inspected.archive_sha256 == core.archive_sha256,
            "prepared core changed before activation"
        );
        let current = CoreReference {
            directory,
            version: core.manifest.version.clone(),
            archive_sha256: core.archive_sha256.clone(),
        };
        let state = self.updater.read_state()?;
        let previous = match state {
            Some(state) if state.current == current => state.previous,
            Some(state) => Some(state.current),
            None => None,
        };
        let state = ActiveState {
            schema_version: 1,
            current,
            previous,
        };
        atomic_write_json(&self.updater.root.join(ACTIVE_NAME), &state)
            .context("commit healthy core version")
    }

    async fn download_archive(
        &self,
        url: &str,
        expected_size: u64,
        digest: &str,
    ) -> Result<NamedTempFile> {
        let mut response = self
            .updater
            .client
            .get(url)
            .send()
            .await
            .context("download core archive")?
            .error_for_status()
            .context("core archive HTTP error")?;
        if let Some(length) = response.content_length() {
            ensure!(
                length == expected_size && length <= MAX_DOWNLOAD_BYTES,
                "core archive content length differs from release metadata"
            );
        }
        let temporary = NamedTempFile::new_in(self.updater.root.join("staging"))
            .context("create core download file")?;
        let mut output = tokio::fs::File::from_std(temporary.reopen()?);
        let mut hasher = Sha256::new();
        let mut length = 0u64;
        while let Some(chunk) = response
            .chunk()
            .await
            .context("read core download stream")?
        {
            length = length
                .checked_add(chunk.len() as u64)
                .context("archive length overflow")?;
            ensure!(
                length <= expected_size && length <= MAX_DOWNLOAD_BYTES,
                "core download exceeds expected size"
            );
            hasher.update(&chunk);
            output
                .write_all(&chunk)
                .await
                .context("write core download")?;
        }
        ensure!(length == expected_size, "core download is incomplete");
        ensure!(
            hex::encode(hasher.finalize()) == digest,
            "core archive SHA-256 mismatch"
        );
        output.sync_all().await.context("flush core download")?;
        drop(output);
        Ok(temporary)
    }
}

fn compatible_release(
    release: GithubRelease,
    target: &str,
) -> Result<Option<(CoreVersionKey, CoreRelease)>> {
    if release.draft || release.prerelease {
        return Ok(None);
    }
    let Some((version, revision)) = core_tag_identity(&release.tag_name, target) else {
        return Ok(None);
    };
    let Ok(parsed) = core_version_key(&version, revision) else {
        return Ok(None);
    };
    if !parsed.0.pre.is_empty() || !parsed.0.build.is_empty() {
        return Ok(None);
    }
    let archive_name = core_release_archive(&version, revision, target);
    let checksum_name = format!("{archive_name}.sha256");
    let archives: Vec<_> = release
        .assets
        .iter()
        .filter(|asset| asset.name == archive_name)
        .collect();
    let checksums: Vec<_> = release
        .assets
        .iter()
        .filter(|asset| asset.name == checksum_name)
        .collect();
    if archives.is_empty() || checksums.is_empty() {
        return Ok(None);
    }
    ensure!(
        archives.len() == 1 && checksums.len() == 1,
        "release has duplicate core assets"
    );
    let selected = CoreRelease {
        version,
        package_revision: revision.map(str::to_owned),
        tag: release.tag_name,
        archive: archives[0].clone(),
        checksum: checksums[0].clone(),
    };
    Ok(Some((parsed, selected)))
}

fn archive_name(version: &str, target: &str) -> String {
    format!("orca-core-{version}-{target}.zip")
}

fn core_release_archive(version: &str, revision: Option<&str>, target: &str) -> String {
    match revision {
        Some(_) => archive_name(
            &format!("v{}", display_core_version(version, revision)),
            target,
        ),
        None => archive_name(version, target),
    }
}

fn core_tag_identity<'a>(tag: &'a str, target: &str) -> Option<(String, Option<&'a str>)> {
    if let Some(identity) = tag.strip_prefix("core-macos-v") {
        if target != "aarch64-apple-darwin" {
            return None;
        }
        return match identity.split_once('-') {
            Some((revision, upstream)) => Some((format!("v{upstream}"), Some(revision))),
            None => Some((format!("v{identity}"), None)),
        };
    }
    if let Some(identity) = tag.strip_prefix("core-windows-v") {
        if target != "x86_64-pc-windows-msvc" {
            return None;
        }
        let (revision, upstream) = identity.split_once('-')?;
        return Some((format!("v{upstream}"), Some(revision)));
    }
    tag.strip_prefix("core-")
        .map(|version| (version.into(), None))
}

fn display_core_version(version: &str, revision: Option<&str>) -> String {
    format!(
        "{}-{}",
        revision.unwrap_or("0.0.0"),
        version.trim_start_matches('v')
    )
}

fn core_version_key(version: &str, revision: Option<&str>) -> Result<CoreVersionKey> {
    let version = validate_version(version)?;
    let revision = match revision {
        Some(value) => {
            ensure!(value.len() <= 64, "core packaging revision is too long");
            let revision = Version::parse(value).context("invalid core packaging revision")?;
            ensure!(
                revision.pre.is_empty()
                    && revision.build.is_empty()
                    && revision != Version::new(0, 0, 0),
                "core packaging revision must be a positive stable version"
            );
            revision
        }
        None => Version::new(0, 0, 0),
    };
    Ok((version, revision))
}

fn release_client(proxy: Option<&str>) -> Result<Client> {
    let mut builder = Client::builder()
        .user_agent(concat!("orca-tauri/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(600))
        .https_only(true)
        .redirect(Policy::custom(|attempt| {
            if attempt.previous().len() >= 5 {
                return attempt.error("too many GitHub release redirects");
            }
            let allowed_host = matches!(
                attempt.url().host_str(),
                Some(
                    "github.com"
                        | "api.github.com"
                        | "release-assets.githubusercontent.com"
                        | "objects.githubusercontent.com"
                        | "github-releases.githubusercontent.com"
                )
            );
            if attempt.url().scheme() == "https" && allowed_host {
                attempt.follow()
            } else {
                attempt.error("release redirect left the GitHub HTTPS host allowlist")
            }
        }));
    if let Some(proxy) = proxy {
        let parsed = reqwest::Url::parse(proxy).context("invalid release download proxy URL")?;
        ensure!(
            matches!(parsed.scheme(), "http" | "https"),
            "download proxy must use http:// or https://"
        );
        builder =
            builder.proxy(reqwest::Proxy::all(proxy).context("configure release download proxy")?);
    }
    builder.build().context("create release download client")
}

fn install_directory_name(version: &str, target: &str, digest: &str) -> String {
    format!("{version}-{target}-{}", &digest[..16])
}

async fn bounded_response(response: Response, maximum: usize) -> Result<Vec<u8>> {
    let mut response = response
        .error_for_status()
        .context("GitHub release HTTP error (check repository, network and API rate limit)")?;
    ensure!(
        response
            .content_length()
            .is_none_or(|size| size <= maximum as u64),
        "release metadata exceeds size limit"
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.context("read release response")? {
        ensure!(
            bytes.len().saturating_add(chunk.len()) <= maximum,
            "release metadata exceeds size limit"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn validate_repository(repository: &str) -> Result<()> {
    let parts: Vec<_> = repository.split('/').collect();
    ensure!(
        parts.len() == 2
            && parts.iter().all(|part| {
                !part.is_empty()
                    && *part != "."
                    && *part != ".."
                    && part.len() <= 100
                    && part.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
                    })
            }),
        "update_repository must be a literal GitHub OWNER/REPO, without URL or placeholders"
    );
    Ok(())
}

fn validate_target(target: &str) -> Result<()> {
    ensure!(
        !target.is_empty()
            && target.len() <= 80
            && target
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
        "invalid core target triple"
    );
    Ok(())
}

fn validate_version(version: &str) -> Result<Version> {
    ensure!(version.len() <= 80, "core version is too long");
    let version = version
        .strip_prefix('v')
        .context("core versions must start with v")?;
    Version::parse(version).context("core version must follow semantic versioning, e.g. v1.4.264")
}

fn validate_digest(digest: &str) -> Result<()> {
    ensure!(
        digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid SHA-256 digest"
    );
    Ok(())
}

fn parse_checksum(bytes: &[u8], expected_filename: &str) -> Result<String> {
    let text = std::str::from_utf8(bytes)
        .context("checksum asset is not UTF-8")?
        .trim();
    let lines: Vec<_> = text.lines().collect();
    ensure!(
        lines.len() == 1,
        "checksum asset must contain exactly one entry"
    );
    let fields: Vec<_> = lines[0].split_whitespace().collect();
    ensure!(
        fields.len() == 2,
        "checksum entry must contain digest and asset filename"
    );
    validate_digest(fields[0])?;
    ensure!(
        fields[1].strip_prefix('*').unwrap_or(fields[1]) == expected_filename,
        "checksum filename differs from core archive"
    );
    Ok(fields[0].to_ascii_lowercase())
}

fn prepare_archive(
    root: &Path,
    archive: &Path,
    version: &str,
    target: &str,
    digest: &str,
    expected_revision: Option<&str>,
) -> Result<InstalledCore> {
    validate_version(version)?;
    validate_target(target)?;
    validate_digest(digest)?;
    let name = install_directory_name(version, target, digest);
    let destination = root.join("versions").join(&name);
    if destination.exists() {
        let core = inspect_installed(&destination, target)?;
        ensure!(
            core.manifest.version == version && core.archive_sha256 == digest,
            "immutable core directory already exists with different content"
        );
        ensure!(
            expected_revision
                .is_none_or(|revision| core.manifest.package_revision.as_deref() == Some(revision)),
            "core packaging revision differs from release tag"
        );
        return Ok(core);
    }
    let staging = tempfile::Builder::new()
        .prefix("extract-")
        .tempdir_in(root.join("staging"))
        .context("create isolated core extraction directory")?;
    extract_archive(archive, staging.path())?;
    let manifest = inspect_manifest(staging.path(), target)?;
    ensure!(
        manifest.version == version,
        "core manifest version differs from release tag"
    );
    ensure!(
        expected_revision
            .is_none_or(|revision| manifest.package_revision.as_deref() == Some(revision)),
        "core packaging revision differs from release tag"
    );
    ensure!(
        !staging.path().join(RECEIPT_NAME).exists(),
        "archive contains reserved install receipt"
    );
    atomic_write_json(
        &staging.path().join(RECEIPT_NAME),
        &InstallReceipt {
            schema_version: 1,
            version: version.into(),
            target: target.into(),
            archive_sha256: digest.into(),
        },
    )?;
    // Both paths are on the same filesystem. Nothing touches current/previous
    // installation directories, even if the same upstream version is republished.
    fs::rename(staging.path(), &destination).context("publish immutable core directory")?;
    sync_directory(&root.join("versions"))?;
    // TempDir cleanup now sees the old path as absent; do not keep a leaked path.
    drop(staging);
    Ok(InstalledCore {
        directory: destination,
        manifest,
        archive_sha256: digest.into(),
    })
}

fn inspect_installed(directory: &Path, target: &str) -> Result<InstalledCore> {
    ensure_regular_directory(directory)?;
    let manifest = inspect_manifest(directory, target)?;
    let receipt: InstallReceipt = serde_json::from_slice(&read_bounded_file(
        &directory.join(RECEIPT_NAME),
        MAX_MANIFEST_BYTES,
    )?)
    .context("read installed core receipt")?;
    ensure!(
        receipt.schema_version == 1
            && receipt.version == manifest.version
            && receipt.target == target,
        "installed core receipt does not match manifest"
    );
    validate_digest(&receipt.archive_sha256)?;
    Ok(InstalledCore {
        directory: directory.into(),
        manifest,
        archive_sha256: receipt.archive_sha256,
    })
}

fn inspect_manifest(directory: &Path, target: &str) -> Result<CoreManifest> {
    let manifest: CoreManifest = serde_json::from_slice(&read_bounded_file(
        &directory.join(MANIFEST_NAME),
        MAX_MANIFEST_BYTES,
    )?)
    .context("read core-manifest.json at archive root")?;
    ensure!(
        manifest.schema_version == 1,
        "unsupported core manifest schema"
    );
    core_version_key(&manifest.version, manifest.package_revision.as_deref())?;
    ensure!(
        manifest.target == target,
        "core manifest platform differs from this shell"
    );
    let executable = safe_relative_path(&manifest.executable, false)?;
    let executable_path = checked_existing_path(directory, &executable)?;
    ensure!(
        executable_path.is_file(),
        "core executable is not a regular file"
    );
    if manifest.working_directory != "." {
        let working_directory = safe_relative_path(&manifest.working_directory, false)?;
        let working_directory = checked_existing_path(directory, &working_directory)?;
        ensure!(
            working_directory.is_dir(),
            "core working_directory is not a directory"
        );
    }
    validate_http_path(&manifest.health_path)?;
    validate_http_path(&manifest.ui_path)?;
    ensure!(
        manifest.args.len() <= 256
            && manifest
                .args
                .iter()
                .all(|arg| arg.len() <= 8192 && !arg.contains('\0')),
        "core argument list exceeds limits or contains NUL"
    );
    ensure!(
        manifest.environment.len() <= 128
            && manifest.environment.iter().all(|(name, value)| {
                !name.is_empty()
                    && !name.contains(['=', '\0'])
                    && name.len() <= 256
                    && value.len() <= 32768
                    && !value.contains('\0')
            }),
        "invalid core environment entry"
    );
    Ok(manifest)
}

fn validate_http_path(path: &str) -> Result<()> {
    ensure!(
        path.starts_with('/')
            && !path.starts_with("//")
            && !path.contains(['\\', '\r', '\n', '\0', '#'])
            && path.len() <= 2048,
        "manifest health/UI path must be a local absolute URL path"
    );
    Ok(())
}

fn checked_existing_path(root: &Path, relative: &Path) -> Result<PathBuf> {
    let mut path = root.to_path_buf();
    for component in relative.components() {
        path.push(component);
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("missing package file {}", relative.display()))?;
        ensure!(
            !is_link_or_reparse(&metadata),
            "core path contains a symbolic link or reparse point"
        );
    }
    Ok(path)
}

/// Stronger than host-dependent Path::components: reject Windows drive letters,
/// ADS names, reserved devices and separators even while validating on Unix.
fn safe_relative_path(name: &str, directory_entry: bool) -> Result<PathBuf> {
    let name = if directory_entry {
        name.strip_suffix('/').unwrap_or(name)
    } else {
        name
    };
    ensure!(
        !name.is_empty() && name.len() <= 4096 && !name.starts_with('/') && !name.contains('\\'),
        "unsafe ZIP path"
    );
    let mut output = PathBuf::new();
    for component in name.split('/') {
        ensure!(
            !component.is_empty() && component != "." && component != "..",
            "ZIP path traversal or empty path component"
        );
        ensure!(
            !component.ends_with(['.', ' ']) && component.encode_utf16().count() <= 255,
            "ZIP path is ambiguous or too long on Windows"
        );
        ensure!(
            !component
                .chars()
                .any(|c| c.is_control() || matches!(c, ':' | '<' | '>' | '"' | '|' | '?' | '*')),
            "ZIP path contains an invalid Windows filename character"
        );
        let stem = component.split('.').next().unwrap_or("").to_uppercase();
        let reserved = matches!(
            stem.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$" | "CONIN$" | "CONOUT$"
        ) || ["COM", "LPT"].iter().any(|prefix| {
            stem.strip_prefix(prefix).is_some_and(|suffix| {
                matches!(
                    suffix,
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )
            })
        });
        ensure!(!reserved, "ZIP path uses a reserved Windows device name");
        output.push(component);
    }
    Ok(output)
}

fn extract_archive(archive_path: &Path, destination: &Path) -> Result<()> {
    let mut archive_file = File::open(archive_path).context("open downloaded core archive")?;
    let declared_entries = check_zip_entry_count(&mut archive_file)?;
    archive_file.seek(SeekFrom::Start(0))?;
    let mut archive = ZipArchive::new(archive_file).context("read core ZIP directory")?;
    ensure!(
        archive.len() as u64 == declared_entries && declared_entries <= MAX_ARCHIVE_ENTRIES,
        "ZIP entry count mismatch or limit exceeded"
    );
    let mut seen = HashSet::new();
    let mut unpacked = 0u64;
    for index in 0..archive.len() {
        let entry = archive
            .by_index(index)
            .context("read ZIP entry (encrypted entries are not supported)")?;
        let name = entry.name().to_owned();
        let relative = safe_relative_path(&name, entry.is_dir())?;
        ensure!(
            entry.enclosed_name().is_some(),
            "ZIP path leaves extraction root"
        );
        ensure!(
            seen.insert(relative.to_string_lossy().to_lowercase()),
            "ZIP contains duplicate or case-colliding paths"
        );
        let mode = entry.unix_mode().unwrap_or(0);
        let kind = mode & 0o170000;
        ensure!(
            matches!(kind, 0 | 0o100000 | 0o040000),
            "ZIP contains a symlink or special file"
        );
        ensure!(
            entry.size() <= MAX_FILE_BYTES,
            "ZIP entry exceeds single-file limit"
        );
        unpacked = unpacked
            .checked_add(entry.size())
            .context("ZIP size overflow")?;
        ensure!(
            unpacked <= MAX_UNPACKED_BYTES,
            "ZIP exceeds total expanded-size limit"
        );
        ensure!(
            entry.size() <= 16 * 1024 * 1024
                || entry.size() / entry.compressed_size().max(1) <= 2000,
            "ZIP entry has an excessive compression ratio"
        );
        let output = destination.join(&relative);
        if entry.is_dir() {
            ensure!(entry.size() == 0, "ZIP directory contains unexpected data");
            ensure_private_directory(&output)?;
            continue;
        }
        ensure!(
            kind != 0o040000,
            "ZIP entry has contradictory directory metadata"
        );
        if let Some(parent) = output.parent() {
            ensure_private_directory(parent)?;
        }
        let mut output_file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&output)
            .with_context(|| format!("create extracted file {}", relative.display()))?;
        let expected_size = entry.size();
        let count = std::io::copy(&mut entry.take(expected_size + 1), &mut output_file)
            .context("extract and verify ZIP entry CRC")?;
        ensure!(
            count == expected_size,
            "ZIP entry expanded length differs from its metadata"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(
                &output,
                fs::Permissions::from_mode(if mode & 0o111 != 0 { 0o700 } else { 0o600 }),
            )?;
        }
        output_file
            .sync_all()
            .context("flush extracted core file")?;
    }
    sync_directory(destination)?;
    Ok(())
}

/// Check the end record before ZipArchive allocates per-entry metadata. Both
/// classic ZIP and ZIP64 are supported; split/multi-disk archives are rejected.
fn check_zip_entry_count(file: &mut File) -> Result<u64> {
    let size = file.metadata()?.len();
    ensure!(
        (22..=MAX_DOWNLOAD_BYTES).contains(&size),
        "invalid or oversized ZIP file"
    );
    let tail_length = size.min(65_557) as usize;
    file.seek(SeekFrom::End(-(tail_length as i64)))?;
    let mut tail = vec![0u8; tail_length];
    file.read_exact(&mut tail)?;
    let position = (0..=tail.len() - 22)
        .rev()
        .find(|&position| {
            tail[position..position + 4] == *b"PK\x05\x06"
                && position
                    + 22
                    + u16::from_le_bytes([tail[position + 20], tail[position + 21]]) as usize
                    == tail.len()
        })
        .context("ZIP end record not found")?;
    let end = &tail[position..position + 22];
    let u16_at = |offset| u16::from_le_bytes([end[offset], end[offset + 1]]);
    ensure!(
        u16_at(4) == 0 && u16_at(6) == 0,
        "split ZIP archives are not supported"
    );
    ensure!(
        u16_at(8) == u16_at(10),
        "split ZIP archive entry counts are not supported"
    );
    let mut entries = u16_at(10) as u64;
    if entries == u16::MAX as u64 {
        let end_offset = size - tail_length as u64 + position as u64;
        ensure!(end_offset >= 20, "missing ZIP64 locator");
        file.seek(SeekFrom::Start(end_offset - 20))?;
        let mut locator = [0u8; 20];
        file.read_exact(&mut locator)?;
        ensure!(locator[..4] == *b"PK\x06\x07", "missing ZIP64 locator");
        ensure!(
            u32::from_le_bytes(locator[4..8].try_into()?) == 0
                && u32::from_le_bytes(locator[16..20].try_into()?) == 1,
            "split ZIP64 archives are not supported"
        );
        let offset = u64::from_le_bytes(locator[8..16].try_into()?);
        ensure!(
            offset
                .checked_add(56)
                .is_some_and(|end| end <= end_offset - 20),
            "invalid ZIP64 end offset"
        );
        file.seek(SeekFrom::Start(offset))?;
        let mut zip64 = [0u8; 56];
        file.read_exact(&mut zip64)?;
        ensure!(zip64[..4] == *b"PK\x06\x06", "invalid ZIP64 end record");
        ensure!(
            u32::from_le_bytes(zip64[16..20].try_into()?) == 0
                && u32::from_le_bytes(zip64[20..24].try_into()?) == 0,
            "split ZIP64 archives are not supported"
        );
        entries = u64::from_le_bytes(zip64[32..40].try_into()?);
        ensure!(
            entries == u64::from_le_bytes(zip64[24..32].try_into()?),
            "ZIP64 entry counts disagree"
        );
    }
    ensure!(
        entries > 0 && entries <= MAX_ARCHIVE_ENTRIES,
        "ZIP entry count exceeds limit or archive is empty"
    );
    Ok(entries)
}

fn hash_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let length = file.read(&mut buffer)?;
        if length == 0 {
            break;
        }
        hash.update(&buffer[..length]);
    }
    Ok(hex::encode(hash.finalize()))
}

fn read_bounded_file(path: &Path, maximum: usize) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        !is_link_or_reparse(&metadata) && metadata.is_file(),
        "expected a regular metadata file"
    );
    ensure!(
        metadata.len() <= maximum as u64,
        "metadata file exceeds size limit"
    );
    let mut bytes = Vec::new();
    File::open(path)?
        .take(maximum as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= maximum,
        "metadata file grew beyond size limit"
    );
    Ok(bytes)
}

fn is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_type().is_symlink() || metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn ensure_regular_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir() && !is_link_or_reparse(&metadata),
        "expected a regular core directory"
    );
    Ok(())
}

fn ensure_private_directory(path: &Path) -> Result<()> {
    if !path.exists() {
        fs::create_dir_all(path).context("create core directory")?;
    }
    ensure_regular_directory(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn atomic_write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path.parent().context("metadata path has no parent")?;
    let mut temporary = NamedTempFile::new_in(parent)?;
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    temporary.write_all(&bytes)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .map_err(|error| anyhow!(error.error))
        .context("atomically replace metadata")?;
    // The atomic replacement is the commit point. Returning an error after it
    // would make callers incorrectly restart the old core although the pointer
    // already names the new one. Directory durability is best-effort on filesystems
    // that do not support fsync; file data was flushed before the replacement.
    let _ = sync_directory(parent);
    Ok(())
}

fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use zip::write::SimpleFileOptions;

    const TARGET: &str = "x86_64-pc-windows-msvc";

    fn write_zip(path: &Path, entries: &[(&str, &[u8])]) {
        let mut writer = zip::ZipWriter::new(File::create(path).unwrap());
        let options =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for (name, bytes) in entries {
            writer.start_file(*name, options).unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap();
    }

    fn fixture_archive(temp: &TempDir, version: &str) -> PathBuf {
        let path = temp.path().join(format!("{version}.zip"));
        let manifest = serde_json::json!({
            "schema_version": 1, "version": version, "target": TARGET,
            "executable": "runtime/node.exe", "args": ["{core_dir}/orcad/server.js", "--port", "{port}"],
            "environment": {}, "health_path": "/health", "ui_path": "/"
        }).to_string();
        write_zip(
            &path,
            &[
                (MANIFEST_NAME, manifest.as_bytes()),
                ("runtime/node.exe", b"test runtime"),
                ("orcad/server.js", b"test server"),
            ],
        );
        path
    }

    #[test]
    fn rejects_cross_platform_zip_slip_and_windows_aliases() {
        for name in [
            "../outside",
            "a/../../outside",
            "/absolute",
            "C:/drive",
            "C:relative",
            "..\\outside",
            "a\\..\\outside",
            "a//b",
            "a/./b",
            "a/file:stream",
            "CON",
            "aux.txt",
            "LPT1.log",
            "com².txt",
            "a./file",
            "a /file",
            "nul\0.txt",
        ] {
            assert!(
                safe_relative_path(name, false).is_err(),
                "accepted {name:?}"
            );
        }
        assert_eq!(
            safe_relative_path("orcad/node_modules/package/file.js", false).unwrap(),
            PathBuf::from("orcad/node_modules/package/file.js")
        );
    }

    #[test]
    fn checksum_requires_exact_asset_filename() {
        let digest = "a".repeat(64);
        assert_eq!(
            parse_checksum(format!("{digest}  core.zip\n").as_bytes(), "core.zip").unwrap(),
            digest
        );
        assert!(parse_checksum(format!("{digest}  other.zip\n").as_bytes(), "core.zip").is_err());
        assert!(parse_checksum(format!("{digest}\n").as_bytes(), "core.zip").is_err());
        assert!(parse_checksum(
            format!("{digest}  core.zip\n{digest}  core.zip\n").as_bytes(),
            "core.zip"
        )
        .is_err());
    }

    #[test]
    fn release_selection_ignores_shell_prerelease_and_wrong_target() {
        let make = |tag: &str, target: &str, prerelease| GithubRelease {
            tag_name: tag.into(),
            draft: false,
            prerelease,
            assets: vec![
                ReleaseAsset {
                    name: archive_name("v1.4.264", target),
                    size: 100,
                },
                ReleaseAsset {
                    name: format!("{}.sha256", archive_name("v1.4.264", target)),
                    size: 100,
                },
            ],
        };
        assert!(compatible_release(make("v1.4.264", TARGET, false), TARGET)
            .unwrap()
            .is_none());
        assert!(
            compatible_release(make("core-v1.4.264", TARGET, true), TARGET)
                .unwrap()
                .is_none()
        );
        assert!(
            compatible_release(make("core-v1.4.264", "aarch64-apple-darwin", false), TARGET)
                .unwrap()
                .is_none()
        );
        assert!(
            compatible_release(make("core-v1.4.264", TARGET, false), TARGET)
                .unwrap()
                .is_some()
        );
        let mac = "aarch64-apple-darwin";
        assert!(
            compatible_release(make("core-macos-v1.4.264", mac, false), mac)
                .unwrap()
                .is_some()
        );
        assert!(
            compatible_release(make("core-macos-v1.4.264", TARGET, false), TARGET)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn extraction_rejects_symlinks_and_case_collisions() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("symlink.zip");
        let mut writer = zip::ZipWriter::new(File::create(&archive).unwrap());
        writer
            .add_symlink("escape", "../outside", SimpleFileOptions::default())
            .unwrap();
        writer.finish().unwrap();
        let output = temp.path().join("output");
        fs::create_dir(&output).unwrap();
        assert!(extract_archive(&archive, &output).is_err());
        write_zip(&archive, &[("A.txt", b"a"), ("a.txt", b"b")]);
        assert!(extract_archive(&archive, &output).is_err());
        assert!(!temp.path().join("outside").exists());
    }

    #[test]
    fn versioned_core_release_selects_platform_and_compares_packaging_revisions() {
        let release = |tag: &str, target: &str| {
            let (_, version) = tag.rsplit_once("-v").unwrap();
            let archive = format!("orca-core-v{version}-{target}.zip");
            GithubRelease {
                tag_name: tag.into(),
                draft: false,
                prerelease: false,
                assets: vec![
                    ReleaseAsset {
                        name: archive.clone(),
                        size: 100,
                    },
                    ReleaseAsset {
                        name: format!("{archive}.sha256"),
                        size: 100,
                    },
                ],
            }
        };
        let (key, update) =
            compatible_release(release("core-windows-v0.0.2-1.4.264", TARGET), TARGET)
                .unwrap()
                .unwrap();
        assert_eq!(update.display_version(), "0.0.2-1.4.264");
        assert!(key > core_version_key("v1.4.264", Some("0.0.1")).unwrap());
        assert!(key > core_version_key("v1.4.264", None).unwrap());
        assert!(key < core_version_key("v1.4.265", Some("0.0.1")).unwrap());
        assert!(
            compatible_release(release("core-macos-v0.0.2-1.4.264", TARGET), TARGET)
                .unwrap()
                .is_none()
        );
        assert!(
            compatible_release(release("core-windows-v0.0.0-1.4.264", TARGET), TARGET)
                .unwrap()
                .is_none()
        );
        let mac = "aarch64-apple-darwin";
        let (_, update_mac) = compatible_release(release("core-macos-v0.0.1-1.4.265", mac), mac)
            .unwrap()
            .unwrap();
        assert_eq!(update_mac.display_version(), "0.0.1-1.4.265");
        let temp = TempDir::new().unwrap();
        let archive = fixture_archive(&temp, "v1.4.264");
        let digest = hash_file(&archive).unwrap();
        fs::create_dir(temp.path().join("staging")).unwrap();
        fs::create_dir(temp.path().join("versions")).unwrap();
        let error = prepare_archive(
            temp.path(),
            &archive,
            &update.version,
            TARGET,
            &digest,
            Some("0.0.2"),
        )
        .unwrap_err();
        assert!(error.to_string().contains("packaging revision differs"));
        let legacy = prepare_archive(
            temp.path(),
            &archive,
            &update.version,
            TARGET,
            &digest,
            None,
        )
        .unwrap();
        assert_eq!(legacy.manifest.display_version(), "0.0.0-1.4.264");
        assert!(update.is_newer_than(&legacy.manifest).unwrap());
        let mut current = legacy.manifest;
        current.package_revision = Some("0.0.2".into());
        assert!(!update.is_newer_than(&current).unwrap());
    }

    #[test]
    fn extraction_rejects_traversal_and_declared_zip_bombs() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("unsafe.zip");
        let output = temp.path().join("output");
        fs::create_dir(&output).unwrap();
        write_zip(&archive, &[("../outside", b"escape")]);
        assert!(extract_archive(&archive, &output).is_err());
        assert!(!temp.path().join("outside").exists());

        write_zip(&archive, &[("huge", b"small")]);
        let mut bytes = fs::read(&archive).unwrap();
        let central = bytes
            .windows(4)
            .position(|bytes| bytes == b"PK\x01\x02")
            .unwrap();
        let oversized = ((MAX_FILE_BYTES + 1) as u32).to_le_bytes();
        bytes[22..26].copy_from_slice(&oversized);
        bytes[central + 24..central + 28].copy_from_slice(&oversized);
        fs::write(&archive, bytes).unwrap();
        assert!(extract_archive(&archive, &output).is_err());
        assert!(!output.join("huge").exists());

        // A forged ZIP64 footer can request millions of metadata allocations
        // without a large download. Reject its count before ZipArchive::new.
        let mut bytes = vec![0u8; 98];
        bytes[..4].copy_from_slice(b"PK\x06\x06");
        bytes[4..12].copy_from_slice(&44u64.to_le_bytes());
        bytes[24..32].copy_from_slice(&(MAX_ARCHIVE_ENTRIES + 1).to_le_bytes());
        bytes[32..40].copy_from_slice(&(MAX_ARCHIVE_ENTRIES + 1).to_le_bytes());
        bytes[56..60].copy_from_slice(b"PK\x06\x07");
        bytes[72..76].copy_from_slice(&1u32.to_le_bytes());
        bytes[76..80].copy_from_slice(b"PK\x05\x06");
        bytes[84..88].copy_from_slice(&[255, 255, 255, 255]);
        fs::write(&archive, bytes).unwrap();
        assert!(check_zip_entry_count(&mut File::open(archive).unwrap()).is_err());
    }

    #[tokio::test]
    async fn failed_or_abandoned_candidate_does_not_change_active_core() {
        let temp = tempfile::tempdir().unwrap();
        let updater = CoreUpdater::new(temp.path().join("core"), None, TARGET.into()).unwrap();
        let first = fixture_archive(&temp, "v1.4.263");
        let second = fixture_archive(&temp, "v1.4.264");
        {
            let session = updater.begin_update().await;
            let installed = session
                .prepare_local_archive(&first, "v1.4.263", &hash_file(&first).unwrap())
                .await
                .unwrap();
            session.commit(&installed).unwrap();
        }
        let active_before = fs::read(updater.root.join(ACTIVE_NAME)).unwrap();
        {
            let session = updater.begin_update().await;
            let _unhealthy = session
                .prepare_local_archive(&second, "v1.4.264", &hash_file(&second).unwrap())
                .await
                .unwrap();
            // Simulate a failed health check or shell crash before commit.
        }
        assert_eq!(
            fs::read(updater.root.join(ACTIVE_NAME)).unwrap(),
            active_before
        );
        assert_eq!(
            updater.load_active().unwrap().unwrap().manifest.version,
            "v1.4.263"
        );
        let session = updater.begin_update().await;
        let healthy = session
            .prepare_local_archive(&second, "v1.4.264", &hash_file(&second).unwrap())
            .await
            .unwrap();
        session.commit(&healthy).unwrap();
        assert_eq!(
            updater.load_active().unwrap().unwrap().manifest.version,
            "v1.4.264"
        );
        assert_eq!(
            updater.load_previous().unwrap().unwrap().manifest.version,
            "v1.4.263"
        );
    }

    #[tokio::test]
    async fn corrupt_or_mismatched_package_never_activates() {
        let temp = tempfile::tempdir().unwrap();
        let updater = CoreUpdater::new(temp.path().join("core"), None, TARGET.into()).unwrap();
        let archive = fixture_archive(&temp, "v1.4.264");
        let session = updater.begin_update().await;
        assert!(session
            .prepare_local_archive(&archive, "v1.4.264", &"0".repeat(64))
            .await
            .is_err());
        assert!(session
            .prepare_local_archive(&archive, "v1.4.265", &hash_file(&archive).unwrap())
            .await
            .is_err());
        assert!(updater.load_active().unwrap().is_none());
    }

    #[tokio::test]
    async fn update_session_serializes_entire_activation_window() {
        let temp = tempfile::tempdir().unwrap();
        let updater = CoreUpdater::new(temp.path().join("core"), None, TARGET.into()).unwrap();
        let session = updater.begin_update().await;
        assert!(
            tokio::time::timeout(Duration::from_millis(10), updater.begin_update())
                .await
                .is_err()
        );
        drop(session);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), updater.begin_update())
                .await
                .is_ok()
        );
    }
}
