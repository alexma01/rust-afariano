use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, bail};

/// Every setting comes from the environment (or the same-named flag).
#[derive(clap::Parser, Debug, Clone)]
#[command(name = "rust-afariano", version, about = "rust-afariano: git hosting and CI pipelines in a few MB of RAM")]
pub struct Config {
    /// Postgres URL, e.g. postgres://rust_afariano:…@localhost:5432/rust_afariano
    #[arg(long, env = "RUST_AFARIANO_DATABASE_URL")]
    pub database_url: String,

    /// Public base URL, e.g. https://git.example.com (no trailing slash).
    #[arg(long, env = "RUST_AFARIANO_PUBLIC_URL")]
    pub public_url: String,

    /// Sign in with email + password (accounts created by an admin, or the first
    /// admin through the setup link printed at startup).
    #[arg(long, env = "RUST_AFARIANO_PASSWORD_LOGIN", default_value_t = true, action = clap::ArgAction::Set)]
    pub password_login: bool,

    /// OpenID Connect issuer URL (e.g. https://id.example.com/realms/main).
    #[arg(long, env = "RUST_AFARIANO_OIDC_ISSUER")]
    pub oidc_issuer: Option<String>,

    #[arg(long, env = "RUST_AFARIANO_OIDC_CLIENT_ID")]
    pub oidc_client_id: Option<String>,

    /// Empty for a public client (PKCE only).
    #[arg(long, env = "RUST_AFARIANO_OIDC_CLIENT_SECRET", hide_env_values = true)]
    pub oidc_client_secret: Option<String>,

    /// Label of the single sign-on button.
    #[arg(long, env = "RUST_AFARIANO_OIDC_NAME", default_value = "single sign-on")]
    pub oidc_name: String,

    /// Base URL of a "Rich" auth server (auth.hartwellbridge.com and compatible):
    /// a PKCE login that is not standard OIDC.
    #[arg(long, env = "RUST_AFARIANO_RICH_AUTH_URL")]
    pub rich_auth_url: Option<String>,

    #[arg(long, env = "RUST_AFARIANO_RICH_AUTH_NAME", default_value = "Hartwell Bridge")]
    pub rich_auth_name: String,

    /// Comma separated emails (`ana@example.com`) or domains (`*@example.com`)
    /// that get an account automatically on their first single sign-on. Others
    /// need an account created by an admin first.
    #[arg(long, env = "RUST_AFARIANO_ALLOWED_EMAILS", value_delimiter = ',')]
    pub allowed_emails: Vec<String>,

    /// 32-byte key (base64) that encrypts CI/CD variables at rest. When unset, a
    /// key is generated in the data directory: back that file up with the database.
    #[arg(long, env = "RUST_AFARIANO_SECRET_KEY", hide_env_values = true)]
    pub secret_key: Option<String>,

    /// Repositories, logs, the SSH host key and the generated secret key live here.
    #[arg(long, env = "RUST_AFARIANO_DATA_DIR", default_value = "./data")]
    pub data_dir: PathBuf,

    #[arg(long, env = "RUST_AFARIANO_HTTP_ADDR", default_value = "127.0.0.1:8080")]
    pub http_addr: SocketAddr,

    /// Empty disables the SSH server.
    #[arg(long, env = "RUST_AFARIANO_SSH_ADDR", default_value = "0.0.0.0:2222")]
    pub ssh_addr: String,

    /// Port shown in SSH clone URLs (differs from the bind port behind NAT).
    #[arg(long, env = "RUST_AFARIANO_SSH_PUBLIC_PORT", default_value_t = 2222)]
    pub ssh_public_port: u16,

    /// Concurrent clones/repacks (each can take ~50 MB).
    #[arg(long, env = "RUST_AFARIANO_MAX_PACKS", default_value_t = 2)]
    pub max_packs: usize,

    /// Postgres pool size.
    #[arg(long, env = "RUST_AFARIANO_DB_POOL", default_value_t = 5)]
    pub db_pool: u32,

    /// Session lifetime in hours.
    #[arg(long, env = "RUST_AFARIANO_SESSION_TTL_HOURS", default_value_t = 24 * 30)]
    pub session_ttl_hours: i64,
}

