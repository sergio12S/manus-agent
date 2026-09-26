//! `manus agent …`: set up, connect and operate the agent wallet.

use crate::{
    self as agent_wallet, approval::ApprovalOutcome, budget::parse_units, connect, crypto,
    keychain, NetworkConfig,
};
use clap::Subcommand;
use solana_sdk::signature::Signer;
use std::sync::Arc;
use zeroize::Zeroizing;

#[derive(Subcommand)]
pub enum AgentAction {
    /// Create (or bring) the agent wallet, keep its password in the Keychain, pick a network
    Setup {
        /// devnet (default) or mainnet-beta
        #[arg(long, default_value = "devnet")]
        cluster: String,
        /// Custom RPC endpoint (defaults to Helius when HELIUS_API_KEY is set)
        #[arg(long)]
        rpc_url: Option<String>,
        /// Import an existing recovery phrase through a hidden prompt
        #[arg(long)]
        import: bool,
        /// With --import: the address the phrase must derive
        #[arg(long)]
        expected_address: Option<String>,
        /// Adopt an existing encrypted wallet file (e.g. ./my-agent.wallet.enc)
        #[arg(long)]
        from_file: Option<String>,
    },
    /// Serve the wallet to an agent over MCP stdio (clients launch this)
    Mcp,
    /// Register the wallet with an agent client: claude, codex or gemini
    Connect { client: String },
    /// Address, balances, budget and today's spending
    Status,
    /// Recent operations and receipts
    History {
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },
    /// Show the budget, or change it; widening asks for Touch ID
    Budget {
        /// Max SOL per transfer
        #[arg(long)]
        send_per_op: Option<String>,
        /// Max SOL transferred per 24h
        #[arg(long)]
        send_daily: Option<String>,
        /// Max SOL per swap or stake
        #[arg(long)]
        convert_per_op: Option<String>,
        /// Max SOL swapped or staked per 24h
        #[arg(long)]
        convert_daily: Option<String>,
        /// Recipient that is always allowed inside the limits (repeatable)
        #[arg(long)]
        trust: Vec<String>,
        #[arg(long)]
        max_slippage_bps: Option<u16>,
    },
    /// Switch between devnet and mainnet-beta; asks for Touch ID
    Network {
        cluster: String,
        #[arg(long)]
        rpc_url: Option<String>,
    },
    /// Forget the Keychain password; agents cannot use the wallet until setup runs again
    Lock,
}

/// Run a `manus agent …` command for wallet `name`.
pub async fn run(name: &str, action: &AgentAction) -> anyhow::Result<()> {
    match action {
        AgentAction::Setup {
            cluster,
            rpc_url,
            import,
            expected_address,
            from_file,
        } => {
            setup(
                name,
                cluster,
                rpc_url.clone(),
                *import,
                expected_address.as_deref(),
                from_file.as_deref(),
            )
            .await
        }
        AgentAction::Mcp => {
            let wallet = agent_wallet::open(name).await?;
            eprintln!(
                "manus agent wallet {} on {} — serving MCP on stdio",
                wallet.address(),
                wallet.cluster
            );
            agent_wallet::mcp::run_stdio(Arc::new(wallet)).await
        }
        AgentAction::Connect { client } => {
            let client: connect::Client = client.parse()?;
            if !agent_wallet::wallet_path(name).exists() {
                return Err(anyhow::anyhow!(
                    "Run `manus agent setup` before connecting an agent."
                ));
            }
            println!("✅ {}", connect::connect(client, name)?);
            println!("   Restart the agent, then ask it: \"what's in my Manus wallet?\"");
            Ok(())
        }
        AgentAction::Status => print(agent_wallet::open(name).await?.status().await?),
        AgentAction::History { limit } => {
            print(agent_wallet::open(name).await?.history(*limit).await?)
        }
        AgentAction::Budget {
            send_per_op,
            send_daily,
            convert_per_op,
            convert_daily,
            trust,
            max_slippage_bps,
        } => {
            let wallet = agent_wallet::open(name).await?;
            let mut budget = wallet.budget().await?;
            let sol = |value: &Option<String>, current: u64| -> anyhow::Result<u64> {
                value
                    .as_deref()
                    .map(|v| parse_units(v, 9))
                    .unwrap_or(Ok(current))
            };
            budget.send_sol.per_op = sol(send_per_op, budget.send_sol.per_op)?;
            budget.send_sol.daily = sol(send_daily, budget.send_sol.daily)?;
            budget.convert_sol.per_op = sol(convert_per_op, budget.convert_sol.per_op)?;
            budget.convert_sol.daily = sol(convert_daily, budget.convert_sol.daily)?;
            for recipient in trust {
                solana_sdk::pubkey::Pubkey::try_from(recipient.as_str())
                    .map_err(|_| anyhow::anyhow!("'{recipient}' is not a Solana address"))?;
                if !budget.trusted_recipients.contains(recipient) {
                    budget.trusted_recipients.push(recipient.clone());
                }
            }
            if let Some(bps) = max_slippage_bps {
                budget.max_slippage_bps = *bps;
            }
            print(
                wallet
                    .change_budget(budget, "changed from the terminal")
                    .await?,
            )
        }
        AgentAction::Network { cluster, rpc_url } => {
            let network = NetworkConfig {
                cluster: validate_cluster(cluster)?,
                rpc_url: rpc_url
                    .clone()
                    .unwrap_or_else(|| NetworkConfig::default_rpc(cluster)),
            };
            if network.cluster == "mainnet-beta" {
                require_presence("Manus: move the agent wallet to Solana mainnet (real funds)")?;
            }
            check_rpc(&network).await?;
            network.save(name)?;
            println!(
                "✅ Agent wallet now uses {} ({})",
                network.cluster, network.rpc_url
            );
            Ok(())
        }
        AgentAction::Lock => {
            keychain::remove(name)?;
            println!("🔒 Password removed from the Keychain. Agents can no longer use '{name}'.");
            Ok(())
        }
    }
}

