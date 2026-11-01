use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use sqlx::PgPool;
use tokio::sync::{Notify, Semaphore};

use crate::accounts::Limiter;
use crate::config::Config;
use crate::logs::LogHub;
use crate::models::{REPO_COLS, Repo};

pub type Shared = Arc<AppState>;

pub struct AppState {
    pub cfg: Config,
    pub db: PgPool,
    pub http: reqwest::Client,
    /// Woken when a job becomes pending: long-polling runners retry.
    pub jobs_ready: Notify,
    pub logs: LogHub,
    /// Bounds concurrent pack building (clones/fetches), the memory-heavy part.
    pub pack_slots: Arc<Semaphore>,
    /// One push at a time per repo, so before/after ref snapshots are exact.
    push_locks: Mutex<HashMap<i64, Arc<tokio::sync::Mutex<()>>>>,
    /// Encrypts CI/CD variable values at rest.
    pub cipher: crate::secrets::Cipher,
    /// One-time first-admin link while there are no accounts.
    pub setup_token: Mutex<Option<String>>,
    /// Failed password sign-ins per email.
    pub limiter: Limiter,
    /// Argon2 hashing is memory hungry: two at a time.
    pub hash_slots: Semaphore,
    /// OpenID provider metadata, fetched on first use.
    pub oidc: tokio::sync::Mutex<Option<crate::auth::OidcProvider>>,
}

impl AppState {
    pub fn new(cfg: Config, db: PgPool) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .user_agent(concat!("rust-afariano/", env!("CARGO_PKG_VERSION")))
            .build()?;
        let cipher = crate::secrets::Cipher::load(cfg.secret_key.as_deref(), &cfg.data_dir)?;
        Ok(Self {
            cipher,
            setup_token: Mutex::new(None),
            limiter: Limiter::default(),
            hash_slots: Semaphore::new(2),
            oidc: tokio::sync::Mutex::new(None),
            logs: LogHub::new(cfg.data_dir.join("logs")),
            pack_slots: Arc::new(Semaphore::new(cfg.max_packs.max(1))),
            cfg,
            db,
            http,
            jobs_ready: Notify::new(),
            push_locks: Mutex::new(HashMap::new()),
        })
    }

    pub fn close_setup(&self) {
        *self.setup_token.lock().expect("setup token") = None;
    }

    pub fn push_lock(&self, repo_id: i64) -> Arc<tokio::sync::Mutex<()>> {
        let mut map = self.push_locks.lock().expect("push lock map");
        map.entry(repo_id).or_default().clone()
    }

    pub async fn repo_by_path(&self, namespace: &str, name: &str) -> sqlx::Result<Option<Repo>> {
        let name = name.strip_suffix(".git").unwrap_or(name);
        sqlx::query_as::<_, Repo>(sql!(
            "SELECT {REPO_COLS} FROM repos WHERE lower(namespace) = lower($1) AND lower(name) = lower($2)"
        ))
        .bind(namespace)
        .bind(name)
        .fetch_optional(&self.db)
        .await
    }

    pub async fn repo_by_id(&self, id: i64) -> sqlx::Result<Repo> {
        sqlx::query_as::<_, Repo>(sql!("SELECT {REPO_COLS} FROM repos WHERE id = $1"))
            .bind(id)
            .fetch_one(&self.db)
            .await
    }

    pub fn git(&self, repo_id: i64) -> crate::git::Git {
        crate::git::Git::new(&self.cfg.data_dir, repo_id)
    }
}
