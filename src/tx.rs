//! Transaction construction, structural inspection and simulation-derived effects.
//!
//! Policy never trusts the agent's description of an operation. Every transaction
//! is signed, structurally inspected, simulated byte-for-byte, and judged by the
//! balance changes that simulation reports for the wallet's own accounts.

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde::{Deserialize, Serialize};
use solana_account_decoder::UiAccountEncoding;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_config::{
    RpcSimulateTransactionAccountsConfig, RpcSimulateTransactionConfig,
};
use solana_sdk::{
    commitment_config::CommitmentConfig,
    instruction::Instruction,
    message::{Message, VersionedMessage},
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    transaction::VersionedTransaction,
};
use spl_associated_token_account::{
    get_associated_token_address_with_program_id,
    instruction::create_associated_token_account_idempotent,
};
use std::str::FromStr;

use super::budget::DEVNET_USDC_MINT;
use crate::mints::{JITO_SOL_MINT, M_SOL_MINT, SOL_MINT, USDC_MINT};

pub const JUPITER_V6_PROGRAM: &str = "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4";
const COMPUTE_BUDGET_PROGRAM: &str = "ComputeBudget111111111111111111111111111111";
const MEMO_PROGRAM: &str = "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr";
const USDT_MINT: &str = "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB";
const MAX_PRIORITY_FEE_LAMPORTS: u64 = 200_000;

/// Manus fee on swaps and liquid staking, collected atomically by Jupiter in the
/// output token of an ExactIn swap. Transfers carry no fee.
pub const PLATFORM_FEE_BPS: u16 = 10;

/// Token account that receives the Manus fee in `asset`, owned by the fee wallet.
pub fn fee_account_for(asset: &Asset) -> Pubkey {
    let owner = Pubkey::from_str(crate::FEE_WALLET).expect("fee wallet address");
    let (mint, program) = match (asset.mint, asset.token_program) {
        (Some(mint), Some(program)) => (mint, program),
        _ => (
            Pubkey::from_str(SOL_MINT).expect("wsol mint"),
            spl_token::id(),
        ),
    };
    get_associated_token_address_with_program_id(&owner, &mint, &program)
}

/// A token the wallet can reason about.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Asset {
    pub symbol: String,
    /// None for native SOL.
    pub mint: Option<Pubkey>,
    pub decimals: u8,
    pub token_program: Option<Pubkey>,
}

impl Asset {
    pub fn sol() -> Self {
        Self {
            symbol: "SOL".into(),
            mint: None,
            decimals: 9,
            token_program: None,
        }
    }

    pub fn mint_or_wsol(&self) -> String {
        self.mint
            .map(|mint| mint.to_string())
            .unwrap_or_else(|| SOL_MINT.to_string())
    }
}

fn known_mint(symbol: &str, cluster: &str) -> Option<&'static str> {
    let mainnet = cluster == "mainnet-beta";
    match symbol.to_ascii_uppercase().as_str() {
        "USDC" if mainnet => Some(USDC_MINT),
        "USDC" => Some(DEVNET_USDC_MINT),
        "USDT" if mainnet => Some(USDT_MINT),
        "JITOSOL" if mainnet => Some(JITO_SOL_MINT),
        "MSOL" if mainnet => Some(M_SOL_MINT),
        _ => None,
    }
}

pub fn symbol_for_mint(mint: &str) -> Option<&'static str> {
    match mint {
        USDC_MINT | DEVNET_USDC_MINT => Some("USDC"),
        USDT_MINT => Some("USDT"),
        JITO_SOL_MINT => Some("JitoSOL"),
        M_SOL_MINT => Some("mSOL"),
        SOL_MINT => Some("SOL"),
        _ => None,
    }
}

