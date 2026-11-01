use base64::prelude::*;
use chrono::{DateTime, Utc};
use rand::Rng;
use sha2::{Digest, Sha256};

/// `prefix` + 32 random bytes, base64url.
pub fn new_token(prefix: &str) -> String {
    let mut buf = [0u8; 32];
    rand::rng().fill_bytes(&mut buf);
    format!("{prefix}{}", BASE64_URL_SAFE_NO_PAD.encode(buf))
}

pub fn hash_token(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

pub fn b64url(bytes: &[u8]) -> String {
    BASE64_URL_SAFE_NO_PAD.encode(bytes)
}

pub fn sha256(bytes: &[u8]) -> Vec<u8> {
    Sha256::digest(bytes).to_vec()
}

pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Namespace / repository name: `[A-Za-z0-9][A-Za-z0-9._-]*`, ≤ 64, not `*.git`.
pub fn valid_slug(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else { return false };
    s.len() <= 64
        && first.is_ascii_alphanumeric()
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && !s.ends_with(".git")
        && !s.ends_with('.')
        && !s.contains("..")
}

/// Namespaces that would collide with server routes.
pub fn reserved_namespace(s: &str) -> bool {
    matches!(
        s.to_lowercase().as_str(),
        "-" | "api" | "auth" | "static" | "assets" | "favicon.ico" | "robots.txt" | "healthz"
    )
}

/// `CI_COMMIT_REF_SLUG`: lowercase, non-alphanumerics → `-`, ≤ 63, no edge dashes.
pub fn ref_slug(r: &str) -> String {
    let mut out: String = r.to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    out.truncate(63);
    out.trim_matches('-').to_string()
}

pub fn ago(t: DateTime<Utc>) -> String {
    let secs = (Utc::now() - t).num_seconds().max(0);
    match secs {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} min ago", secs / 60),
        3600..=86_399 => format!("{} h ago", secs / 3600),
        86_400..=2_591_999 => format!("{} d ago", secs / 86_400),
        _ => t.format("%Y-%m-%d").to_string(),
    }
}

pub fn duration(secs: i64) -> String {
    let secs = secs.max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// Simple glob with `*` only (protected branch patterns, environment scopes).
pub fn glob_match(pattern: &str, s: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == s;
    }
    let mut rest = s;
    for (i, part) in parts.iter().enumerate() {
        if i == 0 {
            match rest.strip_prefix(part) {
                Some(r) => rest = r,
                None => return false,
            }
        } else if i == parts.len() - 1 {
            return rest.ends_with(part);
        } else if let Some(pos) = rest.find(part) {
            rest = &rest[pos + part.len()..];
        } else {
            return false;
        }
    }
    true
}

pub fn short_sha(sha: &str) -> &str {
    &sha[..sha.len().min(8)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs() {
        assert!(valid_slug("my-project"));
        assert!(valid_slug("lead.capture_platform"));
        assert!(!valid_slug("-x"));
        assert!(!valid_slug("a/b"));
        assert!(!valid_slug("x.git"));
        assert!(!valid_slug("a..b"));
        assert!(!valid_slug(""));
    }

    #[test]
    fn globs() {
        assert!(glob_match("main", "main"));
        assert!(glob_match("release/*", "release/1.0"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("a*c*e", "abcde"));
        assert!(!glob_match("release/*", "main"));
    }

    #[test]
    fn slug_of_ref() {
        assert_eq!(ref_slug("Feature/New_UI"), "feature-new-ui");
        assert_eq!(ref_slug("-x-"), "x");
    }

    #[test]
    fn tokens() {
        let t = new_token("rfp_");
        assert!(t.starts_with("rfp_") && t.len() > 40);
        assert_eq!(hash_token(&t), hash_token(&t));
        assert!(ct_eq(b"abc", b"abc") && !ct_eq(b"abc", b"abd"));
    }
}
