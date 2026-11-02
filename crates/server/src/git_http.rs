//! Git smart HTTP (`info/refs`, `git-upload-pack`, `git-receive-pack`), streamed
//! straight between the socket and a `git … --stateless-rpc` process.

use std::io::Read;
use std::process::Stdio;

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::prelude::*;
use futures_util::StreamExt;
use rust_afariano_proto as proto;
use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;

use crate::access::{self, Role};
use crate::app::Shared;
use crate::auth;
use crate::error::{AppError, AppResult, bad};
use crate::git::{git_cmd, repo_dir};
use crate::models::{Repo, User};
use crate::push::after_push;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Service {
    Upload,
    Receive,
}

impl Service {
    fn name(self) -> &'static str {
        match self {
            Self::Upload => "upload-pack",
            Self::Receive => "receive-pack",
        }
    }
}

enum Principal {
    User(User),
    /// A running job's token: read-only, its own repo only.
    Job {
        repo_id: i64,
    },
}

/// Credentials first, repository second: without valid credentials every path
/// answers 401, so repository names cannot be probed.
async fn access(
    state: &Shared,
    headers: &HeaderMap,
    ns: &str,
    name: &str,
    service: Service,
) -> AppResult<(Repo, Principal)> {
    let principal = authenticate(state, headers).await?;
    let repo = state.repo_by_path(ns, name).await?;
    match (&principal, repo) {
        (Principal::User(user), Some(repo)) => {
            let need = if service == Service::Receive { Role::Write } else { Role::Read };
            access::require(state, user, &repo, need).await?;
            Ok((repo, principal))
        }
        (Principal::User(_), None) => Err(AppError::NotFound),
        (Principal::Job { repo_id }, Some(repo)) if *repo_id == repo.id && service == Service::Upload => {
            Ok((repo, principal))
        }
        (Principal::Job { .. }, _) => Err(AppError::Forbidden),
    }
}

async fn authenticate(state: &Shared, headers: &HeaderMap) -> AppResult<Principal> {
    let Some(raw) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) else {
        return Err(AppError::GitAuth);
    };
    let password = if let Some(b64) = raw.strip_prefix("Basic ") {
        let decoded = BASE64_STANDARD.decode(b64.trim()).map_err(|_| AppError::GitAuth)?;
        let decoded = String::from_utf8(decoded).map_err(|_| AppError::GitAuth)?;
        decoded.split_once(':').map(|(_, p)| p.to_string()).ok_or(AppError::GitAuth)?
    } else if let Some(t) = raw.strip_prefix("Bearer ") {
        t.trim().to_string()
    } else {
        return Err(AppError::GitAuth);
    };
    if password.starts_with(proto::JOB_TOKEN_PREFIX) {
        let Some(job) = auth::job_by_token(state, &password).await? else {
            return Err(AppError::GitAuth);
        };
        return Ok(Principal::Job { repo_id: job.repo_id });
    }
    match auth::user_by_pat(state, &password).await? {
        Some(u) => Ok(Principal::User(u)),
        None => Err(AppError::GitAuth),
    }
}

fn pkt_line(s: &str) -> Vec<u8> {
    format!("{:04x}{s}", s.len() + 4).into_bytes()
}

fn git_protocol(headers: &HeaderMap) -> Option<String> {
    headers
        .get("git-protocol")
        .and_then(|v| v.to_str().ok())
        .filter(|v| v.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '=' | ':' | '.' | '-' | '_')))
        .map(String::from)
}

#[derive(Deserialize)]
pub struct InfoRefsQuery {
    service: Option<String>,
}

pub async fn info_refs(
    State(state): State<Shared>,
    Path((ns, name)): Path<(String, String)>,
    Query(q): Query<InfoRefsQuery>,
    headers: HeaderMap,
) -> AppResult<Response> {
    let service = match q.service.as_deref() {
        Some("git-upload-pack") => Service::Upload,
        Some("git-receive-pack") => Service::Receive,
        _ => return Err(bad("This server only speaks the smart HTTP protocol (git ≥ 1.6.6).")),
    };
    let (repo, _principal) = access(&state, &headers, &ns, &name, service).await?;
    let proto_header = git_protocol(&headers);
    let v2 = proto_header.as_deref().is_some_and(|p| p.contains("version=2"));
    let mut cmd = git_cmd(&state.cfg.data_dir);
    cmd.arg(service.name()).args(["--stateless-rpc", "--advertise-refs"]).arg(repo_dir(&state.cfg.data_dir, repo.id));
    if let Some(p) = &proto_header {
        cmd.env("GIT_PROTOCOL", p);
    }
    let out = cmd.stdin(Stdio::null()).stderr(Stdio::null()).output().await?;
    if !out.status.success() {
        return Err(AppError::Internal(anyhow::anyhow!("git {} --advertise-refs failed", service.name())));
    }
    let mut body = Vec::with_capacity(out.stdout.len() + 64);
    if !v2 {
        body.extend(pkt_line(&format!("# service=git-{}\n", service.name())));
        body.extend(b"0000");
    }
    body.extend(out.stdout);
    Ok((
        [
            (header::CONTENT_TYPE, format!("application/x-git-{}-advertisement", service.name())),
            (header::CACHE_CONTROL, "no-cache, max-age=0, must-revalidate".to_string()),
        ],
        body,
    )
        .into_response())
}