fn trim_url(u: &mut Option<String>) {
    *u = u.take().map(|s| s.trim().trim_end_matches('/').to_string()).filter(|s| !s.is_empty());
}

impl Config {
    pub fn validate(mut self) -> anyhow::Result<Self> {
        self.public_url = self.public_url.trim_end_matches('/').to_string();
        trim_url(&mut self.oidc_issuer);
        trim_url(&mut self.rich_auth_url);
        self.oidc_client_id = self.oidc_client_id.take().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        self.oidc_client_secret = self.oidc_client_secret.take().filter(|s| !s.is_empty());
        self.allowed_emails =
            self.allowed_emails.iter().map(|e| e.trim().to_lowercase()).filter(|e| !e.is_empty()).collect();
        if !(self.public_url.starts_with("https://") || self.public_url.starts_with("http://")) {
            bail!("RUST_AFARIANO_PUBLIC_URL must start with http:// or https://");
        }
        if self.oidc_issuer.is_some() != self.oidc_client_id.is_some() {
            bail!("set RUST_AFARIANO_OIDC_ISSUER and RUST_AFARIANO_OIDC_CLIENT_ID together");
        }
        if !self.password_login && self.oidc_issuer.is_none() && self.rich_auth_url.is_none() {
            bail!("no way to sign in: enable RUST_AFARIANO_PASSWORD_LOGIN or configure OIDC");
        }
        if self.allowed_emails.iter().any(|e| e == "*") {
            tracing::warn!(
                "RUST_AFARIANO_ALLOWED_EMAILS contains `*`: anyone your identity provider accepts gets an account"
            );
        }
        std::fs::create_dir_all(self.data_dir.join("repos"))
            .and_then(|_| std::fs::create_dir_all(self.data_dir.join("logs")))
            .with_context(|| format!("creating {}", self.data_dir.display()))?;
        self.data_dir = self.data_dir.canonicalize()?;
        Ok(self)
    }

    pub fn oidc_enabled(&self) -> bool {
        self.oidc_issuer.is_some() && self.oidc_client_id.is_some()
    }

    pub fn secure_cookies(&self) -> bool {
        self.public_url.starts_with("https://")
    }

    /// `git.example.com` (host of the public URL, without port).
    pub fn public_host(&self) -> &str {
        let rest = self.public_url.split("://").nth(1).unwrap_or(&self.public_url);
        let host_port = rest.split('/').next().unwrap_or(rest);
        host_port.split(':').next().unwrap_or(host_port)
    }

    /// May this email get an account on its first single sign-on?
    pub fn email_allowed(&self, email: &str) -> bool {
        let email = email.trim().to_lowercase();
        let domain = email.rsplit_once('@').map(|(_, d)| d).unwrap_or("");
        self.allowed_emails
            .iter()
            .any(|p| p == "*" || *p == email || p.strip_prefix("*@").is_some_and(|d| !domain.is_empty() && d == domain))
    }

    pub fn ssh_clone_url(&self, path: &str) -> String {
        if self.ssh_public_port == 22 {
            format!("git@{}:{path}.git", self.public_host())
        } else {
            format!("ssh://git@{}:{}/{path}.git", self.public_host(), self.ssh_public_port)
        }
    }

    pub fn http_clone_url(&self, path: &str) -> String {
        format!("{}/{path}.git", self.public_url)
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::Config;

    fn cfg(allowed: &str) -> Config {
        Config::parse_from([
            "x",
            "--database-url",
            "postgres://x",
            "--public-url",
            "http://h",
            "--allowed-emails",
            allowed,
        ])
    }

    #[test]
    fn allow_list_patterns() {
        let c = cfg("ana@example.com,*@corp.example");
        assert!(c.email_allowed("Ana@Example.com"));
        assert!(c.email_allowed("bob@corp.example"));
        assert!(!c.email_allowed("bob@evil.corp.example"));
        assert!(!c.email_allowed("eve@example.com"));
        assert!(cfg("*").email_allowed("anyone@anywhere.io"));
    }
}
