//! Local settings and translation from a verified package to a native launch.
use crate::{
    process::{CoreManager, LaunchSpec, ReadinessMode},
    updater::InstalledCore,
};
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

const UPSTREAM_DEFAULT_PORT: u16 = 6768;
#[cfg(target_os = "macos")]
const DEFAULT_PORT: u16 = 16768;
#[cfg(not(target_os = "macos"))]
const DEFAULT_PORT: u16 = UPSTREAM_DEFAULT_PORT;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub port: u16,
    pub core_directory: Option<PathBuf>,
    pub update_repository: Option<String>,
    pub download_proxy: Option<String>,
    pub start_core_on_launch: bool,
    pub open_window_on_start: bool,
    pub autostart: bool,
    pub global_shortcut: String,
    pub notifications: bool,
    pub environment: BTreeMap<String, String>,
    pub startup_timeout_secs: u64,
    pub shutdown_timeout_secs: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            core_directory: None,
            update_repository: option_env!("ORCA_TAURI_REPOSITORY")
                .filter(|value| !value.is_empty())
                .map(str::to_owned),
            download_proxy: None,
            start_core_on_launch: true,
            open_window_on_start: false,
            autostart: false,
            global_shortcut: "CommandOrControl+Shift+O".into(),
            notifications: true,
            environment: BTreeMap::new(),
            startup_timeout_secs: 60,
            shutdown_timeout_secs: 8,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub root: PathBuf,
    pub config: PathBuf,
    pub core: PathBuf,
    pub runtime_data: PathBuf,
    pub logs: PathBuf,
    pub webview_data: PathBuf,
}

impl AppPaths {
    pub fn discover() -> Result<PathBuf> {
        let root = match std::env::var_os("ORCA_TAURI_HOME") {
            Some(path) => PathBuf::from(path),
            None => dirs::home_dir()
                .context("cannot find the user home directory; set ORCA_TAURI_HOME")?
                .join(".orca-tauri"),
        };
        ensure!(
            root.is_absolute(),
            "ORCA_TAURI_HOME must be an absolute path"
        );
        Ok(root)
    }

    pub fn initialize(root: PathBuf, config: &Config) -> Result<Self> {
        config.validate()?;
        let paths = Self {
            config: root.join("config.json"),
            core: config
                .core_directory
                .clone()
                .unwrap_or_else(|| root.join("core")),
            runtime_data: root.join("orca-data"),
            logs: root.join("logs"),
            webview_data: root.join("webview"),
            root,
        };
        for directory in [
            &paths.root,
            &paths.core,
            &paths.runtime_data,
            &paths.logs,
            &paths.webview_data,
        ] {
            private_directory(directory)?;
        }
        Ok(paths)
    }
}

impl Config {
    pub fn load_or_create(root: &Path) -> Result<Self> {
        private_directory(root)?;
        let path = root.join("config.json");
        if !path.exists() {
            let config = Self::default();
            config.save(&path)?;
            return Ok(config);
        }
        let mut bytes = Vec::new();
        File::open(&path)?
            .take(64 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 64 * 1024, "config.json exceeds 64 KiB");
        let config: Self = serde_json::from_slice(&bytes).context("parse config.json")?;
        config.validate()?;
        Ok(config)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        let parent = path.parent().context("config path has no parent")?;
        private_directory(parent)?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        serde_json::to_writer_pretty(&mut temporary, self)?;
        temporary.write_all(b"\n")?;
        temporary.as_file().sync_all()?;
        temporary.persist(path).map_err(|error| error.error)?;
        #[cfg(unix)]
        let _ = File::open(parent).and_then(|directory| directory.sync_all());
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(self.port >= 1024, "port must be between 1024 and 65535");
        ensure!(
            (5..=300).contains(&self.startup_timeout_secs),
            "startup timeout must be 5..300 seconds"
        );
        ensure!(
            (1..=30).contains(&self.shutdown_timeout_secs),
            "shutdown timeout must be 1..30 seconds"
        );
        ensure!(
            !self.global_shortcut.trim().is_empty(),
            "global_shortcut must not be empty"
        );
        if let Some(path) = &self.core_directory {
            ensure!(
                path.is_absolute(),
                "core_directory must be an absolute path"
            );
        }
        if let Some(repository) = &self.update_repository {
            let parts: Vec<_> = repository.split('/').collect();
            ensure!(
                parts.len() == 2
                    && parts.iter().all(|part| {
                        !part.is_empty()
                            && *part != "."
                            && *part != ".."
                            && part
                                .bytes()
                                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
                    }),
                "update_repository must be an owner/repository name"
            );
        }
        if let Some(proxy) = &self.download_proxy {
            let proxy = url::Url::parse(proxy).context("invalid download_proxy")?;
            ensure!(
                matches!(proxy.scheme(), "http" | "https") && proxy.host_str().is_some(),
                "download_proxy must use http:// or https://"
            );
        }
        for (key, value) in &self.environment {
            ensure!(
                !key.is_empty() && !key.contains(['=', '\0']) && !value.contains('\0'),
                "invalid environment entry"
            );
            ensure!(
                !matches!(
                    key.to_ascii_uppercase().as_str(),
                    "ORCA_USER_DATA" | "ORCA_VERSION"
                ),
                "ORCA_USER_DATA and ORCA_VERSION are managed by the verified core manifest"
            );
        }
        Ok(())
    }

