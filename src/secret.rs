use bip39::Mnemonic;
use serde::{Deserialize, Serialize};
use solana_sdk::{
    derivation_path::DerivationPath,
    signature::{Keypair, SeedDerivable, Signer},
};
use std::str::FromStr;
use zeroize::{Zeroize, ZeroizeOnDrop};

use solana_sdk::pubkey::Pubkey;

const WALLET_SECRET_VERSION: u8 = 1;
const AUTO_DISCOVERY_ACCOUNTS: u32 = 20;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Zeroize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WalletDerivation {
    LegacySeed32,
    Bip44 { path: String },
}

#[derive(Clone, Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
pub struct StoredWalletSecret {
    pub version: u8,
    pub mnemonic: String,
    pub derivation: WalletDerivation,
}

impl StoredWalletSecret {
    pub fn legacy(mnemonic: &Mnemonic) -> Self {
        Self {
            version: WALLET_SECRET_VERSION,
            mnemonic: mnemonic.to_string(),
            derivation: WalletDerivation::LegacySeed32,
        }
    }

    pub fn decode(plaintext: &str) -> anyhow::Result<Self> {
        if plaintext.trim_start().starts_with('{') {
            let stored: Self = serde_json::from_str(plaintext)
                .map_err(|error| anyhow::anyhow!("Invalid wallet secret envelope: {}", error))?;
            if stored.version != WALLET_SECRET_VERSION {
                return Err(anyhow::anyhow!(
                    "Unsupported wallet secret version {}",
                    stored.version
                ));
            }
            Mnemonic::from_str(stored.mnemonic.trim())
                .map_err(|error| anyhow::anyhow!("Invalid stored mnemonic: {}", error))?;
            return Ok(stored);
        }

        let mnemonic = Mnemonic::from_str(plaintext.trim())
            .map_err(|error| anyhow::anyhow!("Invalid legacy mnemonic: {}", error))?;
        Ok(Self::legacy(&mnemonic))
    }

    pub fn encode(&self) -> anyhow::Result<String> {
        Ok(serde_json::to_string(self)?)
    }

    pub fn derive_keypair(&self) -> anyhow::Result<Keypair> {
        let mnemonic = Mnemonic::from_str(self.mnemonic.trim())
            .map_err(|error| anyhow::anyhow!("Invalid stored mnemonic: {}", error))?;
        let seed = mnemonic.to_seed("");
        match &self.derivation {
            WalletDerivation::LegacySeed32 => Keypair::from_seed(&seed[..32])
                .map_err(|error| anyhow::anyhow!("Legacy keypair derivation failed: {}", error)),
            WalletDerivation::Bip44 { path } => {
                let derivation_path = DerivationPath::from_absolute_path_str(path)
                    .map_err(|error| anyhow::anyhow!("Invalid derivation path: {}", error))?;
                Keypair::from_seed_and_derivation_path(&seed, Some(derivation_path))
                    .map_err(|error| anyhow::anyhow!("BIP-44 keypair derivation failed: {}", error))
            }
        }
    }

    pub fn derivation_label(&self) -> &str {
        match &self.derivation {
            WalletDerivation::LegacySeed32 => "legacy_seed32",
            WalletDerivation::Bip44 { path } => path,
        }
    }

    pub fn find_for_expected_address(
        phrase: &str,
        expected_address: &str,
    ) -> anyhow::Result<(Self, Keypair)> {
        let expected = Pubkey::from_str(expected_address)
            .map_err(|error| anyhow::anyhow!("Invalid expected Solana address: {}", error))?;
        let mnemonic = Mnemonic::from_str(phrase.trim())
            .map_err(|error| anyhow::anyhow!("Invalid mnemonic phrase: {}", error))?;

        let legacy = Self::legacy(&mnemonic);
        let keypair = legacy.derive_keypair()?;
        if keypair.pubkey() == expected {
            return Ok((legacy, keypair));
        }

        let mut paths = vec!["m/44'/501'".to_string()];
        for account in 0..AUTO_DISCOVERY_ACCOUNTS {
            paths.push(format!("m/44'/501'/{}'", account));
            paths.push(format!("m/44'/501'/{}'/0'", account));
        }
        for path in paths {
            let stored = Self {
                version: WALLET_SECRET_VERSION,
                mnemonic: mnemonic.to_string(),
                derivation: WalletDerivation::Bip44 { path },
            };
            let keypair = stored.derive_keypair()?;
            if keypair.pubkey() == expected {
                return Ok((stored, keypair));
            }
        }

        Err(anyhow::anyhow!(
            "The phrase did not derive expected address {} using the legacy Manus scheme or standard Solana BIP-44 accounts 0..{}. Nothing was stored.",
            expected,
            AUTO_DISCOVERY_ACCOUNTS - 1
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_MNEMONIC: &str =
        "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

    #[test]
    fn envelope_round_trip_preserves_legacy_derivation() {
        let mnemonic = Mnemonic::from_str(TEST_MNEMONIC).expect("mnemonic");
        let stored = StoredWalletSecret::legacy(&mnemonic);
        let expected_address = stored.derive_keypair().expect("derive").pubkey();
        let decoded =
            StoredWalletSecret::decode(&stored.encode().expect("encode")).expect("decode");
        assert_eq!(decoded.derivation, WalletDerivation::LegacySeed32);
        assert_eq!(
            decoded.derive_keypair().expect("derive").pubkey(),
            expected_address
        );
    }

    #[test]
    fn auto_discovers_standard_bip44_derivation() {
        let stored = StoredWalletSecret {
            version: WALLET_SECRET_VERSION,
            mnemonic: TEST_MNEMONIC.to_string(),
            derivation: WalletDerivation::Bip44 {
                path: "m/44'/501'/0'/0'".to_string(),
            },
        };
        let expected = stored
            .derive_keypair()
            .expect("derive")
            .pubkey()
            .to_string();
        let (discovered, keypair) =
            StoredWalletSecret::find_for_expected_address(TEST_MNEMONIC, &expected)
                .expect("discover");
        assert_eq!(keypair.pubkey().to_string(), expected);
        assert_eq!(discovered.derivation, stored.derivation);
    }

    #[test]
    fn address_mismatch_fails_closed() {
        let unexpected = Keypair::new().pubkey().to_string();
        let error = StoredWalletSecret::find_for_expected_address(TEST_MNEMONIC, &unexpected)
            .err()
            .expect("must reject mismatch");
        assert!(error.to_string().contains("Nothing was stored"));
    }
}