pub async fn upload_pack(
    State(state): State<Shared>,
    Path((ns, name)): Path<(String, String)>,
    headers: HeaderMap,
    body: Body,
) -> AppResult<Response> {
    rpc(state, ns, name, headers, body, Service::Upload).await
}

pub async fn receive_pack(
    State(state): State<Shared>,
    Path((ns, name)): Path<(String, String)>,
    headers: HeaderMap,
    body: Body,
) -> AppResult<Response> {
    rpc(state, ns, name, headers, body, Service::Receive).await
}

async fn rpc(
    state: Shared,
    ns: String,
    name: String,
    headers: HeaderMap,
    body: Body,
    service: Service,
) -> AppResult<Response> {
    let (repo, principal) = access(&state, &headers, &ns, &name, service).await?;
    let expected = format!("application/x-git-{}-request", service.name());
    if headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()) != Some(expected.as_str()) {
        return Err(bad("unexpected content type"));
    }
    let gzip = headers.get(header::CONTENT_ENCODING).and_then(|v| v.to_str().ok()).is_some_and(|v| v.contains("gzip"));

    // Packing is what eats memory: bound concurrent upload-packs.
    let permit = match service {
        Service::Upload => Some(state.pack_slots.clone().acquire_owned().await.map_err(|e| anyhow::anyhow!(e))?),
        Service::Receive => None,
    };
    let push = match (&principal, service) {
        (Principal::User(user), Service::Receive) => {
            let guard = state.push_lock(repo.id).lock_owned().await;
            let before = state.git(repo.id).ref_map().await?;
            Some((guard, before, user.clone()))
        }
        _ => None,
    };

    let mut cmd = git_cmd(&state.cfg.data_dir);
    cmd.arg(service.name()).arg("--stateless-rpc").arg(repo_dir(&state.cfg.data_dir, repo.id));
    if let Some(p) = git_protocol(&headers) {
        cmd.env("GIT_PROTOCOL", p);
    }
    let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()?;
    let mut stdin = child.stdin.take().expect("piped");
    let stdout = child.stdout.take().expect("piped");

    tokio::spawn(async move {
        let result: anyhow::Result<()> = async {
            if gzip {
                let raw = axum::body::to_bytes(body, 256 << 20).await?;
                let plain = tokio::task::spawn_blocking(move || {
                    let mut out = Vec::new();
                    flate2::read::GzDecoder::new(&raw[..]).take(1 << 30).read_to_end(&mut out).map(|_| out)
                })
                .await??;
                stdin.write_all(&plain).await?;
            } else {
                let mut stream = body.into_data_stream();
                while let Some(chunk) = stream.next().await {
                    stdin.write_all(&chunk?).await?;
                }
            }
            stdin.shutdown().await?;
            Ok(())
        }
        .await;
        if let Err(e) = result {
            tracing::debug!("git request body: {e:#}");
        }
    });

    let state2 = state.clone();
    tokio::spawn(async move {
        let _ = child.wait().await;
        drop(permit);
        if let Some((guard, before, user)) = push {
            after_push(state2, repo, before, user).await;
            drop(guard);
        }
    });

    // Answer only once git has output. A reverse proxy streaming the request body
    // (nginx with `proxy_request_buffering off`) stops forwarding it as soon as it has
    // passed the response header on, so a push larger than what had arrived by then
    // stalled halfway, with index-pack waiting and the push lock held. upload-pack and
    // receive-pack write nothing before they have read their whole request.
    let mut out = ReaderStream::new(stdout);
    let first = out.next().await;
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, format!("application/x-git-{}-result", service.name())),
            (header::CACHE_CONTROL, "no-cache".to_string()),
        ],
        Body::from_stream(futures_util::stream::iter(first).chain(out)),
    )
        .into_response())
}
