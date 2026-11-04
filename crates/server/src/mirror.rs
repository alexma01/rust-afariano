//! Pull mirrors: repositories whose code and `.gitlab-ci.yml` live on another
//! server (GitHub, Bitbucket, …), copied here so pipelines can run on them.
//!
//! rust-afariano fetches the upstream itself: every few minutes, when the
//! upstream's webhook calls, and right before each pipeline started by hand or
//! through the API. Pushes to a mirror are refused, so the copy never holds a
//! commit the upstream lacks, and a pipeline never starts on an old commit.
//!
//! A sync holds the repository's push lock: `git ls-remote` first (one cheap
//! round trip), then `git fetch` only when a branch or tag differs.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use base64::prelude::*;
use chrono::{DateTime, Utc};
use hmac::{Hmac, KeyInit, Mac};
use russh::keys::{Algorithm, HashAlg, PrivateKey, PublicKey};
use sha2::Sha256;
use tokio::io::AsyncReadExt;

use crate::app::{AppState, Shared};
use crate::git::Git;
use crate::models::Repo;
use crate::ops::Checked;
use crate::util::{ct_eq, glob_match, new_token};

const LS_REMOTE_TIMEOUT: Duration = Duration::from_secs(60);
/// A first copy of a big repository takes a while.
const FETCH_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// How long Run pipeline waits for the fetch before giving up (it goes on).
const PIPELINE_WAIT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Mirror {
    pub repo_id: i64,
    pub ssh_private_key: String,
    pub ssh_public_key: String,
    pub known_hosts: String,
    pub https_token: Option<String>,
    pub branches: String,
    pub webhook_secret: String,
    pub status_token: Option<String>,
    pub synced_at: Option<DateTime<Utc>>,
    pub checked_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
    pub hook_at: Option<DateTime<Utc>>,
    pub hook_error: Option<String>,
    pub status_at: Option<DateTime<Utc>>,
    pub status_error: Option<String>,
}

const MIRROR_COLS: &str = "repo_id, ssh_private_key, ssh_public_key, known_hosts, https_token, branches, \
     webhook_secret, status_token, synced_at, checked_at, error, hook_at, hook_error, status_at, status_error";

pub async fn load(state: &AppState, repo_id: i64) -> sqlx::Result<Option<Mirror>> {
    sqlx::query_as::<_, Mirror>(sql!("SELECT {MIRROR_COLS} FROM mirrors WHERE repo_id = $1"))
        .bind(repo_id)
        .fetch_optional(&state.db)
        .await
}

// ---------------------------------------------------------------- upstream URLs

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Ssh,
    Https,
}

/// A validated upstream clone URL. Only https and ssh are accepted: no local
/// paths, `file://`, `ext::` (runs commands) or plain http (would leak tokens).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upstream {
    pub transport: Transport,
    /// Lowercase, without user and port.
    pub host: String,
    /// `owner/repo`: no leading slash, no `.git`.
    pub path: String,
}

/// The forge an upstream lives on, when it is one whose commit status API we speak.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Forge {
    /// github.com or GitHub Enterprise Cloud (`*.ghe.com`). `repo`: `owner/name`.
    GitHub { api: String, repo: String },
    /// bitbucket.org. `repo`: `workspace/slug`.
    Bitbucket { api: String, repo: String },
}

impl Forge {
    pub fn name(&self) -> &'static str {
        match self {
            Forge::GitHub { .. } => "GitHub",
            Forge::Bitbucket { .. } => "Bitbucket",
        }
    }
}

