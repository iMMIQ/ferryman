//! Application-layer AEAD encryption for stored model API keys.
//!
//! When `FERRYMAN_SECRET_KEY` is configured, API keys are encrypted with
//! ChaCha20-Poly1305 before being persisted in SQLite (`enc:v1:<nonce>:<ct>`).
//! Legacy plaintext values are still readable on load so upgrades are seamless.

use anyhow::{anyhow, bail, Context, Result};
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, CHACHA20_POLY1305};
use ring::rand::{SecureRandom, SystemRandom};
use sha2::{Digest, Sha256};
use std::sync::Arc;

const PREFIX: &str = "enc:v1:";
const NONCE_LEN: usize = 12;

#[derive(Clone, Default)]
pub(crate) struct SecretCipher {
    key: Option<Arc<LessSafeKey>>,
}

impl SecretCipher {
    pub(crate) fn from_env() -> Self {
        match std::env::var("FERRYMAN_SECRET_KEY") {
            Ok(secret) if !secret.trim().is_empty() => Self::from_passphrase(secret.trim()),
            _ => Self::default(),
        }
    }

    pub(crate) fn from_passphrase(passphrase: &str) -> Self {
        let digest = Sha256::digest(passphrase.as_bytes());
        let unbound = UnboundKey::new(&CHACHA20_POLY1305, &digest)
            .expect("32-byte key for ChaCha20-Poly1305");
        Self {
            key: Some(Arc::new(LessSafeKey::new(unbound))),
        }
    }

    pub(crate) fn encrypt(&self, plaintext: &str) -> Result<String> {
        if plaintext.is_empty() {
            return Ok(String::new());
        }
        let Some(key) = &self.key else {
            return Ok(plaintext.to_string());
        };
        let mut nonce_bytes = [0u8; NONCE_LEN];
        SystemRandom::new()
            .fill(&mut nonce_bytes)
            .map_err(|_| anyhow!("failed to generate random nonce"))?;
        let nonce = Nonce::assume_unique_for_key(nonce_bytes);
        let mut in_out = plaintext.as_bytes().to_vec();
        key.seal_in_place_append_tag(nonce, Aad::empty(), &mut in_out)
            .map_err(|_| anyhow!("failed to encrypt secret"))?;
        Ok(format!(
            "{PREFIX}{}:{}",
            hex::encode(nonce_bytes),
            hex::encode(in_out)
        ))
    }

    pub(crate) fn decrypt(&self, stored: &str) -> Result<String> {
        let Some(rest) = stored.strip_prefix(PREFIX) else {
            return Ok(stored.to_string());
        };
        let Some(key) = &self.key else {
            bail!("FERRYMAN_SECRET_KEY is required to decrypt stored API keys");
        };
        let (nonce_hex, ct_hex) = rest
            .split_once(':')
            .ok_or_else(|| anyhow!("malformed encrypted secret"))?;
        let mut nonce_bytes = [0u8; NONCE_LEN];
        hex::decode_to_slice(nonce_hex, &mut nonce_bytes)
            .context("invalid encrypted secret nonce")?;
        let mut in_out = hex::decode(ct_hex).context("invalid encrypted secret ciphertext")?;
        let nonce = Nonce::assume_unique_for_key(nonce_bytes);
        let plain = key
            .open_in_place(nonce, Aad::empty(), &mut in_out)
            .map_err(|_| anyhow!("failed to decrypt secret; check FERRYMAN_SECRET_KEY"))?;
        String::from_utf8(plain.to_vec()).context("decrypted secret is not valid UTF-8")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_encrypts_and_decrypts_and_reads_legacy_plaintext() {
        let cipher = SecretCipher::from_passphrase("test-master-secret");
        let encrypted = cipher.encrypt("sk-secret-12345").unwrap();
        assert!(encrypted.starts_with(PREFIX));
        assert!(!encrypted.contains("sk-secret-12345"));
        assert_eq!(cipher.decrypt(&encrypted).unwrap(), "sk-secret-12345");
        assert_eq!(
            cipher.decrypt("sk-legacy-plain").unwrap(),
            "sk-legacy-plain"
        );
        assert_eq!(cipher.encrypt("").unwrap(), "");

        let wrong = SecretCipher::from_passphrase("wrong-secret");
        assert!(wrong.decrypt(&encrypted).is_err());

        let none = SecretCipher::default();
        assert!(none.decrypt(&encrypted).is_err());
        assert_eq!(none.encrypt("plain").unwrap(), "plain");
    }
}
