//! Administration: accounts. (Runners live in settings.rs, also admin-only.)

use askama::Template;
use axum::Form;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Redirect, Response};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use sqlx::FromRow;

use super::render;
use crate::accounts;
use crate::app::Shared;
use crate::auth::{AdminUser, end_sessions};
use crate::error::{AppError, AppResult};
use crate::models::User;
use crate::util::ago;

#[derive(FromRow)]
struct AccountRow {
    id: i64,
    email: String,
    name: String,
    is_admin: bool,
    disabled: bool,
    has_password: bool,
    created_at: DateTime<Utc>,
    last_login_at: Option<DateTime<Utc>>,
}

struct UserRow {
    a: AccountRow,
    initial: String,
    created: String,
    last_login: String,
    is_me: bool,
}

#[derive(Template)]
#[template(path = "admin_users.html")]
struct UsersPage {
    user: User,
    users: Vec<UserRow>,
    error: String,
    notice: String,
    sso: String,
}

async fn users_page(state: &Shared, user: User, error: &str, notice: &str) -> AppResult<Response> {
    let rows: Vec<AccountRow> = sqlx::query_as(
        "SELECT id, email, name, is_admin, disabled, password_hash IS NOT NULL AS has_password, created_at, last_login_at \
         FROM users ORDER BY is_admin DESC, lower(email)",
    )
    .fetch_all(&state.db)
    .await?;
    let cfg = &state.cfg;
    let sso = match (cfg.oidc_enabled(), cfg.rich_auth_url.is_some()) {
        (true, true) => format!("{} or {}", cfg.oidc_name, cfg.rich_auth_name),
        (true, false) => cfg.oidc_name.clone(),
        (false, true) => cfg.rich_auth_name.clone(),
        _ => String::new(),
    };
    render(UsersPage {
        users: rows
            .into_iter()
            .map(|a| UserRow {
                initial: super::initial(if a.name.is_empty() { &a.email } else { &a.name }),
                created: ago(a.created_at),
                last_login: a.last_login_at.map(ago).unwrap_or_else(|| "never".into()),
                is_me: a.id == user.id,
                a,
            })
            .collect(),
        user,
        error: error.into(),
        notice: notice.into(),
        sso,
    })
}

pub async fn users(State(state): State<Shared>, AdminUser(user): AdminUser) -> AppResult<Response> {
    users_page(&state, user, "", "").await
}

#[derive(Deserialize)]
pub struct NewUserForm {
    email: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    password2: String,
    is_admin: Option<String>,
}

pub async fn create_user(
    State(state): State<Shared>,
    AdminUser(user): AdminUser,
    Form(f): Form<NewUserForm>,
) -> AppResult<Response> {
    let admin = f.is_admin.is_some_and(|v| !v.is_empty());
    let password = Some((f.password.as_str(), f.password2.as_str()));
    match crate::ops::create_account(&state, &user, &f.email, &f.name, password, admin).await? {
        Ok(_) => {
            let email = f.email.trim().to_lowercase();
            users_page(&state, user, "", &format!("Account {email} created.")).await
        }
        Err(e) => users_page(&state, user, e, "").await,
    }
}

#[derive(Deserialize)]
pub struct UserAction {
    action: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    password2: String,
}

pub async fn update_user(
    State(state): State<Shared>,
    AdminUser(user): AdminUser,
    Path(id): Path<i64>,
    Form(f): Form<UserAction>,
) -> AppResult<Response> {
    let target: (String, bool) = sqlx::query_as("SELECT email, is_admin FROM users WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let (email, target_admin) = target;
    let admins: i64 =
        sqlx::query_scalar("SELECT count(*) FROM users WHERE is_admin AND NOT disabled").fetch_one(&state.db).await?;
    let self_action = id == user.id;
    let last_admin = target_admin && admins <= 1;
    let refuse = match f.action.as_str() {
        "remove_admin" | "disable" | "delete" if self_action => Some("You cannot do that to your own account."),
        "remove_admin" | "disable" | "delete" if last_admin => Some("Keep at least one active administrator."),
        _ => None,
    };
    if let Some(msg) = refuse {
        return users_page(&state, user, msg, "").await;
    }
    let notice = match f.action.as_str() {
        "make_admin" => {
            sqlx::query("UPDATE users SET is_admin = TRUE WHERE id = $1").bind(id).execute(&state.db).await?;
            format!("{email} is now an administrator.")
        }
        "remove_admin" => {
            sqlx::query("UPDATE users SET is_admin = FALSE WHERE id = $1").bind(id).execute(&state.db).await?;
            format!("{email} is no longer an administrator.")
        }
        "disable" => {
            sqlx::query("UPDATE users SET disabled = TRUE WHERE id = $1").bind(id).execute(&state.db).await?;
            end_sessions(&state, id).await?;
            format!("{email} is disabled and signed out everywhere.")
        }
        "enable" => {
            sqlx::query("UPDATE users SET disabled = FALSE WHERE id = $1").bind(id).execute(&state.db).await?;
            format!("{email} can sign in again.")
        }
        "set_password" => {
            if let Err(e) = accounts::check_new_password(&f.password, &f.password2) {
                return users_page(&state, user, e, "").await;
            }
            let hash = accounts::hash_password(&state, f.password).await?;
            sqlx::query("UPDATE users SET password_hash = $2 WHERE id = $1")
                .bind(id)
                .bind(hash)
                .execute(&state.db)
                .await?;
            if !self_action {
                end_sessions(&state, id).await?;
            }
            format!("New password set for {email}.")
        }
        "delete" => {
            sqlx::query("DELETE FROM users WHERE id = $1").bind(id).execute(&state.db).await?;
            format!("{email} deleted.")
        }
        _ => return Err(AppError::BadRequest("unknown action".into())),
    };
    tracing::info!(target = %email, by = %user.email, action = %f.action, "account changed");
    users_page(&state, user, "", &notice).await
}

pub async fn redirect_users() -> Response {
    Redirect::to("/-/admin/users").into_response()
}