fn host_ok(h: &str) -> bool {
    !h.is_empty()
        && h.len() <= 253
        && !h.starts_with(['-', '.'])
        && h.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

fn user_ok(u: &str) -> bool {
    !u.is_empty() && !u.starts_with('-') && u.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

impl Upstream {
    pub fn parse(raw: &str) -> Result<Self, &'static str> {
        const SHAPE: &str = "Use the upstream's clone URL: https://host/owner/repo.git, \
                             git@host:owner/repo.git or ssh://git@host/owner/repo.git.";
        let s = raw.trim();
        if s.is_empty() {
            return Err("Enter the upstream's clone URL.");
        }
        if s.len() > 500 || s.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err("A clone URL has no spaces.");
        }
        if s.starts_with("http://") {
            return Err("Use https://: plain http would send tokens and private code in the clear.");
        }
        let (transport, authority, path) = if let Some(rest) = s.strip_prefix("https://") {
            let (auth, path) = rest.split_once('/').ok_or(SHAPE)?;
            if auth.contains('@') {
                return Err("Leave credentials out of the URL: put a token in the HTTPS token field.");
            }
            (Transport::Https, auth, path)
        } else if let Some(rest) = s.strip_prefix("ssh://") {
            let (auth, path) = rest.split_once('/').ok_or(SHAPE)?;
            (Transport::Ssh, auth, path)
        } else if s.contains("://") || s.contains("::") || s.starts_with(['/', '.', '-', '~']) {
            return Err(SHAPE);
        } else {
            // scp-like `[user@]host:path`.
            let (auth, path) = s.split_once(':').ok_or(SHAPE)?;
            if auth.contains('/') {
                return Err(SHAPE);
            }
            (Transport::Ssh, auth, path)
        };
        let host_port = match authority.rsplit_once('@') {
            Some((user, hp)) if transport == Transport::Ssh => {
                if !user_ok(user) {
                    return Err("The user in the SSH URL is not valid.");
                }
                hp
            }
            Some(_) => return Err(SHAPE),
            None => authority,
        };
        let host = match host_port.split_once(':') {
            Some((h, port)) => {
                if port.parse::<u16>().map_or(true, |p| p == 0) {
                    return Err("The port in the URL is not valid.");
                }
                h
            }
            None => host_port,
        };
        if !host_ok(host) {
            return Err("The host in the URL is not valid.");
        }
        let path = path.trim_start_matches('/').trim_end_matches('/');
        let path = path.strip_suffix(".git").unwrap_or(path);
        if path.is_empty()
            || path.starts_with('-')
            || path.split('/').any(|seg| seg.is_empty() || seg == "." || seg == "..")
            || path.contains(['\\', '\'', '"', '`', '$', ';'])
        {
            return Err("The repository path in the URL is not valid.");
        }
        Ok(Self { transport, host: host.to_lowercase(), path: path.to_string() })
    }

    /// `bitbucket.org/molecle/davegantt`, for people.
    pub fn display(&self) -> String {
        format!("{}/{}", self.host, self.path)
    }

    pub fn forge(&self) -> Option<Forge> {
        let segments: Vec<&str> = self.path.split('/').collect();
        if segments.len() != 2 {
            return None;
        }
        let repo = self.path.clone();
        match self.host.as_str() {
            "github.com" | "www.github.com" | "ssh.github.com" => {
                Some(Forge::GitHub { api: "https://api.github.com".into(), repo })
            }
            h if h.ends_with(".ghe.com") && !h.starts_with("api.") => {
                Some(Forge::GitHub { api: format!("https://api.{h}"), repo })
            }
            "bitbucket.org" | "www.bitbucket.org" | "ssh.bitbucket.org" | "altssh.bitbucket.org" => {
                Some(Forge::Bitbucket { api: "https://api.bitbucket.org/2.0".into(), repo })
            }
            _ => None,
        }
    }

    /// The upstream's page in a browser, when it can be told from the URL.
    pub fn web_url(&self) -> Option<String> {
        match (self.forge(), self.transport) {
            (Some(Forge::GitHub { .. }), _) if self.host.ends_with("github.com") => {
                Some(format!("https://github.com/{}", self.path))
            }
            (Some(Forge::Bitbucket { .. }), _) => Some(format!("https://bitbucket.org/{}", self.path)),
            (Some(Forge::GitHub { .. }), _) | (None, Transport::Https) => {
                Some(format!("https://{}/{}", self.host, self.path))
            }
            (None, Transport::Ssh) => None,
        }
    }

    /// The user name sent with a bare HTTPS token (`user:token` overrides it).
    fn https_user(&self) -> &'static str {
        match self.forge() {
            Some(Forge::GitHub { .. }) => "x-access-token",
            Some(Forge::Bitbucket { .. }) => "x-token-auth",
            None => "git",
        }
    }
}

/// Branch patterns to copy: comma separated, `*` as wildcard (one per pattern, as
/// git refspecs allow). Empty means every branch.
pub fn parse_branches(s: &str) -> Result<Vec<String>, &'static str> {
    let mut out: Vec<String> =
        s.split([',', ' ', '\n']).map(str::trim).filter(|p| !p.is_empty()).map(String::from).collect();
    if out.is_empty() || out.iter().any(|p| p == "*") {
        return Ok(vec!["*".into()]);
    }
    for p in &out {
        if p.matches('*').count() > 1 {
            return Err("A branch pattern may hold one `*` only.");
        }
        if !crate::ops::valid_branch(&p.replace('*', "x")) {
            return Err("A branch pattern is not a valid branch name.");
        }
    }
    out.dedup();
    Ok(out)
}

// ---------------------------------------------------------------- settings

pub struct MirrorInput {
    pub url: String,
    /// `None` leaves the setting as it is.
    pub branches: Option<String>,
    /// `Some("")` removes the token.
    pub https_token: Option<String>,
    pub status_token: Option<String>,
}

fn encrypt_opt(state: &AppState, v: &str) -> Option<String> {
    let v = v.trim();
    (!v.is_empty()).then(|| state.cipher.encrypt(v))
}

