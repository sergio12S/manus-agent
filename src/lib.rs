//! Manus agent wallet: a Solana wallet an AI agent operates inside a budget its
//! human sets, with Touch ID approval above it and a receipt for every operation.
//!
//! Everything lives under `~/.manus` (or `MANUS_HOME`) so an MCP client can start
//! the server from any working directory.
//!
//! This crate is the exact agent-wallet code compiled into the official `manus`
//! binary. It is source-available under FSL-1.1-ALv2; see LICENSE.

pub mod approval;
pub mod budget;
pub mod cli;
pub mod connect;
pub mod crypto;
pub mod engine;
pub mod gbrain;
pub mod keychain;
pub mod mcp;
pub mod mints;
pub mod secret;
pub mod store;
pub mod tx;

use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::signature::Keypair;
use std::path::PathBuf;
use zeroize::Zeroizing;

use crate::secret::StoredWalletSecret;

/// Wallet that receives the 0.1% Manus fee on swaps, stake and unstake.
/// Transfers carry no Manus fee.
pub const FEE_WALLET: &str = "8vLghUcHrLQQqRLmQvBEy95sPT2F5Rh3J6BCYkiFquMi";

pub const DEVNET_GENESIS: &str = "EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG";
pub const MAINNET_GENESIS: &str = "5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d";

pub fn home() -> PathBuf {
    std::env::var_os("MANUS_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".manus")))
        .unwrap_or_else(|| PathBuf::from(".manus"))
}

pub fn ensure_home() -> anyhow::Result<PathBuf> {
    let dir = home();
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(dir)
}

pub fn wallet_path(name: &str) -> PathBuf {
    home().join(format!("{name}.wallet.enc"))
}

pub fn store_path(name: &str) -> PathBuf {
    home().join(format!("{name}.agent.db"))
}

pub fn config_path(name: &str) -> PathBuf {
    home().join(format!("{name}.env"))
}

/// Network settings chosen by the human at setup time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkConfig {
    pub cluster: String,
    pub rpc_url: String,
}

impl NetworkConfig {
    pub fn default_rpc(cluster: &str) -> String {
        match cluster {
            "mainnet-beta" => match std::env::var("HELIUS_API_KEY") {
                Ok(key) if !key.is_empty() => {
                    format!("https://mainnet.helius-rpc.com/?api-key={key}")
                }
                _ => "https://api.mainnet-beta.solana.com".into(),
            },
            "localnet" => "http://127.0.0.1:8899".into(),
            _ => "https://api.devnet.solana.com".into(),
        }
    }

    /// Read `~/.manus/<name>.env`; process environment wins over the file.
    pub fn load(name: &str) -> anyhow::Result<Self> {
        let mut values = std::collections::HashMap::new();
        if let Ok(content) = std::fs::read_to_string(config_path(name)) {
            for line in content.lines().map(str::trim) {
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                if let Some((key, value)) = line.split_once('=') {
                    values.insert(key.trim().to_string(), value.trim().to_string());
                }
            }
        }
        let get = |key: &str| std::env::var(key).ok().or_else(|| values.get(key).cloned());
        for key in [
            "HELIUS_API_KEY",
            "JUPITER_API_KEY",
            "MANUS_GBRAIN_URL",
            "MANUS_GBRAIN_KEY",
            "MANUS_APPROVAL",
        ] {
            if std::env::var(key).is_err() {
                if let Some(value) = values.get(key) {
                    std::env::set_var(key, value);
                }
            }
        }
        let cluster = get("MANUS_CLUSTER").unwrap_or_else(|| "devnet".into());
        if !matches!(cluster.as_str(), "devnet" | "localnet" | "mainnet-beta") {
            return Err(anyhow::anyhow!(
                "MANUS_CLUSTER must be devnet, localnet or mainnet-beta (got '{cluster}')"
            ));
        }
        let rpc_url = get("MANUS_RPC_URL")
            .filter(|url| !url.is_empty())
            .unwrap_or_else(|| Self::default_rpc(&cluster));
        Ok(Self { cluster, rpc_url })
    }

    pub fn save(&self, name: &str) -> anyhow::Result<()> {
        let path = config_path(name);
        let mut kept: Vec<String> = std::fs::read_to_string(&path)
            .unwrap_or_default()
            .lines()
            .filter(|line| {
                !line.starts_with("MANUS_CLUSTER=") && !line.starts_with("MANUS_RPC_URL=")
            })
            .map(str::to_string)
            .collect();
        kept.insert(0, format!("MANUS_CLUSTER={}", self.cluster));
        if self.rpc_url != Self::default_rpc(&self.cluster) {
            kept.insert(1, format!("MANUS_RPC_URL={}", self.rpc_url));
        }
        write_private(&path, (kept.join("\n") + "\n").as_bytes())
    }
}

pub fn write_private(path: &std::path::Path, contents: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    let temp = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temp)?;
    file.write_all(contents)?;
    file.sync_all()?;
    std::fs::rename(&temp, path)?;
    Ok(())
}

/// Refuse to operate if the RPC endpoint is not the network the human chose.
pub async fn verify_network(rpc: &RpcClient, cluster: &str) -> anyhow::Result<()> {
    let expected = match cluster {
        "devnet" => DEVNET_GENESIS,
        "mainnet-beta" => MAINNET_GENESIS,
        _ => return Ok(()),
    };
    let actual = rpc.get_genesis_hash().await?.to_string();
    if actual != expected {
        return Err(anyhow::anyhow!(
            "RPC genesis {actual} is not {cluster}; refusing to start"
        ));
    }
    Ok(())
}

pub fn unlock(name: &str, password: &str) -> anyhow::Result<Keypair> {
    let path = wallet_path(name);
    let data = std::fs::read(&path).map_err(|_| {
        anyhow::anyhow!(
            "No agent wallet at {}. Run `manus agent setup` first.",
            path.display()
        )
    })?;
    let plaintext = Zeroizing::new(crate::crypto::decrypt_mnemonic(&data, password)?);
    StoredWalletSecret::decode(&plaintext)?.derive_keypair()
}

/// Open the wallet non-interactively, as an MCP server does.
pub async fn open(name: &str) -> anyhow::Result<engine::AgentWallet> {
    let network = NetworkConfig::load(name)?;
    let password = keychain::load(name)?.ok_or_else(|| {
        anyhow::anyhow!(
            "Wallet password is not in the Keychain. Run `manus agent setup` in a terminal once."
        )
    })?;
    let signer = unlock(name, &password)?;
    drop(password);
    let rpc =
        RpcClient::new_with_commitment(network.rpc_url.clone(), CommitmentConfig::confirmed());
    verify_network(&rpc, &network.cluster).await?;
    let store = store::Store::open(&store_path(name)).await?;
    Ok(engine::AgentWallet::new(
        rpc,
        signer,
        network.cluster,
        store,
        approval::default_approver(),
        gbrain::Gbrain::from_env(),
    ))
}