/// Resolve "SOL", a known symbol, or a base58 mint into an asset with on-chain decimals.
pub async fn resolve_asset(rpc: &RpcClient, token: &str, cluster: &str) -> anyhow::Result<Asset> {
    let token = token.trim();
    if token.eq_ignore_ascii_case("SOL") || token == SOL_MINT {
        return Ok(Asset::sol());
    }
    let mint_text = known_mint(token, cluster).unwrap_or(token);
    let mint = Pubkey::from_str(mint_text).map_err(|_| {
        anyhow::anyhow!(
            "unknown token '{token}'. Use SOL, USDC, USDT, JitoSOL, mSOL or a mint address"
        )
    })?;
    let account = rpc
        .get_account_with_commitment(&mint, CommitmentConfig::confirmed())
        .await?
        .value
        .ok_or_else(|| anyhow::anyhow!("mint {mint} does not exist on {cluster}"))?;
    if account.owner != spl_token::id() && account.owner != spl_token_2022::id() {
        return Err(anyhow::anyhow!("{mint} is not an SPL token mint"));
    }
    if account.data.len() < 45 {
        return Err(anyhow::anyhow!("mint {mint} has malformed data"));
    }
    Ok(Asset {
        symbol: symbol_for_mint(mint_text)
            .map(str::to_string)
            .unwrap_or_else(|| short(&mint)),
        mint: Some(mint),
        decimals: account.data[44],
        token_program: Some(account.owner),
    })
}

pub fn short(key: &Pubkey) -> String {
    let text = key.to_string();
    format!("{}…{}", &text[..4], &text[text.len() - 4..])
}

/// Build a native SOL transfer, optionally with a memo.
pub fn sol_transfer_instructions(
    from: &Pubkey,
    to: &Pubkey,
    lamports: u64,
    memo: Option<&str>,
) -> Vec<Instruction> {
    let mut instructions = vec![solana_system_interface::instruction::transfer(
        from, to, lamports,
    )];
    if let Some(memo) = memo {
        instructions.push(memo_instruction(memo));
    }
    instructions
}

/// Build an SPL `TransferChecked`, creating the recipient's associated account if needed.
pub fn token_transfer_instructions(
    owner: &Pubkey,
    to: &Pubkey,
    asset: &Asset,
    amount: u64,
    memo: Option<&str>,
) -> anyhow::Result<Vec<Instruction>> {
    let mint = asset.mint.ok_or_else(|| anyhow::anyhow!("not a token"))?;
    let program = asset
        .token_program
        .ok_or_else(|| anyhow::anyhow!("token program unknown"))?;
    let source = get_associated_token_address_with_program_id(owner, &mint, &program);
    let destination = get_associated_token_address_with_program_id(to, &mint, &program);
    let mut instructions = vec![create_associated_token_account_idempotent(
        owner, to, &mint, &program,
    )];
    let transfer = if program == spl_token_2022::id() {
        spl_token_2022::instruction::transfer_checked(
            &program,
            &source,
            &mint,
            &destination,
            owner,
            &[],
            amount,
            asset.decimals,
        )?
    } else {
        spl_token::instruction::transfer_checked(
            &program,
            &source,
            &mint,
            &destination,
            owner,
            &[],
            amount,
            asset.decimals,
        )?
    };
    instructions.push(transfer);
    if let Some(memo) = memo {
        instructions.push(memo_instruction(memo));
    }
    Ok(instructions)
}

fn memo_instruction(memo: &str) -> Instruction {
    Instruction {
        program_id: Pubkey::from_str(MEMO_PROGRAM).expect("memo program id"),
        accounts: vec![],
        data: memo.as_bytes().to_vec(),
    }
}

pub async fn sign_instructions(
    rpc: &RpcClient,
    signer: &Keypair,
    instructions: &[Instruction],
) -> anyhow::Result<VersionedTransaction> {
    let blockhash = rpc.get_latest_blockhash().await?;
    let message = Message::new_with_blockhash(instructions, Some(&signer.pubkey()), &blockhash);
    Ok(VersionedTransaction::try_new(
        VersionedMessage::Legacy(message),
        &[signer],
    )?)
}

/// Programs an agent transaction may invoke at the top level.
fn allowed_program(program: &Pubkey) -> bool {
    let text = program.to_string();
    *program == solana_system_interface::program::id()
        || *program == spl_token::id()
        || *program == spl_token_2022::id()
        || *program == spl_associated_token_account::id()
        || matches!(
            text.as_str(),
            COMPUTE_BUDGET_PROGRAM | MEMO_PROGRAM | JUPITER_V6_PROGRAM
        )
}

