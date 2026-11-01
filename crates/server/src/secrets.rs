//! CI/CD variable values are stored encrypted (AES-256-GCM). Stored form:
//! `enc:v1:` + base64(nonce ‖ ciphertext). The key comes from
//! `RUST_AFARIANO_SECRET_KEY` or a file generated once in the data directory.

use std::path::Path;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{Context, bail};
use base64::prelude::*;
use rand::Rng;

const PREFIX: &str = "enc:v1:";
const KEY_FILE: &str = "secret.key";

pub struct Cipher {
    aead: Aes256Gcm,
}

impl Cipher {
    pub fn load(env_key: Option<&str>, data_dir: &Path) -> anyhow::Result<Self> {
        let key = match env_key {
            Some(k) => decode_key(k).context("RUST_AFARIANO_SECRET_KEY")?,
            None => {
                let path = data_dir.join(KEY_FILE);
                if path.exists() {
                    decode_key(std::fs::read_to_string(&path)?.trim()).with_context(|| path.display().to_string())?
                } else {
                    let mut key = [0u8; 32];
                    rand::rng().fill_bytes(&mut key);
                    std::fs::write(&path, BASE64_STANDARD.encode(key))?;
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
                    }
                    tracing::warn!(
                        "generated {}: it encrypts CI/CD variables, back it up with the database",
                        path.display()
                    );
                    key.to_vec()
                }
            }
        };
        let aead = Aes256Gcm::new_from_slice(&key).map_err(|_| anyhow::anyhow!("secret key must be 32 bytes"))?;
        Ok(Self { aead })
    }

    pub fn encrypt(&self, plain: &str) -> String {
        let mut nonce = [0u8; 12];
        rand::rng().fill_bytes(&mut nonce);
        let ct = self.aead.encrypt(&Nonce::from(nonce), plain.as_bytes()).expect("AES-GCM encryption");
        let mut out = nonce.to_vec();
        out.extend(ct);
        format!("{PREFIX}{}", BASE64_STANDARD.encode(out))
    }

    /// Values written before encryption existed are returned as they are.
    pub fn decrypt(&self, stored: &str) -> anyhow::Result<String> {
        let Some(b64) = stored.strip_prefix(PREFIX) else {
            return Ok(stored.to_string());
        };
        let raw = BASE64_STANDARD.decode(b64)?;
        if raw.len() < 12 + 16 {
            bail!("encrypted value too short");
        }
        let (nonce, ct) = raw.split_at(12);
        let nonce: [u8; 12] = nonce.try_into().expect("12 bytes");
        let plain = self
            .aead
            .decrypt(&Nonce::from(nonce), ct)
            .map_err(|_| anyhow::anyhow!("cannot decrypt a CI/CD variable: wrong RUST_AFARIANO_SECRET_KEY?"))?;
        Ok(String::from_utf8(plain)?)
    }

    #[cfg(test)]
    pub fn is_encrypted(stored: &str) -> bool {
        stored.starts_with(PREFIX)
    }
}

fn decode_key(s: &str) -> anyhow::Result<Vec<u8>> {
    let key =
        BASE64_STANDARD.decode(s.trim()).or_else(|_| BASE64_URL_SAFE_NO_PAD.decode(s.trim())).context("not base64")?;
    if key.len() != 32 {
        bail!("must decode to 32 bytes (generate one with: openssl rand -base64 32)");
    }
    Ok(key)
}

/// Encrypts values stored in plain text by earlier versions (once, at startup).
pub async fn encrypt_legacy(db: &sqlx::PgPool, cipher: &Cipher) -> anyhow::Result<()> {
    let rows: Vec<(i64, String)> =
        sqlx::query_as("SELECT id, value FROM variables WHERE value NOT LIKE 'enc:v1:%'").fetch_all(db).await?;
    for (id, value) in &rows {
        sqlx::query("UPDATE variables SET value = $2 WHERE id = $1")
            .bind(id)
            .bind(cipher.encrypt(value))
            .execute(db)
            .await?;
    }
    if !rows.is_empty() {
        tracing::info!("encrypted {} CI/CD variables stored in plain text", rows.len());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let dir = std::env::temp_dir().join(format!("raf-secret-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let c = Cipher::load(None, &dir).unwrap();
        let enc = c.encrypt("-----BEGIN KEY-----\nabc\n");
        assert!(Cipher::is_encrypted(&enc));
        assert_ne!(c.encrypt("x"), c.encrypt("x"), "fresh nonce each time");
        assert_eq!(c.decrypt(&enc).unwrap(), "-----BEGIN KEY-----\nabc\n");
        assert_eq!(c.decrypt("legacy plain").unwrap(), "legacy plain");
        // Same key file → same cipher.
        let c2 = Cipher::load(None, &dir).unwrap();
        assert_eq!(c2.decrypt(&enc).unwrap(), "-----BEGIN KEY-----\nabc\n");
        let other = Cipher::load(Some(&BASE64_STANDARD.encode([7u8; 32])), &dir).unwrap();
        assert!(other.decrypt(&enc).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