/// Makes `repo` a mirror of `input.url`, or changes its mirror settings.
pub async fn configure(state: &AppState, repo: &Repo, input: MirrorInput) -> Checked<()> {
    let up = match Upstream::parse(&input.url) {
        Ok(u) => u,
        Err(e) => return Ok(Err(e)),
    };
    let branches = match input.branches.as_deref().map(parse_branches).transpose() {
        Ok(b) => b.map(|b| b.join(", ")),
        Err(e) => return Ok(Err(e)),
    };
    let url = input.url.trim().to_string();
    let existing = load(state, repo.id).await?;
    let mut tx = state.db.begin().await?;
    match &existing {
        None => {
            let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)
                .map_err(|e| anyhow!("generating the mirror key: {e}"))?;
            let pem = key.to_openssh(russh::keys::ssh_key::LineEnding::LF).map_err(|e| anyhow!("{e}"))?;
            let public = key.public_key().to_openssh().map_err(|e| anyhow!("{e}"))?;
            let comment = format!("rust-afariano {}/{}", state.cfg.public_host(), repo.path());
            sqlx::query(
                "INSERT INTO mirrors (repo_id, ssh_private_key, ssh_public_key, branches, webhook_secret, https_token, \
                 status_token) VALUES ($1, $2, $3, $4, $5, $6, $7)",
            )
            .bind(repo.id)
            .bind(state.cipher.encrypt(&pem))
            .bind(format!("{public} {comment}"))
            .bind(branches.as_deref().unwrap_or("*"))
            .bind(state.cipher.encrypt(&new_token("")))
            .bind(input.https_token.as_deref().and_then(|t| encrypt_opt(state, t)))
            .bind(input.status_token.as_deref().and_then(|t| encrypt_opt(state, t)))
            .execute(&mut *tx)
            .await?;
            // Every branch arriving at the first sync would start a pipeline.
            sqlx::query("UPDATE repos SET mirror_url = $2, pipelines_on_push = FALSE WHERE id = $1")
                .bind(repo.id)
                .bind(&url)
                .execute(&mut *tx)
                .await?;
        }
        Some(m) => {
            if repo.mirror_url.as_deref() != Some(url.as_str()) {
                let old_host = repo.mirror_url.as_deref().and_then(|u| Upstream::parse(u).ok()).map(|u| u.host);
                // Another upstream: its first sync starts no pipelines, and another host
                // means another host key.
                sqlx::query(
                    "UPDATE mirrors SET synced_at = NULL, error = NULL, \
                     known_hosts = CASE WHEN $2 THEN '' ELSE known_hosts END WHERE repo_id = $1",
                )
                .bind(repo.id)
                .bind(old_host.as_deref() != Some(up.host.as_str()))
                .execute(&mut *tx)
                .await?;
                sqlx::query("UPDATE repos SET mirror_url = $2 WHERE id = $1")
                    .bind(repo.id)
                    .bind(&url)
                    .execute(&mut *tx)
                    .await?;
            }
            if let Some(b) = &branches {
                sqlx::query("UPDATE mirrors SET branches = $2 WHERE repo_id = $1")
                    .bind(m.repo_id)
                    .bind(b)
                    .execute(&mut *tx)
                    .await?;
            }
            if let Some(t) = &input.https_token {
                sqlx::query("UPDATE mirrors SET https_token = $2 WHERE repo_id = $1")
                    .bind(m.repo_id)
                    .bind(encrypt_opt(state, t))
                    .execute(&mut *tx)
                    .await?;
            }
            if let Some(t) = &input.status_token {
                sqlx::query("UPDATE mirrors SET status_token = $2, status_error = NULL WHERE repo_id = $1")
                    .bind(m.repo_id)
                    .bind(encrypt_opt(state, t))
                    .execute(&mut *tx)
                    .await?;
            }
        }
    }
    tx.commit().await?;
    Ok(Ok(()))
}

/// Turns the mirror back into an ordinary repository (its content stays).
pub async fn stop(state: &AppState, repo: &Repo) -> sqlx::Result<()> {
    let mut tx = state.db.begin().await?;
    sqlx::query("DELETE FROM mirrors WHERE repo_id = $1").bind(repo.id).execute(&mut *tx).await?;
    sqlx::query("UPDATE repos SET mirror_url = NULL WHERE id = $1").bind(repo.id).execute(&mut *tx).await?;
    tx.commit().await
}

/// The next sync pins whatever host key the upstream presents.
pub async fn forget_host_key(state: &AppState, repo_id: i64) -> sqlx::Result<()> {
    sqlx::query("UPDATE mirrors SET known_hosts = '' WHERE repo_id = $1").bind(repo_id).execute(&state.db).await?;
    Ok(())
}

pub async fn new_webhook_secret(state: &AppState, repo_id: i64) -> sqlx::Result<String> {
    let secret = new_token("");
    sqlx::query("UPDATE mirrors SET webhook_secret = $2 WHERE repo_id = $1")
        .bind(repo_id)
        .bind(state.cipher.encrypt(&secret))
        .execute(&state.db)
        .await?;
    Ok(secret)
}

/// `host ssh-ed25519 SHA256:…` for each pinned host key.
pub fn host_fingerprints(known_hosts: &str) -> Vec<String> {
    known_hosts
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let host = parts.next()?;
            let kind = parts.next()?;
            let b64 = parts.next()?;
            let key = PublicKey::from_openssh(&format!("{kind} {b64}")).ok()?;
            Some(format!("{host} {kind} {}", key.fingerprint(HashAlg::Sha256)))
        })
        .collect()
}

/// Why a push to a mirror is refused, for the git client.
pub fn push_refused(repo: &Repo) -> String {
    let upstream =
        repo.mirror_url.as_deref().map(|u| Upstream::parse(u).map(|u| u.display()).unwrap_or_else(|_| u.to_string()));
    format!(
        "{} is a mirror of {}: push there, and this copy follows.",
        repo.path(),
        upstream.unwrap_or_else(|| "another repository".into())
    )
}

pub fn webhook_url(state: &AppState, repo: &Repo) -> String {
    format!("{}/api/v1/repos/{}/mirror/hook", state.cfg.public_url, repo.path())
}

// ---------------------------------------------------------------- webhooks