/// Reject transactions whose structure could hand control of the wallet to someone else.
pub fn inspect_structure(tx: &VersionedTransaction, wallet: &Pubkey) -> anyhow::Result<()> {
    let message = &tx.message;
    let keys = message.static_account_keys();
    if keys.first() != Some(wallet) {
        return Err(anyhow::anyhow!("fee payer is not the agent wallet"));
    }
    if message.header().num_required_signatures != 1 {
        return Err(anyhow::anyhow!(
            "transaction requires {} signers; agent transactions must have exactly one",
            message.header().num_required_signatures
        ));
    }
    for instruction in message.instructions() {
        let program = keys
            .get(instruction.program_id_index as usize)
            .ok_or_else(|| anyhow::anyhow!("instruction program is not a static account"))?;
        if !allowed_program(program) {
            return Err(anyhow::anyhow!("program {program} is not allowed"));
        }
        let is_token = *program == spl_token::id() || *program == spl_token_2022::id();
        match (is_token, instruction.data.first()) {
            // Approve, SetAuthority, ApproveChecked delegate or transfer control.
            (true, Some(4 | 6 | 13)) => {
                return Err(anyhow::anyhow!(
                    "token delegation or authority change is not allowed"
                ))
            }
            // CloseAccount must return rent to the wallet itself.
            (true, Some(9)) => {
                let destination = instruction
                    .accounts
                    .get(1)
                    .and_then(|index| keys.get(*index as usize));
                if destination != Some(wallet) {
                    return Err(anyhow::anyhow!(
                        "closing a token account must refund the agent wallet"
                    ));
                }
            }
            _ => {}
        }
        if *program == solana_system_interface::program::id() {
            // Assign (1) and AssignWithSeed (10) would transfer ownership of an account.
            if let Some(tag) = instruction.data.get(..4) {
                let tag = u32::from_le_bytes([tag[0], tag[1], tag[2], tag[3]]);
                if matches!(tag, 1 | 10) {
                    return Err(anyhow::anyhow!("account reassignment is not allowed"));
                }
            }
        }
    }
    Ok(())
}

/// Net balance changes for the wallet's accounts that simulation observed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SimulatedEffects {
    pub sol_before: u64,
    pub sol_after: u64,
    /// (mint, before, after) for each watched token account.
    pub tokens: Vec<(String, u64, u64)>,
    pub units_consumed: Option<u64>,
}

impl SimulatedEffects {
    pub fn sol_out(&self) -> u64 {
        self.sol_before.saturating_sub(self.sol_after)
    }

    pub fn token_delta(&self, mint: &str) -> (u64, u64) {
        self.tokens
            .iter()
            .find(|(watched, _, _)| watched == mint)
            .map(|(_, before, after)| {
                (before.saturating_sub(*after), after.saturating_sub(*before))
            })
            .unwrap_or((0, 0))
    }
}

