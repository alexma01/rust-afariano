//! Thin wrapper around the `docker` CLI.

use std::path::Path;

use anyhow::{Result, bail};
use tokio::process::Command;

use crate::config::PullPolicy;
use crate::joblog::JobLog;
use crate::proc;

#[derive(Clone)]
pub struct Docker {
    bin: String,
}

impl Default for Docker {
    fn default() -> Self {
        Self { bin: std::env::var("RUST_AFARIANO_DOCKER").unwrap_or_else(|_| "docker".into()) }
    }
}

impl Docker {
    pub fn cmd(&self) -> Command {
        Command::new(&self.bin)
    }

    pub async fn version(&self) -> Result<String> {
        let mut c = self.cmd();
        c.args(["version", "--format", "{{.Server.Version}}"]);
        Ok(proc::capture(c).await?.trim().to_string())
    }

    pub async fn remove_container(&self, name: &str) {
        let mut c = self.cmd();
        c.args(["rm", "-f", "-v", name]);
        let _ = proc::capture(c).await;
    }

    /// Remove every container labelled with this job id.
    pub async fn remove_job_containers(&self, job_label: &str) {
        let mut c = self.cmd();
        c.args(["ps", "-aq", "--filter", &format!("label={job_label}")]);
        if let Ok(ids) = proc::capture(c).await {
            let ids: Vec<&str> = ids.split_whitespace().collect();
            if !ids.is_empty() {
                let mut c = self.cmd();
                c.args(["rm", "-f", "-v"]).args(&ids);
                let _ = proc::capture(c).await;
            }
        }
    }

    pub async fn image_exists(&self, image: &str) -> bool {
        let mut c = self.cmd();
        c.args(["image", "inspect", "--format", "{{.Id}}", image]);
        proc::capture(c).await.is_ok()
    }

    pub async fn ensure_image(&self, image: &str, policy: PullPolicy, log: &JobLog) -> Result<()> {
        let pull = match policy {
            PullPolicy::Always => true,
            PullPolicy::IfNotPresent => !self.image_exists(image).await,
            PullPolicy::Never => {
                if !self.image_exists(image).await {
                    bail!("image {image} is not present and pull_policy is never");
                }
                false
            }
        };
        if pull {
            log.line(&format!("Pulling docker image {image} ..."));
            let mut c = self.cmd();
            c.args(["pull", "--quiet", image]);
            match proc::capture(c).await {
                Ok(out) => log.line(&format!("Using docker image {}", out.trim())),
                Err(e) => bail!("pulling {image}: {e:#}"),
            }
        } else {
            log.line(&format!("Using locally found image {image}"));
        }
        Ok(())
    }

    /// TCP ports the image exposes.
    pub async fn exposed_ports(&self, image: &str) -> Vec<u16> {
        let mut c = self.cmd();
        c.args(["image", "inspect", "--format", "{{json .Config.ExposedPorts}}", image]);
        let Ok(out) = proc::capture(c).await else {
            return Vec::new();
        };
        parse_exposed_ports(&out)
    }

    pub async fn create_network(&self, name: &str, job_label: &str) -> Result<()> {
        let mut c = self.cmd();
        c.args(["network", "create", "--label", job_label, name]);
        proc::capture(c).await?;
        Ok(())
    }

    pub async fn remove_network(&self, name: &str) {
        let mut c = self.cmd();
        c.args(["network", "rm", name]);
        let _ = proc::capture(c).await;
    }

    pub async fn remove_volume(&self, name: &str) {
        let mut c = self.cmd();
        c.args(["volume", "rm", "-f", name]);
        let _ = proc::capture(c).await;
    }

    pub async fn is_running(&self, name: &str) -> bool {
        let mut c = self.cmd();
        c.args(["inspect", "--format", "{{.State.Running}}", name]);
        matches!(proc::capture(c).await, Ok(s) if s.trim() == "true")
    }

    pub async fn logs_tail(&self, name: &str, lines: u32) -> String {
        let mut c = self.cmd();
        c.args(["logs", "--tail", &lines.to_string(), name]);
        c.stdin(std::process::Stdio::null()).kill_on_drop(true);
        match c.output().await {
            Ok(out) => {
                let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
                s.push_str(&String::from_utf8_lossy(&out.stderr));
                s
            }
            Err(e) => format!("(docker logs failed: {e})"),
        }
    }
}

pub fn parse_exposed_ports(json: &str) -> Vec<u16> {
    let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(json.trim()) else {
        return Vec::new();
    };
    let mut ports: Vec<u16> = map
        .keys()
        .filter_map(|k| {
            let (port, proto) = k.split_once('/').unwrap_or((k.as_str(), "tcp"));
            (proto == "tcp").then(|| port.parse().ok()).flatten()
        })
        .collect();
    ports.sort_unstable();
    ports.dedup();
    ports
}

