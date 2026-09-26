//! Wallet unlock secret storage.
//!
//! On macOS the password lives in the login Keychain as an item created by the
//! `manus` binary. Its access list trusts only that binary, so a shell running
//! `security find-generic-password -w` triggers a system prompt for the human
//! instead of silently disclosing the password to an agent.

use zeroize::Zeroizing;

#[cfg(target_os = "macos")]
const SERVICE: &str = "xyz.manuspay.agent-wallet";

pub fn load(wallet: &str) -> anyhow::Result<Option<Zeroizing<String>>> {
    if let Ok(password) = std::env::var("MANUS_PASSWORD") {
        return Ok(Some(Zeroizing::new(password)));
    }
    platform::load(wallet)
}

pub fn store(wallet: &str, password: &str) -> anyhow::Result<()> {
    platform::store(wallet, password)
}

pub fn remove(wallet: &str) -> anyhow::Result<()> {
    platform::remove(wallet)
}

pub fn available() -> bool {
    cfg!(target_os = "macos")
}

#[cfg(target_os = "macos")]
mod platform {
    use super::SERVICE;
    use security_framework::passwords::{
        delete_generic_password, get_generic_password, set_generic_password,
    };
    use zeroize::Zeroizing;

    const NOT_FOUND: i32 = -25300;

    pub fn load(wallet: &str) -> anyhow::Result<Option<Zeroizing<String>>> {
        match get_generic_password(SERVICE, wallet) {
            Ok(bytes) => {
                let bytes = Zeroizing::new(bytes);
                Ok(Some(Zeroizing::new(
                    String::from_utf8(bytes.to_vec())
                        .map_err(|_| anyhow::anyhow!("Keychain item is not valid UTF-8"))?,
                )))
            }
            Err(error) if error.code() == NOT_FOUND => Ok(None),
            Err(error) => Err(anyhow::anyhow!("Keychain read failed: {error}")),
        }
    }

    pub fn store(wallet: &str, password: &str) -> anyhow::Result<()> {
        set_generic_password(SERVICE, wallet, password.as_bytes())
            .map_err(|error| anyhow::anyhow!("Keychain write failed: {error}"))
    }

    pub fn remove(wallet: &str) -> anyhow::Result<()> {
        match delete_generic_password(SERVICE, wallet) {
            Ok(()) => Ok(()),
            Err(error) if error.code() == NOT_FOUND => Ok(()),
            Err(error) => Err(anyhow::anyhow!("Keychain delete failed: {error}")),
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use zeroize::Zeroizing;

    pub fn load(_wallet: &str) -> anyhow::Result<Option<Zeroizing<String>>> {
        Ok(None)
    }

    pub fn store(_wallet: &str, _password: &str) -> anyhow::Result<()> {
        Err(anyhow::anyhow!(
            "No OS keychain integration on this platform; provide MANUS_PASSWORD through your secret manager"
        ))
    }

    pub fn remove(_wallet: &str) -> anyhow::Result<()> {
        Ok(())
    }
}