/// Simulate the exact signed bytes and read back the wallet's balances.
pub async fn simulate(
    rpc: &RpcClient,
    tx: &VersionedTransaction,
    wallet: &Pubkey,
    watched_assets: &[&Asset],
) -> anyhow::Result<SimulatedEffects> {
    let mut watched: Vec<(String, Pubkey)> = Vec::new();
    for asset in watched_assets {
        if let (Some(mint), Some(program)) = (asset.mint, asset.token_program) {
            let account = get_associated_token_address_with_program_id(wallet, &mint, &program);
            if !watched.iter().any(|(_, existing)| *existing == account) {
                watched.push((mint.to_string(), account));
            }
        }
    }
    let mut addresses = vec![*wallet];
    addresses.extend(watched.iter().map(|(_, account)| *account));

    let before = rpc
        .get_multiple_accounts_with_commitment(&addresses, CommitmentConfig::confirmed())
        .await?
        .value;
    let config = RpcSimulateTransactionConfig {
        sig_verify: true,
        replace_recent_blockhash: false,
        commitment: Some(CommitmentConfig::confirmed()),
        accounts: Some(RpcSimulateTransactionAccountsConfig {
            encoding: Some(UiAccountEncoding::Base64),
            addresses: addresses.iter().map(Pubkey::to_string).collect(),
        }),
        ..Default::default()
    };
    let result = rpc
        .simulate_transaction_with_config(tx, config)
        .await?
        .value;
    if let Some(error) = result.err {
        let logs = result.logs.unwrap_or_default();
        let tail: Vec<String> = logs
            .iter()
            .rev()
            .take(4)
            .rev()
            .map(|line| line.chars().take(160).collect())
            .collect();
        return Err(anyhow::anyhow!(
            "simulation failed: {error:?}; last logs: {}",
            tail.join(" | ")
        ));
    }
    let after = result
        .accounts
        .ok_or_else(|| anyhow::anyhow!("simulation returned no account state"))?;
    if after.len() != addresses.len() {
        return Err(anyhow::anyhow!(
            "simulation returned an unexpected account count"
        ));
    }
    let after_lamports =
        |index: usize| -> u64 { after[index].as_ref().map(|a| a.lamports).unwrap_or(0) };
    let after_token = |index: usize| -> anyhow::Result<u64> {
        match &after[index] {
            None => Ok(0),
            Some(account) => {
                let decoded: solana_sdk::account::Account = account
                    .decode()
                    .ok_or_else(|| anyhow::anyhow!("undecodable simulated token account"))?;
                token_amount(&decoded.data, wallet)
            }
        }
    };

    let mut tokens = Vec::new();
    for (offset, (mint, _)) in watched.iter().enumerate() {
        let index = offset + 1;
        let before_amount = match &before[index] {
            Some(account) => token_amount(&account.data, wallet)?,
            None => 0,
        };
        tokens.push((mint.clone(), before_amount, after_token(index)?));
    }
    Ok(SimulatedEffects {
        sol_before: before[0].as_ref().map(|a| a.lamports).unwrap_or(0),
        sol_after: after_lamports(0),
        tokens,
        units_consumed: result.units_consumed,
    })
}

/// Read the amount of an SPL token account, checking that the wallet owns it.
fn token_amount(data: &[u8], wallet: &Pubkey) -> anyhow::Result<u64> {
    if data.len() < 72 {
        return Err(anyhow::anyhow!("token account data is too short"));
    }
    if &data[32..64] != wallet.as_ref() {
        return Err(anyhow::anyhow!(
            "watched token account is not owned by the wallet"
        ));
    }
    let mut amount = [0u8; 8];
    amount.copy_from_slice(&data[64..72]);
    Ok(u64::from_le_bytes(amount))
}

// ---------------------------------------------------------------- Jupiter