    pub fn launch_spec(&self, paths: &AppPaths, core: &InstalledCore) -> Result<LaunchSpec> {
        let directory = core
            .directory
            .canonicalize()
            .context("resolve installed core directory")?;
        let program = directory
            .join(&core.manifest.executable)
            .canonicalize()
            .context("resolve installed core executable")?;
        let cwd = directory
            .join(&core.manifest.working_directory)
            .canonicalize()
            .context("resolve core working directory")?;
        ensure!(
            program.starts_with(&directory) && program.is_file(),
            "core executable escaped its package"
        );
        ensure!(
            cwd.starts_with(&directory) && cwd.is_dir(),
            "core working directory escaped its package"
        );
        let port = self.port.to_string();
        let core_text = directory
            .to_str()
            .context("core path is not valid Unicode")?;
        let data_text = paths
            .runtime_data
            .to_str()
            .context("runtime data path is not valid Unicode")?;
        let variables = BTreeMap::from([
            ("host", "127.0.0.1"),
            ("port", port.as_str()),
            ("core_dir", core_text),
            ("data_dir", data_text),
        ]);
        let args = core
            .manifest
            .args
            .iter()
            .map(|arg| expand(arg, &variables))
            .collect::<Result<Vec<_>>>()?;
        let mut env = core
            .manifest
            .environment
            .iter()
            .map(|(key, value)| Ok((key.clone(), expand(value, &variables)?)))
            .collect::<Result<BTreeMap<_, _>>>()?;
        env.extend(self.environment.clone());
        // Even when the user routes model traffic through a proxy, local runtime
        // requests must stay direct. Windows env keys are case-insensitive.
        let existing_no_proxy = env
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case("NO_PROXY"))
            .map(|(_, value)| value.clone())
            .or_else(|| std::env::var("NO_PROXY").ok())
            .or_else(|| std::env::var("no_proxy").ok())
            .unwrap_or_default();
        env.retain(|key, _| !key.eq_ignore_ascii_case("NO_PROXY"));
        let no_proxy = merge_no_proxy(&existing_no_proxy);
        env.insert("NO_PROXY".into(), no_proxy.clone());
        #[cfg(unix)]
        env.insert("no_proxy".into(), no_proxy);
        Ok(LaunchSpec {
            program,
            args,
            cwd,
            env,
            health_url: local_url(self.port, &core.manifest.health_path)?,
            port: self.port,
            startup_timeout: Duration::from_secs(self.startup_timeout_secs),
            shutdown_timeout: Duration::from_secs(self.shutdown_timeout_secs),
            shutdown: None, // No undocumented or unauthenticated shutdown endpoint.
            log_dir: paths.logs.clone(),
            version: core
                .manifest
                .runtime_slot_version()
                .context("core manifest must pin the ORCA_VERSION runtime slot")?
                .to_owned(),
            readiness: ReadinessMode::OrcaJson,
        })
    }

    pub async fn start_core(
        &self,
        manager: &CoreManager,
        paths: &AppPaths,
        core: &InstalledCore,
    ) -> Result<()> {
        let result = manager.start(self.launch_spec(paths, core)?).await;
        #[cfg(target_os = "macos")]
        if self.port == UPSTREAM_DEFAULT_PORT
            && result
                .as_ref()
                .is_err_and(|error| error.is::<crate::process::PortOccupied>())
        {
            let alternate = Self {
                port: DEFAULT_PORT,
                ..self.clone()
            };
            manager
                .start(alternate.launch_spec(paths, core)?)
                .await
                .context("The legacy Orca port 6768 is occupied; startup on 16768 also failed")?;
            return Ok(());
        }
        result.map(|_| ())
    }
}