/// Network aliases GitLab derives from a service image name:
/// `docker:29-dind` → `docker`; `reg.io/a/b:1` → `reg.io__a__b`, `reg.io-a-b`.
pub fn derive_aliases(image: &str) -> Vec<String> {
    let no_digest = image.split('@').next().unwrap_or(image);
    // A tag colon comes after the last slash (a registry port colon does not).
    let last_slash = no_digest.rfind('/').map(|i| i + 1).unwrap_or(0);
    let name = match no_digest[last_slash..].rfind(':') {
        Some(i) => &no_digest[..last_slash + i],
        None => no_digest,
    };
    let mut out = Vec::new();
    for candidate in [name.replace('/', "__"), name.replace('/', "-")] {
        let clean = sanitize_alias(&candidate);
        if !clean.is_empty() && !out.contains(&clean) {
            out.push(clean);
        }
    }
    out
}

fn sanitize_alias(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' { c } else { '-' }).collect()
}

/// Split a user `alias:` value (comma or space separated).
pub fn split_aliases(s: &str) -> Vec<String> {
    s.split(|c: char| c == ',' || c.is_whitespace()).filter(|a| !a.is_empty()).map(str::to_string).collect()
}

/// Environment names the `docker` CLI reads itself. Job variables with these
/// names go through `--env-file`, so they cannot redirect our own CLI.
pub fn is_cli_reserved(key: &str) -> bool {
    key.starts_with("DOCKER_")
        || key.starts_with("BUILDX_")
        || matches!(
            key,
            "PATH"
                | "HOME"
                | "TMPDIR"
                | "XDG_CONFIG_HOME"
                | "XDG_RUNTIME_DIR"
                | "HTTP_PROXY"
                | "HTTPS_PROXY"
                | "NO_PROXY"
                | "ALL_PROXY"
                | "http_proxy"
                | "https_proxy"
                | "no_proxy"
                | "all_proxy"
                | "SSL_CERT_FILE"
                | "SSL_CERT_DIR"
        )
}

/// Arguments and CLI-process environment that pass `vars` into a container
/// without putting values in argv: `-e KEY` + value in the CLI env, or
/// `--env-file` for names the CLI itself reads.
pub struct EnvArgs {
    pub args: Vec<String>,
    pub cli_env: Vec<(String, String)>,
}

pub fn env_args(vars: &[(String, String)], env_file: &Path) -> Result<EnvArgs> {
    let mut args = Vec::new();
    let mut cli_env = Vec::new();
    let mut file_lines = String::new();
    for (k, v) in vars {
        if is_cli_reserved(k) {
            if v.contains('\n') || v.contains('\r') {
                bail!("variable {k} is multi-line and its name is reserved by the docker CLI; use a File variable");
            }
            file_lines.push_str(&format!("{k}={v}\n"));
        } else {
            args.push("-e".to_string());
            args.push(k.clone());
            cli_env.push((k.clone(), v.clone()));
        }
    }
    if !file_lines.is_empty() {
        crate::job::write_private(env_file, file_lines.as_bytes())?;
        args.push("--env-file".to_string());
        args.push(env_file.display().to_string());
    }
    Ok(EnvArgs { args, cli_env })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_from_image_names() {
        assert_eq!(derive_aliases("docker:29-dind"), vec!["docker"]);
        assert_eq!(derive_aliases("postgres"), vec!["postgres"]);
        assert_eq!(derive_aliases("postgres:16-alpine"), vec!["postgres"]);
        assert_eq!(derive_aliases("tutum/wordpress:latest"), vec!["tutum__wordpress", "tutum-wordpress"]);
        assert_eq!(derive_aliases("reg.io/a/b:1"), vec!["reg.io__a__b", "reg.io-a-b"]);
        // Registry port is not a tag.
        assert_eq!(derive_aliases("reg.io:5000/a/b"), vec!["reg.io-5000__a__b", "reg.io-5000-a-b"]);
        assert_eq!(derive_aliases("redis@sha256:abcdef"), vec!["redis"]);
    }

    #[test]
    fn split_alias_lists() {
        assert_eq!(split_aliases("db, pg  postgres"), vec!["db", "pg", "postgres"]);
        assert!(split_aliases("").is_empty());
    }

    #[test]
    fn exposed_ports_parse() {
        assert_eq!(parse_exposed_ports(r#"{"2375/tcp":{},"2376/tcp":{},"53/udp":{}}"#), vec![2375, 2376]);
        assert!(parse_exposed_ports("null").is_empty());
    }

    #[test]
    fn env_args_route_reserved_names_to_file() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("env");
        let vars = vec![
            ("A".to_string(), "multi\nline".to_string()),
            ("DOCKER_HOST".to_string(), "tcp://docker:2376".to_string()),
        ];
        let e = env_args(&vars, &f).unwrap();
        assert_eq!(e.args, vec!["-e", "A", "--env-file", f.to_str().unwrap()]);
        assert_eq!(e.cli_env, vec![("A".to_string(), "multi\nline".to_string())]);
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "DOCKER_HOST=tcp://docker:2376\n");
        // Values never appear in argv.
        assert!(!e.args.iter().any(|a| a.contains("multi")));
    }

    #[test]
    fn env_args_reject_multiline_reserved() {
        let dir = tempfile::tempdir().unwrap();
        let vars = vec![("DOCKER_X".to_string(), "a\nb".to_string())];
        assert!(env_args(&vars, &dir.path().join("env")).is_err());
    }
}
