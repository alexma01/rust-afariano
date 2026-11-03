//! Runner configuration (`config.toml`).

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};

pub const CONFIG_ENV: &str = "RUST_AFARIANO_RUNNER_CONFIG";
const SYSTEM_CONFIG: &str = "/etc/rust-afariano-runner/config.toml";
const SYSTEM_BUILDS_DIR: &str = "/var/lib/rust-afariano-runner/builds";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Executor {
    Docker,
    Shell,
}

impl Executor {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Docker => "docker",
            Self::Shell => "shell",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PullPolicy {
    #[default]
    IfNotPresent,
    Always,
    Never,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Server base URL, e.g. `https://git.example.com`.
    pub url: String,
    /// Runner token (`rfr_…`).
    pub token: String,
    #[serde(default = "default_name")]
    pub name: String,
    #[serde(default = "default_executor")]
    pub executor: Executor,
    /// Jobs run at the same time.
    #[serde(default = "default_concurrent")]
    pub concurrent: usize,
    /// Where checkouts live; default `~/.rust-afariano-runner/builds` (root: `/var/lib/rust-afariano-runner/builds`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub builds_dir: Option<PathBuf>,
    /// On SIGTERM, running jobs get this long to finish before they are canceled.
    #[serde(default = "default_shutdown_timeout")]
    pub shutdown_timeout_secs: u64,
    #[serde(default)]
    pub docker: DockerConfig,
    #[serde(default)]
    pub shell: ShellConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DockerConfig {
    #[serde(default = "default_image")]
    pub default_image: String,
    #[serde(default)]
    pub privileged: bool,
    /// `"/path"` = per-job volume shared by job and services; `"/host:/ctr[:ro]"` = bind mount.
    #[serde(default = "default_volumes")]
    pub volumes: Vec<String>,
    #[serde(default)]
    pub pull_policy: PullPolicy,
    /// Small image with `nc`, used to wait for service ports.
    #[serde(default = "default_helper_image")]
    pub helper_image: String,
    /// Extra root CA (PEM, absolute path on the runner host) trusted inside every
    /// job and service container, e.g. the CA of a TLS-inspecting proxy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_ca_file: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ShellConfig {
    /// Shell for the shell executor; default `bash` if on PATH, else `sh`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
}

fn default_name() -> String {
    crate::hostname()
}
fn default_executor() -> Executor {
    Executor::Docker
}
fn default_concurrent() -> usize {
    1
}
fn default_shutdown_timeout() -> u64 {
    600
}
fn default_image() -> String {
    "alpine:3".into()
}
fn default_volumes() -> Vec<String> {
    vec!["/certs/client".into()]
}
fn default_helper_image() -> String {
    "busybox:1.37".into()
}

impl Default for DockerConfig {
    fn default() -> Self {
        Self {
            default_image: default_image(),
            privileged: false,
            volumes: default_volumes(),
            pull_policy: PullPolicy::default(),
            helper_image: default_helper_image(),
            extra_ca_file: None,
        }
    }
}

impl Config {
    /// A config for `exec-local` when no file exists.
    pub fn local(executor: Executor) -> Self {
        Self {
            url: String::new(),
            token: String::new(),
            name: default_name(),
            executor,
            concurrent: 1,
            builds_dir: None,
            shutdown_timeout_secs: default_shutdown_timeout(),
            docker: DockerConfig::default(),
            shell: ShellConfig::default(),
        }
    }

    pub fn builds_dir(&self) -> PathBuf {
        if let Some(dir) = &self.builds_dir {
            return dir.clone();
        }
        if is_root() {
            PathBuf::from(SYSTEM_BUILDS_DIR)
        } else {
            home_dir().join(".rust-afariano-runner").join("builds")
        }
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut cfg: Self = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        cfg.url = cfg.url.trim_end_matches('/').to_string();
        if cfg.concurrent == 0 {
            bail!("{}: concurrent must be at least 1", path.display());
        }
        if let Some(ca) = &cfg.docker.extra_ca_file
            && (!ca.is_absolute() || !ca.is_file())
        {
            bail!("{}: docker.extra_ca_file {} is not an absolute path to a file", path.display(), ca.display());
        }
        Ok(cfg)
    }

    /// Write atomically with mode 0600 (the file holds the runner token).
    pub fn save(&self, path: &Path) -> Result<()> {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        if let Some(dir) = path.parent() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
                .with_context(|| format!("creating {}", dir.display()))?;
        }
        let text = toml::to_string_pretty(self)?;
        let tmp = path.with_extension("toml.tmp");
        let _ = std::fs::remove_file(&tmp);
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
}

/// `$RUST_AFARIANO_RUNNER_CONFIG`, else the system path as root, else `~/.config/rust-afariano-runner/config.toml`.
pub fn config_path() -> PathBuf {
    select_config_path(std::env::var_os(CONFIG_ENV), is_root(), &home_dir())
}

pub fn select_config_path(env: Option<OsString>, root: bool, home: &Path) -> PathBuf {
    match env {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ if root => PathBuf::from(SYSTEM_CONFIG),
        _ => home.join(".config").join("rust-afariano-runner").join("config.toml"),
    }
}

pub fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_path_prefers_env() {
        let home = Path::new("/home/u");
        assert_eq!(select_config_path(Some("/x/c.toml".into()), true, home), PathBuf::from("/x/c.toml"));
    }

    #[test]
    fn config_path_root_uses_etc() {
        let home = Path::new("/root");
        assert_eq!(select_config_path(None, true, home), PathBuf::from("/etc/rust-afariano-runner/config.toml"));
        // An empty env var counts as unset.
        assert_eq!(
            select_config_path(Some("".into()), true, home),
            PathBuf::from("/etc/rust-afariano-runner/config.toml")
        );
    }

    #[test]
    fn config_path_user_uses_home() {
        let home = Path::new("/Users/runner");
        assert_eq!(
            select_config_path(None, false, home),
            PathBuf::from("/Users/runner/.config/rust-afariano-runner/config.toml")
        );
    }

    #[test]
    fn config_defaults_and_roundtrip() {
        let cfg: Config = toml::from_str(
            r#"
url = "https://pipe.example.com/"
token = "rfr_x"
"#,
        )
        .unwrap();
        assert_eq!(cfg.executor, Executor::Docker);
        assert_eq!(cfg.concurrent, 1);
        assert_eq!(cfg.docker.default_image, "alpine:3");
        assert_eq!(cfg.docker.volumes, vec!["/certs/client".to_string()]);
        assert_eq!(cfg.docker.pull_policy, PullPolicy::IfNotPresent);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("config.toml");
        cfg.save(&path).unwrap();
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let back = Config::load(&path).unwrap();
        assert_eq!(back.url, "https://pipe.example.com");
        assert_eq!(back.token, "rfr_x");
    }

    #[test]
    fn pull_policy_kebab_case() {
        let cfg: Config = toml::from_str(
            "url = \"u\"\ntoken = \"t\"\n[docker]\npull_policy = \"if-not-present\"\nprivileged = true\n",
        )
        .unwrap();
        assert_eq!(cfg.docker.pull_policy, PullPolicy::IfNotPresent);
        assert!(cfg.docker.privileged);
        assert!(cfg.docker.extra_ca_file.is_none());
    }

    #[test]
    fn extra_ca_file_must_exist() {
        let dir = tempfile::tempdir().unwrap();
        let ca = dir.path().join("ca.crt");
        let path = dir.path().join("config.toml");
        let write = |ca: &str| {
            std::fs::write(&path, format!("url = \"u\"\ntoken = \"t\"\n[docker]\nextra_ca_file = \"{ca}\"\n")).unwrap()
        };

        write(ca.to_str().unwrap());
        assert!(Config::load(&path).is_err(), "missing file");
        std::fs::write(&ca, "-----BEGIN CERTIFICATE-----\n").unwrap();
        assert_eq!(Config::load(&path).unwrap().docker.extra_ca_file.as_deref(), Some(ca.as_path()));
        write("ca.crt");
        assert!(Config::load(&path).is_err(), "relative path");
    }
}
