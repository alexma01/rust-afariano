//! Local accounts: password hashing (Argon2id), sign-in rate limiting, the
//! first-admin setup token, and turning a single sign-on identity into a user.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use argon2::{Algorithm, Argon2, Params, Version};
use sqlx::FromRow;

use crate::app::AppState;
use crate::util::new_token;

pub const MIN_PASSWORD_LEN: usize = 10;

/// OWASP's Argon2id baseline: 19 MiB, 2 passes. Hashing runs on the blocking
/// pool behind a 2-slot semaphore, so sign-ins cost at most ~40 MB at once.
fn argon() -> Argon2<'static> {
    Argon2::new(Algorithm::Argon2id, Version::V0x13, Params::new(19_456, 2, 1, None).expect("argon2 params"))
}

pub async fn hash_password(state: &AppState, password: String) -> anyhow::Result<String> {
    let _slot = state.hash_slots.acquire().await?;
    tokio::task::spawn_blocking(move || {
        argon().hash_password(password.as_bytes()).map(|h| h.to_string()).map_err(|e| anyhow::anyhow!("hashing: {e}"))
    })
    .await?
}

pub async fn verify_password(state: &AppState, password: String, stored: String) -> bool {
    let Ok(_slot) = state.hash_slots.acquire().await else { return false };
    tokio::task::spawn_blocking(move || {
        PasswordHash::new(&stored).is_ok_and(|h| argon().verify_password(password.as_bytes(), &h).is_ok())
    })
    .await
    .unwrap_or(false)
}

pub fn check_new_password(password: &str, confirm: &str) -> Result<(), &'static str> {
    if password.chars().count() < MIN_PASSWORD_LEN {
        return Err("Use at least 10 characters for the password.");
    }
    if password != confirm {
        return Err("The two passwords do not match.");
    }
    Ok(())
}

/// Failed password sign-ins per email: 5 within 15 minutes, then a pause.
#[derive(Default)]
pub struct Limiter {
    failures: Mutex<HashMap<String, (u32, Instant)>>,
}

const WINDOW: Duration = Duration::from_secs(15 * 60);
const MAX_FAILURES: u32 = 5;

impl Limiter {
    pub fn blocked(&self, key: &str) -> bool {
        let map = self.failures.lock().expect("limiter");
        map.get(key).is_some_and(|(n, since)| *n >= MAX_FAILURES && since.elapsed() < WINDOW)
    }

    pub fn fail(&self, key: &str) {
        let mut map = self.failures.lock().expect("limiter");
        map.retain(|_, (_, since)| since.elapsed() < WINDOW);
        let entry = map.entry(key.to_string()).or_insert((0, Instant::now()));
        entry.0 += 1;
    }

    pub fn clear(&self, key: &str) {
        self.failures.lock().expect("limiter").remove(key);
    }
}

#[allow(dead_code)] // mirrors the table row
#[derive(Debug, Clone, FromRow)]
pub struct Account {
    pub id: i64,
    pub email: String,
    pub name: String,
    pub password_hash: Option<String>,
    pub is_admin: bool,
    pub disabled: bool,
}

pub async fn find_by_email(db: &sqlx::PgPool, email: &str) -> sqlx::Result<Option<Account>> {
    sqlx::query_as(
        "SELECT id, email, name, password_hash, is_admin, disabled FROM users WHERE lower(email) = lower($1)",
    )
    .bind(email.trim())
    .fetch_optional(db)
    .await
}

pub async fn user_count(db: &sqlx::PgPool) -> sqlx::Result<i64> {
    sqlx::query_scalar("SELECT count(*) FROM users").fetch_one(db).await
}

pub async fn create_user(
    db: &sqlx::PgPool,
    email: &str,
    name: &str,
    password_hash: Option<&str>,
    is_admin: bool,
) -> sqlx::Result<i64> {
    sqlx::query_scalar(
        "INSERT INTO users (email, name, password_hash, is_admin) VALUES (lower($1), $2, $3, $4) RETURNING id",
    )
    .bind(email.trim())
    .bind(name.trim())
    .bind(password_hash)
    .bind(is_admin)
    .fetch_one(db)
    .await
}

pub fn valid_email(email: &str) -> bool {
    let email = email.trim();
    let Some((local, domain)) = email.split_once('@') else { return false };
    !local.is_empty() && domain.contains('.') && !domain.starts_with('.') && email.len() <= 254 && !email.contains(' ')
}

/// The user a single sign-on identity maps to, created on first sign-in when the
/// allow-list permits it (the very first user becomes admin). `Ok(Err(msg))` is
/// a refusal to show on the sign-in page.
pub async fn sso_user(state: &AppState, email: &str, name: &str) -> anyhow::Result<Result<i64, String>> {
    let email = email.trim().to_lowercase();
    if let Some(account) = find_by_email(&state.db, &email).await? {
        if account.disabled {
            return Ok(Err("This account is disabled. Ask an administrator.".into()));
        }
        if account.name.is_empty() && !name.trim().is_empty() {
            sqlx::query("UPDATE users SET name = $2 WHERE id = $1")
                .bind(account.id)
                .bind(name.trim())
                .execute(&state.db)
                .await?;
        }
        return Ok(Ok(account.id));
    }
    if !state.cfg.email_allowed(&email) {
        return Ok(Err(format!("{email} has no account here. Ask an administrator to add it.")));
    }
    let first = user_count(&state.db).await? == 0;
    let id = create_user(&state.db, &email, name, None, first).await?;
    if first {
        state.close_setup();
    }
    tracing::info!(email = %email, admin = first, "account created on first sign-in");
    Ok(Ok(id))
}

/// While there are no users, a one-time setup link (printed in the log) lets the
/// person who runs the server create the first admin.
pub async fn open_setup_if_empty(state: &AppState) -> anyhow::Result<()> {
    if user_count(&state.db).await? > 0 {
        return Ok(());
    }
    let token = new_token("");
    let url = format!("{}/-/setup?token={token}", state.cfg.public_url);
    *state.setup_token.lock().expect("setup token") = Some(token);
    tracing::warn!("no accounts yet: create the first admin at {url}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passwords() {
        assert!(check_new_password("short", "short").is_err());
        assert!(check_new_password("long enough!", "long enough?").is_err());
        assert!(check_new_password("long enough!", "long enough!").is_ok());
        let h = argon().hash_password(b"correct horse").unwrap().to_string();
        let parsed = PasswordHash::new(&h).unwrap();
        assert!(argon().verify_password(b"correct horse", &parsed).is_ok());
        assert!(argon().verify_password(b"wrong horse", &parsed).is_err());
    }

    #[test]
    fn limiter() {
        let l = Limiter::default();
        for _ in 0..4 {
            l.fail("a@x.io");
        }
        assert!(!l.blocked("a@x.io"));
        l.fail("a@x.io");
        assert!(l.blocked("a@x.io"));
        assert!(!l.blocked("b@x.io"));
        l.clear("a@x.io");
        assert!(!l.blocked("a@x.io"));
    }

    #[test]
    fn emails() {
        assert!(valid_email("ana@example.com"));
        assert!(!valid_email("ana"));
        assert!(!valid_email("ana@localhost"));
        assert!(!valid_email("a b@example.com"));
    }
}