pub struct Jupiter {
    http: reqwest::Client,
    base: String,
    api_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Quote {
    pub raw: serde_json::Value,
    pub in_amount: u64,
    pub out_amount: u64,
    pub min_out_amount: u64,
    pub price_impact_pct: f64,
    pub route: String,
    /// Manus fee in output-token base units, already deducted from `out_amount`.
    pub platform_fee: u64,
    pub fee_bps: u16,
}

impl Jupiter {
    pub fn from_env() -> Self {
        let api_key = std::env::var("JUPITER_API_KEY")
            .ok()
            .filter(|k| !k.is_empty());
        let base = std::env::var("MANUS_JUPITER_URL").unwrap_or_else(|_| {
            if api_key.is_some() {
                "https://api.jup.ag/swap/v1".into()
            } else {
                "https://lite-api.jup.ag/swap/v1".into()
            }
        });
        Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(20))
                .build()
                .expect("http client"),
            base,
            api_key,
        }
    }

    fn with_key(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.api_key {
            Some(key) => request.header("x-api-key", key),
            None => request,
        }
    }

    pub async fn quote(
        &self,
        input_mint: &str,
        output_mint: &str,
        amount: u64,
        slippage_bps: u16,
        fee_bps: u16,
    ) -> anyhow::Result<Quote> {
        let mut query = vec![
            ("inputMint", input_mint.to_string()),
            ("outputMint", output_mint.to_string()),
            ("amount", amount.to_string()),
            ("slippageBps", slippage_bps.to_string()),
            ("restrictIntermediateTokens", "true".to_string()),
        ];
        if fee_bps > 0 {
            query.push(("platformFeeBps", fee_bps.to_string()));
        }
        let request = self.http.get(format!("{}/quote", self.base)).query(&query);
        let response = self.with_key(request).send().await?;
        let status = response.status();
        let raw: serde_json::Value = response.json().await?;
        if !status.is_success() {
            return Err(anyhow::anyhow!("Jupiter quote failed ({status}): {raw}"));
        }
        let number = |field: &str| -> anyhow::Result<u64> {
            raw[field]
                .as_str()
                .and_then(|text| text.parse().ok())
                .ok_or_else(|| anyhow::anyhow!("Jupiter quote is missing {field}"))
        };
        if raw["inputMint"].as_str() != Some(input_mint)
            || raw["outputMint"].as_str() != Some(output_mint)
            || number("inAmount")? != amount
        {
            return Err(anyhow::anyhow!("Jupiter quote does not match the request"));
        }
        let route = raw["routePlan"]
            .as_array()
            .map(|steps| {
                steps
                    .iter()
                    .filter_map(|step| step["swapInfo"]["label"].as_str())
                    .collect::<Vec<_>>()
                    .join(" → ")
            })
            .unwrap_or_default();
        let platform_fee = platform_fee_amount(&raw, fee_bps)?;
        Ok(Quote {
            platform_fee,
            fee_bps,
            in_amount: number("inAmount")?,
            out_amount: number("outAmount")?,
            min_out_amount: number("otherAmountThreshold")?,
            price_impact_pct: raw["priceImpactPct"]
                .as_str()
                .and_then(|text| text.parse().ok())
                .unwrap_or(0.0),
            route,
            raw,
        })
    }

    /// Ask Jupiter for a swap transaction for `quote` and sign it with the wallet.
    pub async fn signed_swap(
        &self,
        quote: &Quote,
        signer: &Keypair,
        fee_account: Option<Pubkey>,
    ) -> anyhow::Result<VersionedTransaction> {
        let mut body = serde_json::json!({
            "quoteResponse": quote.raw,
            "userPublicKey": signer.pubkey().to_string(),
            "wrapAndUnwrapSol": true,
            "dynamicComputeUnitLimit": true,
            "prioritizationFeeLamports": {
                "priorityLevelWithMaxLamports": {
                    "maxLamports": MAX_PRIORITY_FEE_LAMPORTS,
                    "priorityLevel": "high"
                }
            }
        });
        if let (Some(account), true) = (fee_account, quote.fee_bps > 0) {
            body["feeAccount"] = serde_json::json!(account.to_string());
        }
        let request = self.http.post(format!("{}/swap", self.base)).json(&body);
        let response = self.with_key(request).send().await?;
        let status = response.status();
        let raw: serde_json::Value = response.json().await?;
        if !status.is_success() {
            return Err(anyhow::anyhow!(
                "Jupiter swap build failed ({status}): {raw}"
            ));
        }
        let encoded = raw["swapTransaction"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Jupiter returned no transaction"))?;
        let unsigned: VersionedTransaction = bincode::deserialize(&B64.decode(encoded)?)?;
        Ok(VersionedTransaction::try_new(unsigned.message, &[signer])?)
    }
}

