use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};

pub type AppResult<T> = Result<T, AppError>;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("not found")]
    NotFound,
    #[error("forbidden")]
    Forbidden,
    #[error("{0}")]
    BadRequest(String),
    /// Git over HTTPS without (valid) credentials: asks the client for them.
    #[error("authentication required")]
    GitAuth,
    #[error("unauthorized")]
    Unauthorized,
    /// Another server we depend on (a mirror's upstream) failed.
    #[error("{0}")]
    BadGateway(String),
    /// A git request refused with a reason the git client prints (`remote: …`).
    #[error("{0}")]
    GitRefused(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl From<sqlx::Error> for AppError {
    fn from(e: sqlx::Error) -> Self {
        match e {
            sqlx::Error::RowNotFound => Self::NotFound,
            other => Self::Internal(other.into()),
        }
    }
}

impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self {
        Self::Internal(e.into())
    }
}

impl From<askama::Error> for AppError {
    fn from(e: askama::Error) -> Self {
        Self::Internal(e.into())
    }
}

pub fn bad(msg: impl Into<String>) -> AppError {
    AppError::BadRequest(msg.into())
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, msg) = match &self {
            Self::NotFound => (StatusCode::NOT_FOUND, "Not found".to_string()),
            Self::Forbidden => (StatusCode::FORBIDDEN, "Forbidden".to_string()),
            Self::BadRequest(m) => (StatusCode::BAD_REQUEST, m.clone()),
            Self::BadGateway(m) => (StatusCode::BAD_GATEWAY, m.clone()),
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "Unauthorized".to_string()),
            Self::GitRefused(m) => {
                return (
                    StatusCode::FORBIDDEN,
                    [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                    format!("{m}\n"),
                )
                    .into_response();
            }
            Self::GitAuth => {
                return (
                    StatusCode::UNAUTHORIZED,
                    [(header::WWW_AUTHENTICATE, "Basic realm=\"rust-afariano\"")],
                    "Authentication required: use a personal access token as password\n",
                )
                    .into_response();
            }
            Self::Internal(e) => {
                tracing::error!("internal error: {e:#}");
                (StatusCode::INTERNAL_SERVER_ERROR, "Internal error".to_string())
            }
        };
        let body = format!(
            "<!doctype html><html lang=en><meta charset=utf-8><meta name=viewport content='width=device-width, initial-scale=1'>\
             <title>{code} · rust-afariano</title><link rel=icon href=/-/static/favicon.png type=image/png>\
             <link rel=stylesheet href='/-/static/app.css?v={v}'><main class=center-page><div class=signin-card>\
             <img src=/-/static/logo.png alt=''><h1>{title}</h1><p>{msg}</p>\
             <a class='btn' href='/'>Back to repositories</a></div></main></html>",
            code = status.as_u16(),
            v = crate::web::ASSET_V,
            title = match status.as_u16() {
                404 => "Not found",
                403 => "Not allowed",
                401 => "Sign in needed",
                400 => "Something is off",
                _ => "Server error",
            },
            msg = html_escape(&msg),
        );
        let mut resp = (status, [(header::CONTENT_TYPE, "text/html; charset=utf-8")], body).into_response();
        resp.extensions_mut().insert(ErrorMessage(msg));
        resp
    }
}

/// The plain message of an error response, for `/api` routes to turn into JSON.
#[derive(Clone)]
pub struct ErrorMessage(pub String);

/// `/api/*` errors as `{"message": …}` instead of the HTML error page.
pub async fn json_errors(resp: Response) -> Response {
    let Some(ErrorMessage(msg)) = resp.extensions().get::<ErrorMessage>().cloned() else {
        return resp;
    };
    let (mut parts, _) = resp.into_parts();
    parts.headers.insert(header::CONTENT_TYPE, header::HeaderValue::from_static("application/json"));
    parts.headers.remove(header::CONTENT_LENGTH);
    Response::from_parts(parts, axum::body::Body::from(serde_json::json!({ "message": msg }).to_string()))
}

pub fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}