/// Checks a webhook call against the mirror's secret: GitHub's
/// `X-Hub-Signature-256` and Bitbucket's `X-Hub-Signature` (HMAC-SHA256 of the
/// body, `sha256=<hex>`), or GitLab's `X-Gitlab-Token`.
pub fn verify_webhook(secret: &str, headers: &axum::http::HeaderMap, body: &[u8]) -> Result<(), &'static str> {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(str::trim);
    if let Some(sig) = header("x-hub-signature-256").or_else(|| header("x-hub-signature")) {
        let hex = sig.strip_prefix("sha256=").ok_or("the signature is not HMAC-SHA256")?;
        let sig = decode_hex(hex).ok_or("the signature is not hexadecimal")?;
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC takes keys of any length");
        mac.update(body);
        return mac.verify_slice(&sig).map_err(|_| "the signature does not match the secret");
    }
    if let Some(token) = header("x-gitlab-token") {
        return if ct_eq(token.as_bytes(), secret.as_bytes()) {
            Ok(())
        } else {
            Err("the token does not match the secret")
        };
    }
    Err("the call is not signed: set the secret on the webhook")
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

pub async fn record_hook(state: &AppState, repo_id: i64, error: Option<&str>) {
    if let Err(e) = sqlx::query("UPDATE mirrors SET hook_at = now(), hook_error = $2 WHERE repo_id = $1")
        .bind(repo_id)
        .bind(error)
        .execute(&state.db)
        .await
    {
        tracing::error!(repo_id, "recording the webhook call: {e:#}");
    }
}

// ---------------------------------------------------------------- syncing