/// The fee a quote charges, which may not exceed what we asked for.
fn platform_fee_amount(raw: &serde_json::Value, fee_bps: u16) -> anyhow::Result<u64> {
    if fee_bps == 0 {
        return Ok(0);
    }
    let reported_bps = raw["platformFee"]["feeBps"].as_u64().unwrap_or(0);
    if reported_bps > fee_bps as u64 {
        return Err(anyhow::anyhow!(
            "Jupiter quote charges {reported_bps} bps, more than the Manus fee of {fee_bps} bps"
        ));
    }
    Ok(raw["platformFee"]["amount"]
        .as_str()
        .and_then(|text| text.parse().ok())
        .unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fee_accounts_belong_to_the_fee_wallet() {
        let owner = Pubkey::from_str(crate::FEE_WALLET).unwrap();
        let wsol = Pubkey::from_str(SOL_MINT).unwrap();
        assert_eq!(
            fee_account_for(&Asset::sol()),
            get_associated_token_address_with_program_id(&owner, &wsol, &spl_token::id())
        );
        let mint = Pubkey::new_unique();
        let token = Asset {
            symbol: "X".into(),
            mint: Some(mint),
            decimals: 6,
            token_program: Some(spl_token_2022::id()),
        };
        assert_eq!(
            fee_account_for(&token),
            get_associated_token_address_with_program_id(&owner, &mint, &spl_token_2022::id())
        );
    }

    #[test]
    fn platform_fee_is_read_and_capped() {
        let raw = serde_json::json!({"platformFee": {"amount": "1000", "feeBps": 10}});
        assert_eq!(platform_fee_amount(&raw, 10).unwrap(), 1000);
        assert_eq!(platform_fee_amount(&raw, 0).unwrap(), 0);
        let greedy = serde_json::json!({"platformFee": {"amount": "5000", "feeBps": 50}});
        assert!(platform_fee_amount(&greedy, 10).is_err());
    }

    fn signed(instructions: &[Instruction], payer: &Keypair) -> VersionedTransaction {
        let message = Message::new_with_blockhash(
            instructions,
            Some(&payer.pubkey()),
            &solana_sdk::hash::Hash::default(),
        );
        VersionedTransaction::try_new(VersionedMessage::Legacy(message), &[payer]).unwrap()
    }

    #[test]
    fn plain_transfers_pass_inspection() {
        let wallet = Keypair::new();
        let to = Pubkey::new_unique();
        let tx = signed(
            &sol_transfer_instructions(&wallet.pubkey(), &to, 10, Some("thanks")),
            &wallet,
        );
        inspect_structure(&tx, &wallet.pubkey()).unwrap();

        let asset = Asset {
            symbol: "USDC".into(),
            mint: Some(Pubkey::new_unique()),
            decimals: 6,
            token_program: Some(spl_token::id()),
        };
        let tx = signed(
            &token_transfer_instructions(&wallet.pubkey(), &to, &asset, 5, None).unwrap(),
            &wallet,
        );
        inspect_structure(&tx, &wallet.pubkey()).unwrap();
    }

    #[test]
    fn delegation_reassignment_and_unknown_programs_are_rejected() {
        let wallet = Keypair::new();
        let token_account = Pubkey::new_unique();
        let approve = spl_token::instruction::approve(
            &spl_token::id(),
            &token_account,
            &Pubkey::new_unique(),
            &wallet.pubkey(),
            &[],
            1,
        )
        .unwrap();
        let error = inspect_structure(&signed(&[approve], &wallet), &wallet.pubkey()).unwrap_err();
        assert!(error.to_string().contains("delegation"));

        let assign =
            solana_system_interface::instruction::assign(&wallet.pubkey(), &Pubkey::new_unique());
        let error = inspect_structure(&signed(&[assign], &wallet), &wallet.pubkey()).unwrap_err();
        assert!(error.to_string().contains("reassignment"));

        let close_elsewhere = spl_token::instruction::close_account(
            &spl_token::id(),
            &token_account,
            &Pubkey::new_unique(),
            &wallet.pubkey(),
            &[],
        )
        .unwrap();
        assert!(inspect_structure(&signed(&[close_elsewhere], &wallet), &wallet.pubkey()).is_err());

        let unknown = Instruction {
            program_id: Pubkey::new_unique(),
            accounts: vec![],
            data: vec![],
        };
        assert!(inspect_structure(&signed(&[unknown], &wallet), &wallet.pubkey()).is_err());
    }

    #[test]
    fn foreign_fee_payer_is_rejected() {
        let wallet = Keypair::new();
        let other = Keypair::new();
        let tx = signed(
            &sol_transfer_instructions(&other.pubkey(), &Pubkey::new_unique(), 1, None),
            &other,
        );
        assert!(inspect_structure(&tx, &wallet.pubkey()).is_err());
    }

    #[test]
    fn token_amount_requires_wallet_ownership() {
        let wallet = Pubkey::new_unique();
        let mut data = vec![0u8; 165];
        data[32..64].copy_from_slice(wallet.as_ref());
        data[64..72].copy_from_slice(&42u64.to_le_bytes());
        assert_eq!(token_amount(&data, &wallet).unwrap(), 42);
        assert!(token_amount(&data, &Pubkey::new_unique()).is_err());
    }
}
