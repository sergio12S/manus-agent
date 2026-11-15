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
        /// Allow swapping into this token (symbol or mint) without approval (repeatable)
        #[arg(long = "allow-token")]
        allow_token: Vec<String>,
        /// Remove a token from allowed_tokens (repeatable)
        #[arg(long = "remove-allowed-token")]
        remove_allowed_token: Vec<String>,
        /// Require output tokens of swaps to be in allowed_tokens (true/false)
        #[arg(long)]
        require_allowed_tokens: Option<bool>,
        /// Minimum seconds between automatic swaps without Touch ID
        #[arg(long)]
        swap_cooldown_secs: Option<u64>,
        /// Daily realized-loss circuit breaker in SOL (0 to disable)
        #[arg(long)]
        max_daily_loss_sol: Option<String>,
    },
    /// Get a live swap quote, price impact, token safety report, budget decision, and optional Kelly sizing
    Quote {
        from: String,
        to: String,
        amount: String,
        #[arg(long)]
        slippage_bps: Option<u16>,
        /// Optional Kelly fraction (0.0..1.0) to size SOL trade against wallet balance and budget
        #[arg(long)]
        kelly_fraction: Option<f64>,
    },
    /// Show trading portfolio, token holdings, cost basis, realized/unrealized PnL, and strategy win rate
    Portfolio,
    /// Manage and watch autonomous Take-Profit / Stop-Loss / OCO / Trailing-Stop exit triggers
    Trigger {
        #[command(subcommand)]
        action: TriggerAction,
    },
    /// Switch between devnet and mainnet-beta; asks for Touch ID
    Network {
        cluster: String,
        #[arg(long)]
        rpc_url: Option<String>,
    },
    /// Forget the Keychain password; agents cannot use the wallet until setup runs again
    Lock,
    /// Bill another agent wallet, or pay a bill you were given
    Invoice {
        #[command(subcommand)]
        action: InvoiceAction,
    },
    /// Inspect and pay HTTP 402 (x402 v2) payment challenges
    X402 {
        #[command(subcommand)]
        action: X402Action,
    },
    /// Verify a signed invoice, manus.receipt.v1, operation receipt, or op_/inv_ ID
    Verify {
        /// JSON string, @path/to/file.json, file path, op_ ID, or inv_ ID
        target: String,
        /// Only check cryptographic signatures and schema without calling Solana RPC
        #[arg(long)]
        offline: bool,
        /// Override cluster (devnet, localnet, mainnet-beta)
        #[arg(long)]
        cluster: Option<String>,
        /// Override Solana RPC URL
        #[arg(long)]
        rpc_url: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum TriggerAction {
    /// Create a Take-Profit, Stop-Loss, or OCO exit trigger
    Create {
        from: String,
        to: String,
        amount: String,
        #[arg(long)]
        slippage_bps: Option<u16>,
        #[arg(long)]
        take_profit_rate: Option<f64>,
        #[arg(long)]
        stop_loss_rate: Option<f64>,
        #[arg(long)]
        take_profit_pct: Option<f64>,
        #[arg(long)]
        stop_loss_pct: Option<f64>,
        #[arg(long)]
        trailing_stop_bps: Option<u16>,
        #[arg(long)]
        tag: Option<String>,
        #[arg(long)]
        dry_run: bool,
    },
    /// List active (or --all) triggers
    List {
        #[arg(long)]
        all: bool,
    },
    /// Cancel an active trigger by ID
    Cancel { id: String },
    /// Continuously watch and execute active triggers in the foreground
    Watch {
        #[arg(long, default_value_t = 15)]
        interval_secs: u64,
    },
}

#[derive(Subcommand)]
pub enum X402Action {
    /// Pay an x402 v2 challenge (JSON, base64 PAYMENT-REQUIRED header, or @path)
    Pay {
        challenge: String,
        #[arg(long)]
        request_id: Option<String>,
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
pub enum InvoiceAction {
    /// Sign a bill from this wallet
    Create {
        amount: String,
        description: String,
        /// SOL (default), USDC, USDT, JitoSOL, mSOL, or a mint address
        #[arg(long, default_value = "SOL")]
        token: String,
        /// Hours until the bill expires (1-720, default 168)
        #[arg(long, default_value_t = 168)]
        hours: u64,
    },
    /// Pay a signed invoice. Pass the JSON, or @path to a file
    Pay {
        invoice: String,
        #[arg(long)]
        dry_run: bool,
    },
    /// See whether an invoice has been paid
    Status { invoice_id: String },
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
            allow_token,
            remove_allowed_token,
            require_allowed_tokens,
            swap_cooldown_secs,
            max_daily_loss_sol,
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
            for token in allow_token {
                let trimmed = token.trim();
                if !trimmed.is_empty() && !budget.allowed_tokens.iter().any(|t| t == trimmed) {
                    budget.allowed_tokens.push(trimmed.to_string());
                }
            }
            if !remove_allowed_token.is_empty() {
                budget
                    .allowed_tokens
                    .retain(|t| !remove_allowed_token.iter().any(|r| r == t));
            }
            if let Some(flag) = require_allowed_tokens {
                budget.require_allowed_tokens = *flag;
            }
            if let Some(secs) = swap_cooldown_secs {
                budget.swap_cooldown_secs = *secs;
            }
            if let Some(loss_sol) = max_daily_loss_sol {
                if loss_sol.trim() == "0" {
                    budget.max_daily_loss_sol = 0;
                } else {
                    budget.max_daily_loss_sol = parse_units(loss_sol, 9)?;
                }
            }
            print(
                wallet
                    .change_budget(budget, "changed from the terminal")
                    .await?,
            )
        }
        AgentAction::Quote {
            from,
            to,
            amount,
            slippage_bps,
            kelly_fraction,
        } => {
            let wallet = agent_wallet::open(name).await?;
            print(
                wallet
                    .quote(from, to, amount, *slippage_bps, *kelly_fraction)
                    .await?,
            )
        }
        AgentAction::Portfolio => {
            let wallet = agent_wallet::open(name).await?;
            print(wallet.portfolio().await?)
        }
        AgentAction::Trigger { action } => {
            let wallet = agent_wallet::open(name).await?;
            match action {
                TriggerAction::Create {
                    from,
                    to,
                    amount,
                    slippage_bps,
                    take_profit_rate,
                    stop_loss_rate,
                    take_profit_pct,
                    stop_loss_pct,
                    trailing_stop_bps,
                    tag,
                    dry_run,
                } => print(
                    wallet
                        .create_trigger(
                            from,
                            to,
                            amount,
                            *slippage_bps,
                            *take_profit_rate,
                            *stop_loss_rate,
                            *take_profit_pct,
                            *stop_loss_pct,
                            *trailing_stop_bps,
                            tag.clone(),
                            *dry_run,
                        )
                        .await?,
                ),
                TriggerAction::List { all } => print(wallet.list_triggers(!all).await?),
                TriggerAction::Cancel { id } => print(wallet.cancel_trigger(id).await?),
                TriggerAction::Watch { interval_secs } => {
                    eprintln!(
                        "manus trigger watcher started for {} on {} (every {}s)",
                        wallet.address(),
                        wallet.cluster,
                        interval_secs
                    );
                    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(
                        (*interval_secs).max(5),
                    ));
                    loop {
                        ticker.tick().await;
                        let res = wallet.evaluate_triggers().await?;
                        if res["triggered"]
                            .as_array()
                            .is_some_and(|arr| !arr.is_empty())
                        {
                            print(res)?;
                        }
                    }
                }
            }
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
        AgentAction::Invoice { action } => invoice_command(name, action).await,
        AgentAction::X402 { action } => x402_command(name, action).await,
        AgentAction::Verify {
            target,
            offline,
            cluster,
            rpc_url,
        } => {
            run_verify(
                name,
                target,
                *offline,
                cluster.as_deref(),
                rpc_url.as_deref(),
            )
            .await
        }
    }
}

async fn x402_command(name: &str, action: &X402Action) -> anyhow::Result<()> {
    let wallet = agent_wallet::open(name).await?;
    match action {
        X402Action::Pay {
            challenge,
            request_id,
            dry_run,
        } => {
            let trimmed_arg = challenge.trim();
            if trimmed_arg.starts_with("https://") || trimmed_arg.starts_with("http://") {
                let http = crate::x402::HttpCall {
                    url: trimmed_arg.to_string(),
                    method: None,
                    headers: None,
                    body: None,
                };
                let result = wallet
                    .pay_x402(None, None, Some(http), request_id.clone(), *dry_run)
                    .await?;
                return print(result);
            }
            let text = read_input_arg(challenge)?;
            let trimmed = text.trim();
            let result = if trimmed.starts_with('{') {
                let json: serde_json::Value = serde_json::from_str(trimmed)?;
                wallet
                    .pay_x402(Some(&json), None, None, request_id.clone(), *dry_run)
                    .await?
            } else {
                wallet
                    .pay_x402(None, Some(trimmed), None, request_id.clone(), *dry_run)
                    .await?
            };
            print(result)
        }
    }
}

/// Verify an invoice, `manus.receipt.v1`, or operation receipt without needing the wallet keypair.
pub async fn run_verify(
    name: &str,
    target: &str,
    offline: bool,
    cluster_override: Option<&str>,
    rpc_override: Option<&str>,
) -> anyhow::Result<()> {
    let trimmed = target.trim();
    let document: serde_json::Value = if trimmed.starts_with("op_") {
        let store = crate::store::Store::open(&agent_wallet::store_path(name)).await?;
        let record = store
            .get(trimmed)
            .await?
            .ok_or_else(|| anyhow::anyhow!("unknown operation {trimmed}"))?;
        serde_json::to_value(record)?
    } else if trimmed.starts_with("inv_") {
        let store = crate::store::Store::open(&agent_wallet::store_path(name)).await?;
        let row = store
            .invoice(trimmed)
            .await?
            .ok_or_else(|| anyhow::anyhow!("unknown invoice {trimmed}"))?;
        serde_json::from_str(&row.document_json)?
    } else {
        let text = read_input_arg(trimmed)?;
        serde_json::from_str(&text)
            .map_err(|error| anyhow::anyhow!("cannot parse JSON to verify: {error}"))?
    };

    if offline {
        let report = crate::verify::verify_document(&document, None).await?;
        return print(report);
    }

    let net = NetworkConfig::load(name).unwrap_or(NetworkConfig {
        cluster: "devnet".into(),
        rpc_url: NetworkConfig::default_rpc("devnet"),
    });
    let inferred_cluster = document
        .get("cluster")
        .or_else(|| document.pointer("/invoice/cluster"))
        .or_else(|| document.pointer("/receipt/cluster"))
        .and_then(serde_json::Value::as_str);
    let cluster = match cluster_override.or(inferred_cluster) {
        Some(c) => validate_cluster(c)?,
        None => net.cluster.clone(),
    };
    let rpc_url = match rpc_override {
        Some(url) => url.to_string(),
        None if cluster == net.cluster => net.rpc_url,
        None => NetworkConfig::default_rpc(&cluster),
    };
    let rpc = solana_client::nonblocking::rpc_client::RpcClient::new_with_commitment(
        rpc_url,
        solana_sdk::commitment_config::CommitmentConfig::confirmed(),
    );
    let report = crate::verify::verify_document(&document, Some((&rpc, &cluster))).await?;
    print(report)
}

fn read_input_arg(arg: &str) -> anyhow::Result<String> {
    if let Some(path) = arg.strip_prefix('@') {
        return std::fs::read_to_string(path)
            .map_err(|error| anyhow::anyhow!("cannot read {path}: {error}"));
    }
    if !arg.trim_start().starts_with('{') && std::path::Path::new(arg).is_file() {
        return std::fs::read_to_string(arg)
            .map_err(|error| anyhow::anyhow!("cannot read {arg}: {error}"));
    }
    Ok(arg.to_string())
}

async fn invoice_command(name: &str, action: &InvoiceAction) -> anyhow::Result<()> {
    let wallet = agent_wallet::open(name).await?;
    match action {
        InvoiceAction::Create {
            amount,
            description,
            token,
            hours,
        } => print(
            wallet
                .create_invoice(token, amount, description, *hours)
                .await?,
        ),
        InvoiceAction::Pay { invoice, dry_run } => {
            let text = if let Some(path) = invoice.strip_prefix('@') {
                std::fs::read_to_string(path)
                    .map_err(|error| anyhow::anyhow!("cannot read {path}: {error}"))?
            } else {
                invoice.clone()
            };
            let document = serde_json::from_str(&text)?;
            print(wallet.pay_invoice(&document, *dry_run).await?)
        }
        InvoiceAction::Status { invoice_id } => print(wallet.invoice_status(invoice_id).await?),
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