async fn setup(
    name: &str,
    cluster: &str,
    rpc_url: Option<String>,
    import: bool,
    expected_address: Option<&str>,
    from_file: Option<&str>,
) -> anyhow::Result<()> {
    crypto::validate_wallet_name(name)?;
    let cluster = validate_cluster(cluster)?;
    agent_wallet::ensure_home()?;
    let network = NetworkConfig {
        rpc_url: rpc_url.unwrap_or_else(|| NetworkConfig::default_rpc(&cluster)),
        cluster,
    };
    if network.cluster == "mainnet-beta" {
        require_presence("Manus: set up an agent wallet on Solana mainnet (real funds)")?;
    }
    check_rpc(&network).await?;

    let wallet_file = agent_wallet::wallet_path(name);
    let password: Zeroizing<String>;
    if wallet_file.exists() {
        println!(
            "🔑 Found existing agent wallet at {}",
            wallet_file.display()
        );
        password = prompt("Wallet password: ")?;
    } else if let Some(source) = from_file {
        let data =
            std::fs::read(source).map_err(|e| anyhow::anyhow!("cannot read {source}: {e}"))?;
        password = prompt(&format!("Password for {source}: "))?;
        drop(Zeroizing::new(crypto::decrypt_mnemonic(&data, &password)?));
        crypto::write_new_wallet_file(&wallet_file.display().to_string(), &data)?;
    } else if import {
        let expected = expected_address.ok_or_else(|| {
            anyhow::anyhow!(
                "--import needs --expected-address so a mistyped phrase cannot slip through"
            )
        })?;
        let phrase = prompt("Recovery phrase (hidden): ")?;
        let (stored, keypair) =
            crate::secret::StoredWalletSecret::find_for_expected_address(&phrase, expected)?;
        drop(phrase);
        password = crypto::prompt_new_password()?;
        let encoded = Zeroizing::new(stored.encode()?);
        let encrypted = crypto::encrypt_mnemonic(&encoded, &password)?;
        crypto::write_new_wallet_file(&wallet_file.display().to_string(), &encrypted)?;
        println!("✅ Imported {}", keypair.pubkey());
    } else {
        use rand::RngCore;
        let mut entropy = [0u8; 16];
        rand::rngs::OsRng.fill_bytes(&mut entropy);
        let mnemonic = bip39::Mnemonic::from_entropy(&entropy)?;
        println!("\n🌱 New agent wallet. Write these 12 words down and keep them offline.");
        println!("   They are the only way to recover the funds.\n");
        println!("   {mnemonic}\n");
        let stored = crate::secret::StoredWalletSecret::legacy(&mnemonic);
        password = crypto::prompt_new_password()?;
        let encoded = Zeroizing::new(stored.encode()?);
        let encrypted = crypto::encrypt_mnemonic(&encoded, &password)?;
        crypto::write_new_wallet_file(&wallet_file.display().to_string(), &encrypted)?;
    }

    let keypair = agent_wallet::unlock(name, &password)
        .map_err(|e| anyhow::anyhow!("could not unlock the wallet: {e}"))?;
    if keychain::available() {
        keychain::store(name, &password)?;
        println!(
            "🔐 Password saved to the macOS Keychain (only the manus binary can read it silently)."
        );
    } else {
        println!("ℹ️  No OS keychain here: provide MANUS_PASSWORD to agents through your secret manager.");
    }
    drop(password);
    network.save(name)?;

    let store = agent_wallet::store::Store::open(&agent_wallet::store_path(name)).await?;
    if store.budget().await?.is_none() {
        store
            .save_budget(&agent_wallet::budget::Budget::default_for_cluster(
                &network.cluster,
            ))
            .await?;
    }

    println!("\n✅ Agent wallet ready");
    println!("   Address:  {}", keypair.pubkey());
    println!("   Network:  {}", network.cluster);
    println!("   Budget:   0.5 SOL per transfer, 2 SOL per day; 25 USDC per transfer, 100 per day");
    println!("             anything above asks you for Touch ID");
    println!("\nNext:");
    println!("   Fund it with a small amount, then connect your agent:");
    println!("   manus agent connect claude     # or codex, gemini");
    Ok(())
}

fn validate_cluster(cluster: &str) -> anyhow::Result<String> {
    match cluster {
        "devnet" | "localnet" | "mainnet-beta" => Ok(cluster.to_string()),
        "mainnet" => Ok("mainnet-beta".to_string()),
        other => Err(anyhow::anyhow!(
            "unknown network '{other}'; use devnet or mainnet-beta"
        )),
    }
}

async fn check_rpc(network: &NetworkConfig) -> anyhow::Result<()> {
    let rpc = solana_client::nonblocking::rpc_client::RpcClient::new(network.rpc_url.clone());
    agent_wallet::verify_network(&rpc, &network.cluster)
        .await
        .map_err(|e| anyhow::anyhow!("RPC check failed for {}: {e}", network.rpc_url))
}

/// Security-relevant terminal actions need the same presence proof as spending.
fn require_presence(reason: &str) -> anyhow::Result<()> {
    match agent_wallet::approval::default_approver().request(reason) {
        ApprovalOutcome::Approved { .. } => Ok(()),
        ApprovalOutcome::Denied { reason } => Err(anyhow::anyhow!("not approved: {reason}")),
    }
}

fn prompt(label: &str) -> anyhow::Result<Zeroizing<String>> {
    Ok(Zeroizing::new(rpassword::prompt_password(label)?))
}

fn print(value: serde_json::Value) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}