/// A directory under the data dir for one sync's key and known_hosts files,
/// removed when dropped.
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(data_dir: &Path) -> std::io::Result<Self> {
        let dir = data_dir.join("tmp").join(new_token("mirror-"));
        std::fs::create_dir_all(&dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(Self(dir))
    }

    fn write_private(&self, name: &str, content: &str) -> std::io::Result<PathBuf> {
        use std::io::Write;
        let path = self.0.join(name);
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        opts.open(&path)?.write_all(content.as_bytes())?;
        Ok(path)
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn sh_quote(p: &Path) -> String {
    format!("'{}'", p.display().to_string().replace('\'', r"'\''"))
}

/// Credentials and transport settings for the git processes of one sync.
struct Session {
    _dir: TmpDir,
    env: Vec<(String, String)>,
    /// Set when the host key is not pinned yet: read back after the first contact.
    new_known_hosts: Option<PathBuf>,
}

impl Session {
    fn new(state: &AppState, mirror: &Mirror, up: &Upstream) -> anyhow::Result<Self> {
        let dir = TmpDir::new(&state.cfg.data_dir).context("creating a temporary directory")?;
        let mut env = vec![
            // Nothing but https and ssh, submodules included: `ext::` would run commands.
            ("GIT_ALLOW_PROTOCOL".to_string(), "https:ssh".to_string()),
            ("GIT_TERMINAL_PROMPT".to_string(), "0".to_string()),
            // Empty: git asks no helper program for a password.
            ("GIT_ASKPASS".to_string(), String::new()),
            ("SSH_ASKPASS".to_string(), String::new()),
        ];
        let mut new_known_hosts = None;
        match up.transport {
            Transport::Ssh => {
                let pem = state.cipher.decrypt(&mirror.ssh_private_key)?;
                let pem = if pem.ends_with('\n') { pem } else { format!("{pem}\n") };
                let key = dir.write_private("key", &pem)?;
                let known = dir.write_private("known_hosts", &mirror.known_hosts)?;
                let strict = if mirror.known_hosts.trim().is_empty() {
                    new_known_hosts = Some(known.clone());
                    "accept-new"
                } else {
                    "yes"
                };
                env.push((
                    "GIT_SSH_COMMAND".into(),
                    format!(
                        "ssh -F /dev/null -i {} -o IdentitiesOnly=yes -o IdentityAgent=none -o BatchMode=yes \
                         -o UserKnownHostsFile={} -o GlobalKnownHostsFile=/dev/null -o StrictHostKeyChecking={strict} \
                         -o HashKnownHosts=no -o UpdateHostKeys=no -o ConnectTimeout=20 -o ServerAliveInterval=15 \
                         -o ServerAliveCountMax=4 -o LogLevel=ERROR",
                        sh_quote(&key),
                        sh_quote(&known)
                    ),
                ));
                env.push(("GIT_SSH_VARIANT".into(), "ssh".into()));
            }
            Transport::Https => {
                if let Some(enc) = &mirror.https_token {
                    let token = state.cipher.decrypt(enc)?;
                    let pair = if token.contains(':') { token } else { format!("{}:{token}", up.https_user()) };
                    // A header from the environment, not the URL: ps and error messages never show it.
                    env.push(("GIT_CONFIG_COUNT".into(), "1".into()));
                    env.push(("GIT_CONFIG_KEY_0".into(), "http.extraHeader".into()));
                    env.push((
                        "GIT_CONFIG_VALUE_0".into(),
                        format!("Authorization: Basic {}", BASE64_STANDARD.encode(pair)),
                    ));
                }
            }
        }
        Ok(Self { _dir: dir, env, new_known_hosts })
    }

    /// Runs git; stdout on success, an explained error otherwise.
    async fn run(&self, git: &Git, up: &Upstream, args: &[&str], timeout: Duration) -> anyhow::Result<String> {
        let mut cmd = git.command();
        cmd.envs(self.env.iter().map(|(k, v)| (k, v)));
        // No stored credentials of the machine (e.g. macOS keychain) ever reach the upstream.
        cmd.args(["-c", "credential.helper="]);
        cmd.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd.spawn().context("spawning git")?;
        let mut stdout = child.stdout.take().expect("piped");
        let mut stderr = child.stderr.take().expect("piped");
        let work = async {
            let mut out = Vec::new();
            let mut err = Vec::new();
            let mut out_cap = (&mut stdout).take(16 << 20);
            let mut err_cap = (&mut stderr).take(64 << 10);
            let (a, b) = tokio::join!(out_cap.read_to_end(&mut out), err_cap.read_to_end(&mut err));
            a?;
            b?;
            drop(stdout);
            drop(stderr);
            let status = child.wait().await?;
            anyhow::Ok((status, out, err))
        };
        let (status, out, err) = match tokio::time::timeout(timeout, work).await {
            Ok(r) => r?,
            Err(_) => bail!("{} did not answer within {} s.", up.display(), timeout.as_secs()),
        };
        if !status.success() {
            bail!("{}", explain(&String::from_utf8_lossy(&err), up));
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
    }
}

/// git's error, trimmed, with what to do about the usual causes.
fn explain(stderr: &str, up: &Upstream) -> String {
    let lines: Vec<&str> = stderr.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let mut msg = lines[lines.len().saturating_sub(4)..].join(" ");
    if msg.len() > 600 {
        let cut = (0..=600).rev().find(|i| msg.is_char_boundary(*i)).unwrap_or(0);
        msg.truncate(cut);
        msg.push('…');
    }
    let low = stderr.to_lowercase();
    let forge = up.forge();
    let hint = if low.contains("host key verification failed") || low.contains("remote host identification has changed")
    {
        "The upstream's SSH host key is not the one pinned at the first sync. If the host really changed its key, \
         use Forget host key in Settings → Mirror."
            .to_string()
    } else if low.contains("permission denied (publickey") || low.contains("access denied") {
        match forge {
            Some(Forge::GitHub { .. }) => "Add the mirror's key to the GitHub repository: Settings → Deploy keys → \
                                           Add deploy key, with Allow write access unticked."
                .to_string(),
            Some(Forge::Bitbucket { .. }) => "Add the mirror's key to the Bitbucket repository: Repository settings → \
                                              Access keys → Add key."
                .to_string(),
            None => "Give the mirror's key (Settings → Mirror) read access to the upstream.".to_string(),
        }
    } else if low.contains("could not read username")
        || low.contains("authentication failed")
        || low.contains("invalid username or password")
        || low.contains("returned error: 401")
        || low.contains("returned error: 403")
    {
        "The upstream refused the HTTPS token (or wants one): set a token that may read the repository in \
         Settings → Mirror."
            .to_string()
    } else if low.contains("repository not found") || low.contains("not found") || low.contains("does not exist") {
        "Check the URL, and that the key or token may read that repository (private repositories answer \
         \"not found\" to strangers)."
            .to_string()
    } else if low.contains("could not resolve host") || low.contains("could not resolve hostname") {
        format!("This server cannot resolve {}.", up.host)
    } else {
        String::new()
    };
    match (msg.is_empty(), hint.is_empty()) {
        (true, true) => format!("git failed talking to {}.", up.display()),
        (true, false) => hint,
        (false, true) => msg,
        (false, false) => format!("{msg} — {hint}"),
    }
}

/// The upstream's branches, tags and default branch, from `git ls-remote --symref`.
#[derive(Debug, Default, PartialEq, Eq)]
struct Remote {
    head: Option<String>,
    refs: BTreeMap<String, String>,
}

fn parse_ls_remote(out: &str) -> Remote {
    let mut remote = Remote::default();
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix("ref: ") {
            if let Some((target, "HEAD")) = rest.split_once('\t') {
                remote.head = target.strip_prefix("refs/heads/").map(String::from);
            }
            continue;
        }
        let Some((sha, name)) = line.split_once('\t') else { continue };
        let is_sha = matches!(sha.len(), 40 | 64) && sha.bytes().all(|b| b.is_ascii_hexdigit());
        if !is_sha || name.ends_with("^{}") || !(name.starts_with("refs/heads/") || name.starts_with("refs/tags/")) {
            continue;
        }
        remote.refs.insert(name.to_string(), sha.to_lowercase());
    }
    remote
}

/// The upstream refs this mirror copies: branches matching a pattern, every tag.
fn wanted(remote: &Remote, patterns: &[String]) -> BTreeMap<String, String> {
    remote
        .refs
        .iter()
        .filter(|(name, _)| match name.strip_prefix("refs/heads/") {
            Some(b) => patterns.iter().any(|p| glob_match(p, b)),
            None => true,
        })
        .map(|(n, s)| (n.clone(), s.clone()))
        .collect()
}

fn refspecs(patterns: &[String], wanted: &BTreeMap<String, String>) -> Vec<String> {
    let mut specs: Vec<String> = patterns
        .iter()
        // A named branch the upstream lacks would fail the whole fetch.
        .filter(|p| p.contains('*') || wanted.contains_key(&format!("refs/heads/{p}")))
        .map(|p| format!("+refs/heads/{p}:refs/heads/{p}"))
        .collect();
    specs.push("+refs/tags/*:refs/tags/*".into());
    specs
}

/// Local branches to delete after the fetch: ones no pattern copies any more, and
/// named ones the upstream deleted. (`--prune` handles the wildcard ones.)
fn stale_heads(
    local: &BTreeMap<String, String>,
    wanted: &BTreeMap<String, String>,
    patterns: &[String],
) -> Vec<String> {
    local
        .keys()
        .filter_map(|name| name.strip_prefix("refs/heads/").map(|b| (name, b)))
        .filter(|(name, b)| {
            !wanted.contains_key(*name) && !patterns.iter().any(|p| p.contains('*') && glob_match(p, b))
        })
        .map(|(name, _)| name.clone())
        .collect()
}

pub struct Synced {
    /// Branches and tags created, moved or deleted.
    pub changed: usize,
}

/// Brings the mirror up to date (waits for a sync or push already running).
/// The error is worded for people; it is also kept for Settings → Mirror.
pub async fn sync(state: &Shared, repo_id: i64) -> Result<Synced, String> {
    let lock = state.push_lock(repo_id).lock_owned().await;
    // Webhooks arriving from now on need another sync.
    state.mirror_queued.lock().expect("mirror queue").remove(&repo_id);
    let result = sync_locked(state, repo_id).await.map_err(|e| format!("{e:#}"));
    let error = result.as_ref().err();
    // Still under the lock, so a later sync's outcome is never overwritten by this one.
    let previous: Option<Option<String>> = match sqlx::query_scalar(
        "UPDATE mirrors m SET checked_at = now(), \
         synced_at = CASE WHEN $2::text IS NULL THEN now() ELSE m.synced_at END, error = $2 \
         FROM mirrors old WHERE m.repo_id = $1 AND old.repo_id = m.repo_id RETURNING old.error",
    )
    .bind(repo_id)
    .bind(error)
    .fetch_optional(&state.db)
    .await
    {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(repo_id, "recording the mirror sync: {e:#}");
            None
        }
    };
    drop(lock);
    if let Some(e) = error {
        // Polling retries a broken mirror every few minutes: log each new problem once.
        if previous.flatten().as_deref() != Some(e.as_str()) {
            tracing::warn!(repo_id, "mirror sync failed: {e}");
        }
    }
    result
}

async fn sync_locked(state: &Shared, repo_id: i64) -> anyhow::Result<Synced> {
    let repo = state.repo_by_id(repo_id).await?;
    let Some(url) = repo.mirror_url.clone() else { bail!("{} is not a mirror.", repo.path()) };
    let mirror = load(state, repo_id).await?.ok_or_else(|| anyhow!("{} has no mirror settings.", repo.path()))?;
    let up = Upstream::parse(&url).map_err(|e| anyhow!("{e}"))?;
    let patterns = parse_branches(&mirror.branches).map_err(|e| anyhow!("{e}"))?;
    let git = state.git(repo_id);
    let session = Session::new(state, &mirror, &up)?;

    let listing = session
        .run(&git, &up, &["ls-remote", "--symref", &url, "HEAD", "refs/heads/*", "refs/tags/*"], LS_REMOTE_TIMEOUT)
        .await?;
    if let Some(path) = &session.new_known_hosts {
        let pinned = std::fs::read_to_string(path).unwrap_or_default();
        if !pinned.trim().is_empty() {
            sqlx::query("UPDATE mirrors SET known_hosts = $2 WHERE repo_id = $1")
                .bind(repo_id)
                .bind(pinned.trim())
                .execute(&state.db)
                .await?;
            tracing::info!(repo = %repo.path(), host = %up.host, "pinned the mirror's SSH host key");
        }
    }
    let remote = parse_ls_remote(&listing);
    let wanted = wanted(&remote, &patterns);
    let before = git.ref_map().await?;

    if wanted != before {
        let permit = state.pack_slots.clone().acquire_owned().await?;
        let specs = refspecs(&patterns, &wanted);
        let mut args =
            vec!["-c", "gc.auto=0", "fetch", "--prune", "--no-tags", "--no-write-fetch-head", "--quiet", url.as_str()];
        args.extend(specs.iter().map(String::as_str));
        session.run(&git, &up, &args, FETCH_TIMEOUT).await?;
        drop(permit);
        let fetched = git.ref_map().await?;
        for name in stale_heads(&fetched, &wanted, &patterns) {
            git.delete_ref(&name).await?;
        }
    }

    // The default branch follows the upstream's.
    let mut repo = repo;
    if let Some(head) = &remote.head
        && *head != repo.default_branch
        && git.resolve(&format!("refs/heads/{head}")).await?.is_some()
    {
        git.set_head(head).await?;
        sqlx::query("UPDATE repos SET default_branch = $2 WHERE id = $1")
            .bind(repo_id)
            .bind(head)
            .execute(&state.db)
            .await?;
        repo.default_branch = head.clone();
    }

    // No pipelines for what the first sync brings: that is history, not a push.
    if mirror.synced_at.is_none() {
        repo.pipelines_on_push = false;
    }
    let changed = crate::push::after_ref_update(state, repo, before, None).await;
    Ok(Synced { changed })
}

/// Syncs before a pipeline starts, waiting at most two minutes (the fetch goes on).
pub async fn sync_for_pipeline(state: &Shared, repo: &Repo) -> Result<Synced, String> {
    let s = state.clone();
    let id = repo.id;
    let task = tokio::spawn(async move { sync(&s, id).await });
    match tokio::time::timeout(PIPELINE_WAIT, task).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => Err(format!("internal error: {e}")),
        Err(_) => Err("Fetching the upstream takes more than two minutes; it goes on in the background. \
                       Start the pipeline again in a moment."
            .into()),
    }
}

