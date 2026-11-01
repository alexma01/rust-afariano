//! Embedded SSH server (russh) that only runs `git-upload-pack` and
//! `git-receive-pack`. Users authenticate with keys added in the web UI.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use russh::keys::{Algorithm, HashAlg, PrivateKey, PublicKey};
use russh::server::{Auth, ChannelOpenHandle, Msg, Server as _, Session, VerifiedPublicKey};
use russh::{Channel, ChannelId};
use tokio::io::AsyncWriteExt;

use crate::app::{AppState, Shared};
use crate::git::{git_cmd, repo_dir};
use crate::models::User;
use crate::push::after_push;

pub fn load_or_create_host_key(state: &AppState) -> anyhow::Result<PrivateKey> {
    let path = state.cfg.data_dir.join("ssh_host_ed25519_key");
    if path.exists() {
        let pem = std::fs::read_to_string(&path)?;
        return PrivateKey::from_openssh(pem.trim()).context("parsing SSH host key");
    }
    let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).context("generating SSH host key")?;
    let pem = key.to_openssh(russh::keys::ssh_key::LineEnding::LF).context("encoding SSH host key")?;
    std::fs::write(&path, pem.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    tracing::info!("generated SSH host key {}", path.display());
    Ok(key)
}

pub fn fingerprint(key: &PublicKey) -> String {
    key.fingerprint(HashAlg::Sha256).to_string()
}

/// Parse an `authorized_keys`-style line; returns (normalized key, fingerprint).
pub fn parse_public_key(line: &str) -> anyhow::Result<(String, String)> {
    let line = line.trim();
    let key = PublicKey::from_openssh(line).context("not an OpenSSH public key (ssh-ed25519 AAAA… comment)")?;
    let mut normalized = key.to_openssh().context("encoding key")?;
    if let Some(comment) = line.split_whitespace().nth(2) {
        normalized = format!("{normalized} {comment}");
    }
    Ok((normalized, fingerprint(&key)))
}

pub async fn serve(state: Shared) -> anyhow::Result<()> {
    let addr = state.cfg.ssh_addr.clone();
    let key = load_or_create_host_key(&state)?;
    let config = Arc::new(russh::server::Config {
        inactivity_timeout: Some(Duration::from_secs(600)),
        auth_rejection_time: Duration::from_secs(1),
        auth_rejection_time_initial: Some(Duration::from_secs(0)),
        keys: vec![key],
        ..Default::default()
    });
    let socket = tokio::net::TcpListener::bind(&addr).await.with_context(|| format!("binding SSH on {addr}"))?;
    tracing::info!("ssh listening on {addr}");
    let mut server = SshServer { state };
    server.run_on_socket(config, &socket).await?;
    Ok(())
}

struct SshServer {
    state: Shared,
}

impl russh::server::Server for SshServer {
    type Handler = Conn;
    fn new_client(&mut self, peer: Option<std::net::SocketAddr>) -> Conn {
        Conn { state: self.state.clone(), peer, user: None, channels: HashMap::new(), git_protocol: None }
    }
    fn handle_session_error(&mut self, error: <Self::Handler as russh::server::Handler>::Error) {
        tracing::debug!("ssh session error: {error:#}");
    }
}

struct Conn {
    state: Shared,
    peer: Option<std::net::SocketAddr>,
    user: Option<User>,
    channels: HashMap<ChannelId, Channel<Msg>>,
    git_protocol: Option<String>,
}

/// `git-upload-pack '/ns/name.git'` → ("upload-pack", "ns", "name").
fn parse_command(cmd: &str) -> Option<(&'static str, String, String)> {
    let cmd = cmd.trim();
    let (verb, arg) = cmd.split_once(' ')?;
    let service = match verb {
        "git-upload-pack" => "upload-pack",
        "git-receive-pack" => "receive-pack",
        "git" => {
            let (sub, rest) = arg.trim().split_once(' ')?;
            return match sub {
                "upload-pack" | "receive-pack" => parse_command(&format!("git-{sub} {rest}")),
                _ => None,
            };
        }
        _ => return None,
    };
    let path = arg.trim().trim_matches(|c| c == '\'' || c == '"').trim_start_matches('/');
    let path = path.strip_suffix('/').unwrap_or(path);
    let path = path.strip_suffix(".git").unwrap_or(path);
    let (ns, name) = path.split_once('/')?;
    if name.contains('/') || ns.is_empty() || name.is_empty() || path.contains("..") {
        return None;
    }
    Some((service, ns.to_string(), name.to_string()))
}

impl russh::server::Handler for Conn {
    type Error = anyhow::Error;

