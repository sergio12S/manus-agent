//! Wallet file encryption and the local, human-only parts of setup.
//!
//! A wallet file is `salt(16) || nonce(12) || AES-256-GCM(ciphertext)`, keyed with
//! PBKDF2-HMAC-SHA256 over the password with 600,000 iterations.

use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Nonce};
use pbkdf2::pbkdf2_hmac;
use rand::RngCore;
use sha2::Sha256;
use std::io::Write;
use zeroize::Zeroizing;

const PBKDF2_ROUNDS: u32 = 600_000;

pub fn derive_key(password: &str, salt: &[u8; 16]) -> [u8; 32] {
    let mut key = [0u8; 32];
    pbkdf2_hmac::<Sha256>(password.as_bytes(), salt, PBKDF2_ROUNDS, &mut key);
    key
}

pub fn encrypt_mnemonic(mnemonic: &str, password: &str) -> anyhow::Result<Vec<u8>> {
    let mut salt = [0u8; 16];
    OsRng.fill_bytes(&mut salt);
    let key = Zeroizing::new(derive_key(password, &salt));
    let cipher = Aes256Gcm::new_from_slice(key.as_ref())?;
    let mut nonce = [0u8; 12];
    OsRng.fill_bytes(&mut nonce);
    let ciphertext = cipher
        .encrypt(&Nonce::from(nonce), mnemonic.as_bytes())
        .map_err(|e| anyhow::anyhow!("Encryption failed: {:?}", e))?;
    let mut result = salt.to_vec();
    result.extend_from_slice(&nonce);
    result.extend_from_slice(&ciphertext);
    Ok(result)
}

pub fn decrypt_mnemonic(data: &[u8], password: &str) -> anyhow::Result<String> {
    if data.len() < 16 + 12 {
        return Err(anyhow::anyhow!("Invalid encrypted data"));
    }
    let (salt_slice, rest) = data.split_at(16);
    let (nonce_slice, ciphertext) = rest.split_at(12);
    let mut salt = [0u8; 16];
    salt.copy_from_slice(salt_slice);
    let key = Zeroizing::new(derive_key(password, &salt));
    let cipher = Aes256Gcm::new_from_slice(key.as_ref())?;
    let mut nonce = [0u8; 12];
    nonce.copy_from_slice(nonce_slice);
    let plaintext = cipher
        .decrypt(&Nonce::from(nonce), ciphertext)
        .map_err(|e| anyhow::anyhow!("Decryption failed: {:?}. Is password correct?", e))?;
    Ok(String::from_utf8(plaintext)?)
}

/// Why a password is too weak to protect a wallet, if it is.
pub fn weak_secret_reason(secret: &str) -> Option<&'static str> {
    let s = secret.trim();
    if s.is_empty() {
        return Some("empty");
    }
    if s.len() < 12 {
        return Some("too_short");
    }
    let lower = s.to_ascii_lowercase();
    let weak_markers = [
        "password", "changeme", "12345", "qwerty", "test", "dummy", "example", "default",
    ];
    if weak_markers.iter().any(|m| lower.contains(m)) {
        return Some("contains_weak_pattern");
    }
    None
}

/// Ask twice for a new wallet password on the terminal.
pub fn prompt_new_password() -> anyhow::Result<Zeroizing<String>> {
    let password = Zeroizing::new(rpassword::prompt_password("Create encryption password: ")?);
    if let Some(reason) = weak_secret_reason(&password) {
        return Err(anyhow::anyhow!(
            "Encryption password is not strong enough ({reason}). Use at least 12 characters and avoid common password patterns."
        ));
    }
    let confirmation = Zeroizing::new(rpassword::prompt_password("Confirm encryption password: ")?);
    if password != confirmation {
        return Err(anyhow::anyhow!("Encryption passwords do not match"));
    }
    Ok(password)
}

pub fn validate_wallet_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        return Err(anyhow::anyhow!(
            "Wallet name may contain only ASCII letters, digits, '-' and '_'"
        ));
    }
    Ok(())
}

/// Create a wallet file with mode 0600, refusing to overwrite anything.
pub fn write_new_wallet_file(path: &str, encrypted: &[u8]) -> anyhow::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|error| {
        anyhow::anyhow!(
            "Could not create wallet file '{path}': {error}. Existing wallets are never overwritten."
        )
    })?;
    file.write_all(encrypted)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_wrong_password() {
        let data = encrypt_mnemonic("abandon ability able", "correct horse battery").unwrap();
        assert_eq!(data.len(), 16 + 12 + 20 + 16);
        assert_eq!(
            decrypt_mnemonic(&data, "correct horse battery").unwrap(),
            "abandon ability able"
        );
        assert!(decrypt_mnemonic(&data, "wrong horse battery").is_err());
        assert!(decrypt_mnemonic(&data[..20], "x").is_err());
    }

    #[test]
    fn weak_passwords_and_names_are_refused() {
        assert_eq!(weak_secret_reason("short"), Some("too_short"));
        assert_eq!(
            weak_secret_reason("mypassword-long"),
            Some("contains_weak_pattern")
        );
        assert_eq!(weak_secret_reason("amber-lighthouse-quietly"), None);
        assert!(validate_wallet_name("research_agent-2").is_ok());
        assert!(validate_wallet_name("../etc").is_err());
    }
}