/// Queues a sync in the background, unless one is already waiting to start.
pub fn request_sync(state: &Shared, repo_id: i64) {
    if !state.mirror_queued.lock().expect("mirror queue").insert(repo_id) {
        return;
    }
    let s = state.clone();
    tokio::spawn(async move {
        let _ = sync(&s, repo_id).await;
    });
}

/// Removes key files left behind by a sync that a crash or restart interrupted.
pub fn clean_tmp(data_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(data_dir.join("tmp")) else { return };
    for e in entries.flatten() {
        if e.file_name().to_string_lossy().starts_with("mirror-") {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

/// Syncs every mirror not checked for `mirror_interval` seconds, one at a time.
pub async fn poller(state: Shared) {
    let every = state.cfg.mirror_interval;
    if every == 0 {
        tracing::info!("mirror polling is off (RUST_AFARIANO_MIRROR_INTERVAL=0)");
        return;
    }
    let mut tick = tokio::time::interval(Duration::from_secs(30));
    loop {
        tick.tick().await;
        let due: Vec<i64> = match sqlx::query_scalar(
            "SELECT repo_id FROM mirrors WHERE checked_at IS NULL OR checked_at < now() - make_interval(secs => $1) \
             ORDER BY checked_at NULLS FIRST LIMIT 10",
        )
        .bind(every as f64)
        .fetch_all(&state.db)
        .await
        {
            Ok(ids) => ids,
            Err(e) => {
                tracing::warn!("mirror poller: {e:#}");
                continue;
            }
        };
        for id in due {
            let _ = sync(&state, id).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn up(s: &str) -> Upstream {
        Upstream::parse(s).unwrap_or_else(|e| panic!("{s}: {e}"))
    }

    #[test]
    fn urls() {
        let u = up("git@bitbucket.org:molecle/davegantt.git");
        assert_eq!(
            (u.transport, u.host.as_str(), u.path.as_str()),
            (Transport::Ssh, "bitbucket.org", "molecle/davegantt")
        );
        let u = up("ssh://git@ssh.bitbucket.org:22/molecle/davegantt.git");
        assert_eq!((u.host.as_str(), u.path.as_str()), ("ssh.bitbucket.org", "molecle/davegantt"));
        let u = up("https://github.com/Matteo7S/lead-capture-platform.git");
        assert_eq!(
            (u.transport, u.display().as_str()),
            (Transport::Https, "github.com/Matteo7S/lead-capture-platform")
        );
        let u = up("ferroligroup@ferroligroup.ghe.com:Group/ferroli-iot-app.git");
        assert_eq!(u.host, "ferroligroup.ghe.com");
        assert_eq!(up("ssh://git@127.0.0.1:2222/acme/web.git").path, "acme/web");
        for bad in [
            "",
            "http://github.com/a/b.git",
            "https://user:pass@github.com/a/b.git",
            "file:///srv/repo.git",
            "/srv/repo.git",
            "./repo",
            "ext::sh -c touch% /tmp/x",
            "git://github.com/a/b.git",
            "-oProxyCommand=x:a/b",
            "git@-oProxyCommand=x:a/b",
            "git@host:../etc",
            "git@host:a/b c",
            "https://github.com",
            "https://host:0/a/b",
            "ssh://-u@host/a/b",
        ] {
            assert!(Upstream::parse(bad).is_err(), "{bad} should be refused");
        }
    }

    #[test]
    fn forges() {
        let gh = up("git@github.com:Matteo7S/lead-capture-platform.git").forge().unwrap();
        assert_eq!(
            gh,
            Forge::GitHub { api: "https://api.github.com".into(), repo: "Matteo7S/lead-capture-platform".into() }
        );
        let ghe = up("ferroligroup@ferroligroup.ghe.com:Group/ferroli-iot-app.git").forge().unwrap();
        assert_eq!(
            ghe,
            Forge::GitHub { api: "https://api.ferroligroup.ghe.com".into(), repo: "Group/ferroli-iot-app".into() }
        );
        let bb = up("git@ssh.bitbucket.org:molecle/davegantt.git").forge().unwrap();
        assert!(matches!(bb, Forge::Bitbucket { ref repo, .. } if repo == "molecle/davegantt"));
        assert_eq!(up("git@gitlab.example.com:a/b.git").forge(), None);
        assert_eq!(up("git@github.com:a/b/c.git").forge(), None);
        assert_eq!(up("https://bitbucket.org/molecle/davegantt.git").https_user(), "x-token-auth");
        assert_eq!(up("git@ssh.github.com:a/b.git").web_url().as_deref(), Some("https://github.com/a/b"));
        assert_eq!(up("git@ssh.bitbucket.org:a/b.git").web_url().as_deref(), Some("https://bitbucket.org/a/b"));
        assert_eq!(up("git@x.ghe.com:a/b.git").web_url().as_deref(), Some("https://x.ghe.com/a/b"));
        assert_eq!(up("git@git.example.com:a/b.git").web_url(), None);
    }

    #[test]
    fn branch_patterns() {
        assert_eq!(parse_branches("").unwrap(), vec!["*"]);
        assert_eq!(parse_branches("main, *").unwrap(), vec!["*"]);
        assert_eq!(parse_branches("main, staging release/*").unwrap(), vec!["main", "staging", "release/*"]);
        assert!(parse_branches("a*b*").is_err());
        assert!(parse_branches("bad..name").is_err());
    }

    #[test]
    fn ls_remote_and_refspecs() {
        let out = "ref: refs/heads/main\tHEAD\n\
                   1111111111111111111111111111111111111111\tHEAD\n\
                   1111111111111111111111111111111111111111\trefs/heads/main\n\
                   2222222222222222222222222222222222222222\trefs/heads/staging\n\
                   3333333333333333333333333333333333333333\trefs/heads/feature/x\n\
                   4444444444444444444444444444444444444444\trefs/tags/v1\n\
                   5555555555555555555555555555555555555555\trefs/tags/v1^{}\n\
                   6666666666666666666666666666666666666666\trefs/pull/1/head\n";
        let remote = parse_ls_remote(out);
        assert_eq!(remote.head.as_deref(), Some("main"));
        assert_eq!(remote.refs.len(), 4);
        let patterns = parse_branches("main, gone, feature/*").unwrap();
        let w = wanted(&remote, &patterns);
        assert_eq!(
            w.keys().map(String::as_str).collect::<Vec<_>>(),
            ["refs/heads/feature/x", "refs/heads/main", "refs/tags/v1"]
        );
        assert_eq!(
            refspecs(&patterns, &w),
            [
                "+refs/heads/main:refs/heads/main",
                "+refs/heads/feature/*:refs/heads/feature/*",
                "+refs/tags/*:refs/tags/*"
            ]
        );
        let local: BTreeMap<String, String> =
            ["refs/heads/main", "refs/heads/gone", "refs/heads/staging", "refs/heads/feature/new"]
                .into_iter()
                .map(|n| (n.to_string(), "x".to_string()))
                .collect();
        // `gone` was deleted upstream, `staging` is no longer copied; `feature/new`
        // arrived after the listing through the wildcard.
        assert_eq!(stale_heads(&local, &w, &patterns), ["refs/heads/gone", "refs/heads/staging"]);
    }

    #[test]
    fn webhook_signatures() {
        use axum::http::HeaderMap;
        let body = br#"{"zen":"Keep it logically awesome."}"#;
        let mut mac = Hmac::<Sha256>::new_from_slice(b"s3cret").unwrap();
        mac.update(body);
        let hex: String = mac.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect();

        let mut github = HeaderMap::new();
        github.insert("x-hub-signature", "sha1=0000".parse().unwrap());
        github.insert("x-hub-signature-256", format!("sha256={hex}").parse().unwrap());
        assert_eq!(verify_webhook("s3cret", &github, body), Ok(()));
        assert!(verify_webhook("other", &github, body).is_err());
        assert!(verify_webhook("s3cret", &github, b"tampered").is_err());

        let mut bitbucket = HeaderMap::new();
        bitbucket.insert("x-hub-signature", format!("sha256={hex}").parse().unwrap());
        assert_eq!(verify_webhook("s3cret", &bitbucket, body), Ok(()));

        let mut gitlab = HeaderMap::new();
        gitlab.insert("x-gitlab-token", "s3cret".parse().unwrap());
        assert_eq!(verify_webhook("s3cret", &gitlab, body), Ok(()));
        assert!(verify_webhook("s3cret", &HeaderMap::new(), body).is_err());
    }

    /// RFC 4231 test case 2, so the HMAC itself is the standard one.
    #[test]
    fn hmac_vector() {
        let mut mac = Hmac::<Sha256>::new_from_slice(b"Jefe").unwrap();
        mac.update(b"what do ya want for nothing?");
        let hex: String = mac.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843");
    }

    #[test]
    fn errors_are_explained() {
        let gh = up("git@github.com:a/b.git");
        let e = explain(
            "git@github.com: Permission denied (publickey).\nfatal: Could not read from remote repository.",
            &gh,
        );
        assert!(e.contains("Deploy keys"), "{e}");
        let bb = up("git@bitbucket.org:a/b.git");
        assert!(explain("Host key verification failed.", &bb).contains("Forget host key"));
        assert!(explain("Permission denied (publickey).", &bb).contains("Access keys"));
        assert_eq!(explain("", &bb), "git failed talking to bitbucket.org/a/b.");
    }

    #[test]
    fn host_keys() {
        let line = "ssh.bitbucket.org ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIIazEu89wgQZ4bqs3d63QSMzYVa0MuJ2e2gKTKqu+UUO";
        let fp = host_fingerprints(line);
        assert_eq!(fp.len(), 1);
        // Bitbucket's published fingerprint.
        assert_eq!(fp[0], "ssh.bitbucket.org ssh-ed25519 SHA256:ybgmFkzwOSotHTHLJgHO0QN8L0xErw6vd0VhFA9m3SM");
        assert!(host_fingerprints("garbage").is_empty());
    }
}