    async fn auth_publickey(&mut self, _user: &str, key: VerifiedPublicKey<'_>) -> Result<Auth, Self::Error> {
        let fp = fingerprint(key.public_key);
        let user = sqlx::query_as::<_, User>(
            "UPDATE ssh_keys k SET last_used_at = now() FROM users u WHERE k.fingerprint = $1 AND u.id = k.user_id \
             AND NOT u.disabled RETURNING u.id, u.email, u.name",
        )
        .bind(&fp)
        .fetch_optional(&self.state.db)
        .await?;
        match user {
            Some(u) => {
                tracing::debug!(peer = ?self.peer, user = %u.email, "ssh auth ok");
                self.user = Some(u);
                Ok(Auth::Accept)
            }
            _ => Ok(Auth::reject()),
        }
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn env_request(
        &mut self,
        _channel: ChannelId,
        name: &str,
        value: &str,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if name == "GIT_PROTOCOL" && value.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '=' | ':' | '.')) {
            self.git_protocol = Some(value.to_string());
        }
        Ok(())
    }

    async fn shell_request(&mut self, channel_id: ChannelId, session: &mut Session) -> Result<(), Self::Error> {
        session.channel_success(channel_id)?;
        if let Some(channel) = self.channels.remove(&channel_id) {
            let who = self.user.as_ref().map(|u| u.email.clone()).unwrap_or_default();
            tokio::spawn(async move {
                let msg = format!(
                    "Hi {who}! You've successfully authenticated to rust-afariano, but it does not provide shell access.\r\n"
                );
                let _ = channel.extended_data_bytes(1, msg.into_bytes()).await;
                let _ = channel.exit_status(1).await;
                let _ = channel.eof().await;
                let _ = channel.close().await;
            });
        }
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel_id: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let Some(user) = self.user.clone() else {
            session.channel_failure(channel_id)?;
            return Ok(());
        };
        let command = String::from_utf8_lossy(data).to_string();
        let Some(channel) = self.channels.remove(&channel_id) else {
            session.channel_failure(channel_id)?;
            return Ok(());
        };
        let Some((service, ns, name)) = parse_command(&command) else {
            session.channel_success(channel_id)?;
            tokio::spawn(fail(channel, format!("rust-afariano only runs git fetch/push (got `{command}`).\n")));
            return Ok(());
        };
        let Some(repo) = self.state.repo_by_path(&ns, &name).await? else {
            session.channel_success(channel_id)?;
            tokio::spawn(fail(channel, format!("Repository {ns}/{name} not found.\n")));
            return Ok(());
        };
        session.channel_success(channel_id)?;
        let state = self.state.clone();
        let protocol = self.git_protocol.clone();
        tokio::spawn(async move {
            if let Err(e) = run_git(state, channel, service, repo, user, protocol).await {
                tracing::warn!("ssh git {service}: {e:#}");
            }
        });
        Ok(())
    }
}

async fn fail(channel: Channel<Msg>, msg: String) {
    let _ = channel.extended_data_bytes(1, msg.into_bytes()).await;
    let _ = channel.exit_status(128).await;
    let _ = channel.eof().await;
    let _ = channel.close().await;
}

async fn run_git(
    state: Shared,
    channel: Channel<Msg>,
    service: &'static str,
    repo: crate::models::Repo,
    user: User,
    protocol: Option<String>,
) -> anyhow::Result<()> {
    let permit = match service {
        "upload-pack" => Some(state.pack_slots.clone().acquire_owned().await?),
        _ => None,
    };
    let push = if service == "receive-pack" {
        let guard = state.push_lock(repo.id).lock_owned().await;
        Some((guard, state.git(repo.id).ref_map().await?))
    } else {
        None
    };
    let mut cmd = git_cmd(&state.cfg.data_dir);
    cmd.arg(service).arg(repo_dir(&state.cfg.data_dir, repo.id));
    if let Some(p) = &protocol {
        cmd.env("GIT_PROTOCOL", p);
    }
    let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let mut stdin = child.stdin.take().expect("piped");
    let mut stdout = child.stdout.take().expect("piped");
    let mut stderr = child.stderr.take().expect("piped");

    let (mut read, write) = channel.split();
    let mut out_writer = write.make_writer();
    let mut err_writer = write.make_writer_ext(Some(1));
    let input = tokio::spawn(async move {
        let mut reader = read.make_reader();
        let _ = tokio::io::copy(&mut reader, &mut stdin).await;
        let _ = stdin.shutdown().await;
    });
    let output = async {
        let _ = tokio::io::copy(&mut stdout, &mut out_writer).await;
        let _ = out_writer.flush().await;
    };
    let errors = async {
        let _ = tokio::io::copy(&mut stderr, &mut err_writer).await;
        let _ = err_writer.flush().await;
    };
    tokio::join!(output, errors);
    let status = child.wait().await?;
    input.abort();
    drop(permit);
    let code = status.code().unwrap_or(128) as u32;
    let _ = write.exit_status(code).await;
    let _ = write.eof().await;
    let _ = write.close().await;
    if let Some((guard, before)) = push {
        after_push(state, repo, before, user).await;
        drop(guard);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_command;

    #[test]
    fn commands() {
        let p = |s| parse_command(s).map(|(a, b, c)| format!("{a} {b}/{c}"));
        assert_eq!(p("git-upload-pack '/acme/website.git'").as_deref(), Some("upload-pack acme/website"));
        assert_eq!(p("git-receive-pack 'acme/website'").as_deref(), Some("receive-pack acme/website"));
        assert_eq!(p("git upload-pack 'hw/x.git'").as_deref(), Some("upload-pack hw/x"));
        assert_eq!(p("git-upload-archive 'hw/x.git'"), None);
        assert_eq!(p("rm -rf /"), None);
        assert_eq!(p("git-upload-pack '../../etc/passwd'"), None);
        assert_eq!(p("git-upload-pack 'a/b/c'"), None);
    }
}