pub fn local_url(port: u16, path: &str) -> Result<String> {
    ensure!(
        path.starts_with('/') && !path.starts_with("//") && !path.contains(['\\', '#', '?']),
        "core URL path must be a local absolute path without query or fragment"
    );
    let result = url::Url::parse(&format!("http://127.0.0.1:{port}{path}"))?;
    ensure!(
        result.host_str() == Some("127.0.0.1") && result.port() == Some(port),
        "invalid local core URL"
    );
    Ok(result.into())
}

fn expand(template: &str, variables: &BTreeMap<&str, &str>) -> Result<String> {
    let mut result = String::new();
    let mut remaining = template;
    while let Some(start) = remaining.find('{') {
        result.push_str(&remaining[..start]);
        let tail = &remaining[start + 1..];
        let end = tail
            .find('}')
            .context("unclosed core manifest placeholder")?;
        let name = &tail[..end];
        result.push_str(
            variables
                .get(name)
                .with_context(|| format!("unknown core manifest placeholder: {name}"))?,
        );
        remaining = &tail[end + 1..];
    }
    if remaining.contains('}') {
        bail!("unmatched core manifest placeholder bracket");
    }
    result.push_str(remaining);
    Ok(result)
}

fn merge_no_proxy(existing: &str) -> String {
    let mut entries: Vec<&str> = existing
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .collect();
    for local in ["localhost", "127.0.0.1", "::1", "[::1]"] {
        if !entries
            .iter()
            .any(|entry| entry.eq_ignore_ascii_case(local))
        {
            entries.push(local);
        }
    }
    entries.join(",")
}

fn private_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn localhost_bypass_preserves_existing_proxy_exclusions() {
        let result = merge_no_proxy("example.internal, localhost");
        assert_eq!(result, "example.internal,localhost,127.0.0.1,::1,[::1]");
    }

    #[test]
    fn placeholders_do_not_expand_in_inserted_path_values() {
        let vars = BTreeMap::from([("core_dir", "C:/Users/{port}/core"), ("port", "6768")]);
        assert_eq!(
            expand("{core_dir}/entry.js", &vars).unwrap(),
            "C:/Users/{port}/core/entry.js"
        );
        assert!(expand("{missing}", &vars).is_err());
    }

    #[test]
    fn config_round_trip_does_not_replace_invalid_user_data() {
        let directory = tempfile::tempdir().unwrap();
        let config = Config::load_or_create(directory.path()).unwrap();
        assert_eq!(
            config.port,
            if cfg!(target_os = "macos") {
                16768
            } else {
                6768
            }
        );
        fs::write(directory.path().join("config.json"), b"{bad json}").unwrap();
        assert!(Config::load_or_create(directory.path()).is_err());
        assert_eq!(
            fs::read(directory.path().join("config.json")).unwrap(),
            b"{bad json}"
        );
    }

    #[test]
    fn url_cannot_change_origin() {
        assert!(local_url(6768, "//evil.example/web-index.html").is_err());
        assert!(local_url(6768, "/\\evil.example/").is_err());
        assert_eq!(
            local_url(6768, "/web-index.html").unwrap(),
            "http://127.0.0.1:6768/web-index.html"
        );
    }

    #[test]
    fn product_release_and_runtime_slot_versions_are_not_confused() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().join("payload");
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("node.exe"), b"test-placeholder").unwrap();
        let config = Config::default();
        let paths = AppPaths::initialize(temporary.path().join("user-data"), &config).unwrap();
        let core = InstalledCore {
            directory,
            archive_sha256: "0".repeat(64),
            manifest: crate::updater::CoreManifest {
                schema_version: 1,
                version: "v1.4.264".into(),
                package_revision: None,
                target: env!("ORCA_TAURI_TARGET").into(),
                executable: "node.exe".into(),
                args: vec!["--bind".into(), "{host}".into()],
                working_directory: ".".into(),
                environment: BTreeMap::from([
                    ("ORCA_VERSION".into(), "0.1.0+0123456789ab".into()),
                    ("ORCA_USER_DATA".into(), "{data_dir}".into()),
                ]),
                health_path: "/web-index.html".into(),
                ui_path: "/web-index.html".into(),
            },
        };
        let spec = config.launch_spec(&paths, &core).unwrap();
        assert_eq!(core.manifest.version, "v1.4.264");
        assert_eq!(
            core.manifest.runtime_slot_version(),
            Some("0.1.0+0123456789ab")
        );
        assert_eq!(spec.version, "0.1.0+0123456789ab");
        assert_eq!(spec.env["ORCA_VERSION"], spec.version);
        assert_eq!(spec.args, ["--bind", "127.0.0.1"]);
        assert_eq!(
            spec.health_url,
            format!("http://127.0.0.1:{}/web-index.html", config.port)
        );
    }
}
