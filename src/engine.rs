//! The agent wallet lifecycle:
//! request → build and sign → inspect → simulate → budget decision →
//! (human approval) → durable pre-send record → submit → confirmation → receipt.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_config::RpcSendTransactionConfig;
use solana_client::rpc_request::TokenAccountsFilter;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    hash::Hash,
    pubkey::Pubkey,
    signature::{Keypair, Signature, Signer},
    transaction::VersionedTransaction,
};
use solana_transaction_status::TransactionConfirmationStatus;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use super::approval::{ApprovalOutcome, Approver};
use super::budget::{decide, format_units, parse_units, Budget, Category, Decision, Exposure};
use super::gbrain::Gbrain;
use super::store::{OperationRecord, Store};
use super::tx::{self, Asset, Jupiter, Quote, SimulatedEffects};
use crate::mints::{JITO_SOL_MINT, SOL_MINT};

/// Network fee headroom for a plain transfer.
const SEND_FEE_ALLOWANCE: u64 = 100_000;
/// Fee plus rent for creating a recipient token account.
const TOKEN_SEND_SOL_ALLOWANCE: u64 = 3_000_000;
/// Fees, priority fee and account rent a swap may consume on top of its input.
const SWAP_SOL_ALLOWANCE: u64 = 5_000_000;
const MAX_PRICE_IMPACT_PCT: f64 = 2.0;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Send {
        to: String,
        amount: String,
        #[serde(default = "sol")]
        token: String,
        #[serde(default)]
        memo: Option<String>,
    },
    Swap {
        from: String,
        to: String,
        amount: String,
        #[serde(default)]
        slippage_bps: Option<u16>,
    },
    /// Liquid staking: SOL → JitoSOL.
    Stake { amount: String },
    /// Instant unstake through the market: JitoSOL → SOL. `amount` may be "all".
    Unstake { amount: String },
}

fn sol() -> String {
    "SOL".into()
}

impl Request {
    fn kind(&self) -> &'static str {
        match self {
            Request::Send { .. } => "send",
            Request::Swap { .. } => "swap",
            Request::Stake { .. } => "stake",
            Request::Unstake { .. } => "unstake",
        }
    }
}

/// A signed transaction together with what it is expected to do.
struct Prepared {
    tx: VersionedTransaction,
    blockhash: Hash,
    summary: String,
    watched: Vec<Asset>,
    shape: Shape,
    extra_reasons: Vec<String>,
}

enum Shape {
    SendSol {
        to: Pubkey,
        lamports: u64,
    },
    SendToken {
        to: Pubkey,
        mint: String,
        amount: u64,
    },
    Convert {
        input: Asset,
        output: Asset,
        quote: Box<Quote>,
        input_value_lamports: u64,
    },
}

pub struct AgentWallet {
    pub rpc: RpcClient,
    pub signer: Keypair,
    pub cluster: String,
    pub store: Store,
    pub approver: Arc<dyn Approver>,
    pub gbrain: Gbrain,
    pub jupiter: Jupiter,
    spend_lock: tokio::sync::Mutex<()>,
}

impl AgentWallet {
    pub fn new(
        rpc: RpcClient,
        signer: Keypair,
        cluster: String,
        store: Store,
        approver: Arc<dyn Approver>,
        gbrain: Gbrain,
    ) -> Self {
        Self {
            rpc,
            signer,
            cluster,
            store,
            approver,
            gbrain,
            jupiter: Jupiter::from_env(),
            spend_lock: tokio::sync::Mutex::new(()),
        }
    }

    pub fn address(&self) -> Pubkey {
        self.signer.pubkey()
    }

    pub async fn budget(&self) -> anyhow::Result<Budget> {
        match self.store.budget().await? {
            Some(budget) => Ok(budget),
            None => {
                let budget = Budget::default_for_cluster(&self.cluster);
                self.store.save_budget(&budget).await?;
                Ok(budget)
            }
        }
    }

    fn request_hash(&self, request: &Request) -> String {
        hex::encode(Sha256::digest(format!(
            "manus-agent-op-v1:{}:{}:{}",
            self.cluster,
            self.address(),
            serde_json::to_string(request).unwrap_or_default()
        )))
    }

    /// Run an operation end to end. With `dry_run` nothing is stored or sent.
    pub async fn execute(
        &self,
        request: Request,
        request_id: Option<String>,
        dry_run: bool,
    ) -> anyhow::Result<Value> {
        if let Some(id) = &request_id {
            validate_request_id(id)?;
        }
        let request_hash = self.request_hash(&request);
        if let Some(id) = &request_id {
            if let Some((stored_hash, record)) = self.store.find_by_request(id).await? {
                if stored_hash != request_hash {
                    return Err(anyhow::anyhow!(
                        "request_id '{id}' was already used for a different operation"
                    ));
                }
                return Ok(json!({ "idempotent_replay": true, "operation": view(&record) }));
            }
        }

        // One spending decision at a time, so concurrent calls cannot jointly exceed a limit.
        let _guard = self.spend_lock.lock().await;
        let budget = self.budget().await?;
        let prepared = self.prepare(&request, &budget).await?;
        let (effects, exposure) = self.evaluate(&prepared).await?;
        let spent = self.store.spent_last_day(None).await?;
        let decision = merge(decide(&budget, &spent, &exposure), &prepared.extra_reasons);

        if dry_run {
            return Ok(json!({
                "dry_run": true,
                "summary": prepared.summary,
                "decision": decision,
                "exposure": exposure_view(&exposure),
                "simulation": effects_view(&effects),
                "approval_method": self.approver.describe(),
            }));
        }

        let id = format!("op_{}", uuid::Uuid::new_v4().simple());
        let params = serde_json::to_value(&request)?;
        self.store
            .insert(
                &id,
                request_id.as_deref(),
                &request_hash,
                request.kind(),
                &prepared.summary,
                &params,
            )
            .await?;
        self.store
            .set_evaluation(
                &id,
                &prepared.summary,
                &exposure,
                &serde_json::to_value(&decision)?,
            )
            .await?;

        let mut prepared = prepared;
        if let Decision::NeedsApproval { reasons } = &decision {
            let reason = approval_reason(&prepared.summary, reasons);
            self.gbrain.emit(
                "wallet.approval",
                format!(
                    "Manus asks for approval: {} ({})",
                    prepared.summary,
                    reasons.join("; ")
                ),
                5,
            );
            let approver = self.approver.clone();
            let outcome = tokio::task::spawn_blocking(move || approver.request(&reason)).await?;
            match outcome {
                ApprovalOutcome::Approved { method } => {
                    self.store.set_approval(&id, &method).await?;
                }
                ApprovalOutcome::Denied { reason } => {
                    self.store
                        .set_status(&id, "denied", None, None, Some(&reason))
                        .await?;
                    self.report(&id).await;
                    return self.result(&id).await;
                }
            }
            // The approved bytes may have outlived their blockhash while the human decided.
            let still_valid = self
                .rpc
                .is_blockhash_valid(&prepared.blockhash, CommitmentConfig::processed())
                .await
                .unwrap_or(false);
            if !still_valid {
                let rebuilt = self.prepare(&request, &budget).await?;
                let (_, rebuilt_exposure) = self.evaluate(&rebuilt).await?;
                if !within_approved(&exposure, &rebuilt_exposure) {
                    self.store
                        .set_status(
                            &id,
                            "failed",
                            None,
                            None,
                            Some("effects changed after approval; ask again"),
                        )
                        .await?;
                    return self.result(&id).await;
                }
                prepared = rebuilt;
            } else if let Err(error) = self.evaluate(&prepared).await {
                self.store
                    .set_status(&id, "failed", None, None, Some(&error.to_string()))
                    .await?;
                return self.result(&id).await;
            }
        } else if let Decision::Deny { reason } = &decision {
            self.store
                .set_status(&id, "denied", None, None, Some(reason))
                .await?;
            return self.result(&id).await;
        }

        self.submit(&id, &prepared).await?;
        self.report(&id).await;
        self.result(&id).await
    }

    async fn submit(&self, id: &str, prepared: &Prepared) -> anyhow::Result<()> {
        let signature = prepared.tx.signatures[0];
        self.store
            .set_submitting(id, &signature.to_string(), &prepared.blockhash.to_string())
            .await?;
        let config = RpcSendTransactionConfig {
            // The exact bytes were simulated a moment ago.
            skip_preflight: true,
            max_retries: Some(5),
            ..Default::default()
        };
        if let Err(error) = self
            .rpc
            .send_transaction_with_config(&prepared.tx, config)
            .await
        {
            self.store
                .set_status(
                    id,
                    "submission_unknown",
                    None,
                    None,
                    Some(&error.to_string()),
                )
                .await?;
            return Ok(());
        }
        self.store
            .set_status(id, "submitted", None, None, None)
            .await?;
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(750)).await;
            let status = self.refresh(id).await?;
            if matches!(
                status.as_str(),
                "confirmed" | "finalized" | "failed" | "expired"
            ) {
                break;
            }
        }
        Ok(())
    }

    /// Update an operation's on-chain status and return it.
    pub async fn refresh(&self, id: &str) -> anyhow::Result<String> {
        let record = self
            .store
            .get(id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("unknown operation {id}"))?;
        let Some(signature) = record.signature.as_deref() else {
            return Ok(record.status);
        };
        if matches!(
            record.status.as_str(),
            "finalized" | "failed" | "expired" | "denied"
        ) {
            return Ok(record.status);
        }
        let signature = Signature::from_str(signature)?;
        let statuses = self
            .rpc
            .get_signature_statuses_with_history(&[signature])
            .await?
            .value;
        let (status, slot, error) = match statuses.into_iter().next().flatten() {
            Some(found) if found.err.is_some() => (
                "failed".to_string(),
                Some(found.slot as i64),
                found.err.map(|e| format!("{e:?}")),
            ),
            Some(found) => {
                let status = match found.confirmation_status {
                    Some(TransactionConfirmationStatus::Finalized) => "finalized",
                    Some(TransactionConfirmationStatus::Confirmed) => "confirmed",
                    Some(TransactionConfirmationStatus::Processed) => "processed",
                    None => "submitted",
                };
                (status.to_string(), Some(found.slot as i64), None)
            }
            None => {
                // Unseen transactions can still land until their blockhash expires.
                let blockhash = self.store.blockhash(id).await?;
                let expired = match blockhash.and_then(|b| Hash::from_str(&b).ok()) {
                    Some(hash) => !self
                        .rpc
                        .is_blockhash_valid(&hash, CommitmentConfig::processed())
                        .await
                        .unwrap_or(true),
                    None => false,
                };
                if expired {
                    (
                        "expired".to_string(),
                        None,
                        Some("blockhash expired before landing".to_string()),
                    )
                } else {
                    (record.status.clone(), None, record.error.clone())
                }
            }
        };
        if status != record.status || slot.is_some() {
            self.store
                .set_status(id, &status, None, slot, error.as_deref())
                .await?;
        }
        Ok(status)
    }

    pub async fn operation(&self, id: &str) -> anyhow::Result<Value> {
        self.refresh(id).await?;
        self.result(id).await
    }

    async fn result(&self, id: &str) -> anyhow::Result<Value> {
        let record = self
            .store
            .get(id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("unknown operation {id}"))?;
        Ok(json!({ "operation": view(&record) }))
    }

    async fn report(&self, id: &str) {
        if let Ok(Some(record)) = self.store.get(id).await {
            let priority = if record.status == "denied" || record.status == "failed" {
                3
            } else {
                1
            };
            self.gbrain.emit(
                "wallet.operation",
                format!(
                    "Manus {} [{}] {}{}",
                    record.kind,
                    record.status,
                    record.summary,
                    record
                        .signature
                        .as_ref()
                        .map(|s| format!(" sig={s}"))
                        .unwrap_or_default()
                ),
                priority,
            );
        }
    }

    // ------------------------------------------------------------ preparation

    async fn prepare(&self, request: &Request, budget: &Budget) -> anyhow::Result<Prepared> {
        let wallet = self.address();
        match request {
            Request::Send {
                to,
                amount,
                token,
                memo,
            } => {
                let to = Pubkey::from_str(to.trim())
                    .map_err(|_| anyhow::anyhow!("'{to}' is not a Solana address"))?;
                if to == wallet {
                    return Err(anyhow::anyhow!("refusing to send to the wallet itself"));
                }
                if let Some(memo) = memo {
                    if memo.len() > 200 {
                        return Err(anyhow::anyhow!("memo is limited to 200 bytes"));
                    }
                }
                let asset = tx::resolve_asset(&self.rpc, token, &self.cluster).await?;
                let units = parse_units(amount, asset.decimals)?;
                let summary = format!(
                    "Send {} {} → {}",
                    format_units(units, asset.decimals),
                    asset.symbol,
                    to
                );
                let (instructions, shape) = match asset.mint {
                    None => (
                        tx::sol_transfer_instructions(&wallet, &to, units, memo.as_deref()),
                        Shape::SendSol {
                            to,
                            lamports: units,
                        },
                    ),
                    Some(mint) => (
                        tx::token_transfer_instructions(
                            &wallet,
                            &to,
                            &asset,
                            units,
                            memo.as_deref(),
                        )?,
                        Shape::SendToken {
                            to,
                            mint: mint.to_string(),
                            amount: units,
                        },
                    ),
                };
                let signed = tx::sign_instructions(&self.rpc, &self.signer, &instructions).await?;
                Ok(Prepared {
                    blockhash: *signed.message.recent_blockhash(),
                    tx: signed,
                    summary,
                    watched: vec![asset],
                    shape,
                    extra_reasons: Vec::new(),
                })
            }
            Request::Swap {
                from,
                to,
                amount,
                slippage_bps,
            } => {
                self.prepare_conversion(from, to, amount, *slippage_bps, budget, "Swap")
                    .await
            }
            Request::Stake { amount } => {
                self.prepare_conversion("SOL", JITO_SOL_MINT, amount, None, budget, "Stake")
                    .await
            }
            Request::Unstake { amount } => {
                self.prepare_conversion(JITO_SOL_MINT, "SOL", amount, None, budget, "Unstake")
                    .await
            }
        }
    }

    async fn prepare_conversion(
        &self,
        from: &str,
        to: &str,
        amount: &str,
        slippage_bps: Option<u16>,
        budget: &Budget,
        verb: &str,
    ) -> anyhow::Result<Prepared> {
        if self.cluster != "mainnet-beta" {
            return Err(anyhow::anyhow!(
                "swaps and liquid staking route through Jupiter and are available on mainnet only"
            ));
        }
        let input = tx::resolve_asset(&self.rpc, from, &self.cluster).await?;
        let output = tx::resolve_asset(&self.rpc, to, &self.cluster).await?;
        if input.mint_or_wsol() == output.mint_or_wsol() {
            return Err(anyhow::anyhow!("input and output tokens are the same"));
        }
        let units = if amount.trim().eq_ignore_ascii_case("all") {
            if input.mint.is_none() {
                return Err(anyhow::anyhow!(
                    "'all' is only supported for tokens, not SOL"
                ));
            }
            self.token_balance(&input).await?
        } else {
            parse_units(amount, input.decimals)?
        };
        if units == 0 {
            return Err(anyhow::anyhow!("nothing to convert"));
        }
        let slippage = slippage_bps.unwrap_or(50);
        let mut extra_reasons = Vec::new();
        if slippage > budget.max_slippage_bps {
            extra_reasons.push(format!(
                "slippage {slippage} bps exceeds the agent limit of {} bps",
                budget.max_slippage_bps
            ));
        }
        // The Manus fee is only charged when the fee wallet can receive the output token;
        // the agent's wallet never pays to create that account.
        let fee_account = tx::fee_account_for(&output);
        let fee_bps = match self
            .rpc
            .get_account_with_commitment(&fee_account, CommitmentConfig::confirmed())
            .await
        {
            Ok(response) if response.value.is_some() => tx::PLATFORM_FEE_BPS,
            _ => 0,
        };
        let quote = self
            .jupiter
            .quote(
                &input.mint_or_wsol(),
                &output.mint_or_wsol(),
                units,
                slippage,
                fee_bps,
            )
            .await?;
        // Jupiter reports impact as a fraction; reading it that way errs toward asking.
        let impact_percent = quote.price_impact_pct * 100.0;
        if impact_percent > MAX_PRICE_IMPACT_PCT {
            extra_reasons.push(format!(
                "price impact {impact_percent:.2}% is above {MAX_PRICE_IMPACT_PCT}%"
            ));
        }
        let input_value_lamports = if input.mint.is_none() {
            units
        } else if output.mint.is_none() {
            quote.out_amount
        } else {
            self.jupiter
                .quote(&input.mint_or_wsol(), SOL_MINT, units, 100, 0)
                .await
                .map_err(|e| anyhow::anyhow!("cannot value {} in SOL: {e}", input.symbol))?
                .out_amount
        };
        let fee_note = if quote.platform_fee > 0 {
            format!(
                " · Manus fee {} {} (0.1%)",
                format_units(quote.platform_fee, output.decimals),
                output.symbol
            )
        } else {
            String::new()
        };
        let summary = format!(
            "{verb} {} {} → ≈{} {}{}{fee_note}",
            format_units(units, input.decimals),
            input.symbol,
            format_units(quote.out_amount, output.decimals),
            output.symbol,
            if quote.route.is_empty() {
                String::new()
            } else {
                format!(" via {}", quote.route)
            }
        );
        let signed = self
            .jupiter
            .signed_swap(&quote, &self.signer, Some(fee_account))
            .await?;
        Ok(Prepared {
            blockhash: *signed.message.recent_blockhash(),
            tx: signed,
            summary,
            watched: vec![input.clone(), output.clone()],
            shape: Shape::Convert {
                input,
                output,
                quote: Box::new(quote),
                input_value_lamports,
            },
            extra_reasons,
        })
    }

    /// Inspect and simulate a prepared transaction, then measure its exposure.
    async fn evaluate(&self, prepared: &Prepared) -> anyhow::Result<(SimulatedEffects, Exposure)> {
        let wallet = self.address();
        tx::inspect_structure(&prepared.tx, &wallet)?;
        let watched: Vec<&Asset> = prepared.watched.iter().collect();
        let effects = tx::simulate(&self.rpc, &prepared.tx, &wallet, &watched).await?;
        let exposure = measure(&prepared.shape, &effects)?;
        Ok((effects, exposure))
    }

    async fn token_balance(&self, asset: &Asset) -> anyhow::Result<u64> {
        let (Some(mint), Some(program)) = (asset.mint, asset.token_program) else {
            return Ok(self.rpc.get_balance(&self.address()).await?);
        };
        let account = spl_associated_token_account::get_associated_token_address_with_program_id(
            &self.address(),
            &mint,
            &program,
        );
        match self.rpc.get_token_account_balance(&account).await {
            Ok(balance) => Ok(balance.amount.parse().unwrap_or(0)),
            Err(_) => Ok(0),
        }
    }

    // ------------------------------------------------------------ read models

    pub async fn balances(&self) -> anyhow::Result<Value> {
        let wallet = self.address();
        let lamports = self.rpc.get_balance(&wallet).await?;
        let mut tokens = Vec::new();
        for program in [spl_token::id(), spl_token_2022::id()] {
            let accounts = self
                .rpc
                .get_token_accounts_by_owner(&wallet, TokenAccountsFilter::ProgramId(program))
                .await
                .unwrap_or_default();
            for keyed in accounts {
                if let solana_account_decoder::UiAccountData::Json(parsed) = keyed.account.data {
                    let info = &parsed.parsed["info"];
                    let mint = info["mint"].as_str().unwrap_or_default().to_string();
                    let amount = &info["tokenAmount"];
                    if amount["amount"].as_str() == Some("0") {
                        continue;
                    }
                    tokens.push(json!({
                        "symbol": tx::symbol_for_mint(&mint),
                        "mint": mint,
                        "amount": amount["uiAmountString"],
                        "decimals": amount["decimals"],
                    }));
                }
            }
        }
        Ok(json!({
            "address": wallet.to_string(),
            "cluster": self.cluster,
            "sol": format_units(lamports, 9),
            "tokens": tokens,
        }))
    }

    pub async fn status(&self) -> anyhow::Result<Value> {
        let budget = self.budget().await?;
        let spent = self.store.spent_last_day(None).await?;
        let balances = self.balances().await?;
        Ok(json!({
            "address": self.address().to_string(),
            "cluster": self.cluster,
            "balances": balances,
            "budget": budget_view(&budget, &self.cluster),
            "spent_last_24h": {
                "send_sol": format_units(spent.send_sol, 9),
                "convert_sol": format_units(spent.convert_sol, 9),
                "tokens": spent.send_tokens.iter().map(|(mint, amount)| json!({
                    "mint": mint, "symbol": tx::symbol_for_mint(mint), "base_units": amount
                })).collect::<Vec<_>>(),
            },
            "approval_method": self.approver.describe(),
            "rules": [
                "Operations inside the budget execute immediately.",
                "Anything above it asks the human through the approval method; the agent cannot approve on its own.",
                "Every operation is simulated first and judged by its real balance changes.",
                "Use request_id to make retries safe; the same id never pays twice.",
                "Transfers are free. Swaps, stake and unstake carry a 0.1% Manus fee, shown in the summary."
            ]
        }))
    }

    pub async fn history(&self, limit: i64) -> anyhow::Result<Value> {
        for pending in self.store.unsettled().await? {
            let _ = self.refresh(&pending.id).await;
        }
        let records = self.store.recent(limit).await?;
        Ok(json!({ "operations": records.iter().map(view).collect::<Vec<_>>() }))
    }

    /// Change the budget. Tightening applies at once; widening needs human approval.
    pub async fn change_budget(&self, proposed: Budget, why: &str) -> anyhow::Result<Value> {
        let _guard = self.spend_lock.lock().await;
        let current = self.budget().await?;
        if current == proposed {
            return Ok(json!({ "changed": false, "budget": budget_view(&current, &self.cluster) }));
        }
        let mut approved_by = "not required (budget only tightened)".to_string();
        if !current.permits_without_approval(&proposed) {
            let reason = format!(
                "Manus: raise the agent wallet budget. {}",
                why.chars().take(120).collect::<String>()
            );
            self.gbrain.emit(
                "wallet.approval",
                format!("Manus asks to widen budget: {why}"),
                5,
            );
            let approver = self.approver.clone();
            match tokio::task::spawn_blocking(move || approver.request(&reason)).await? {
                ApprovalOutcome::Approved { method } => approved_by = method,
                ApprovalOutcome::Denied { reason } => {
                    return Ok(json!({ "changed": false, "denied": reason }));
                }
            }
        }
        self.store.save_budget(&proposed).await?;
        self.gbrain
            .emit("wallet.budget", format!("Manus budget changed: {why}"), 2);
        Ok(json!({
            "changed": true,
            "approved_by": approved_by,
            "budget": budget_view(&proposed, &self.cluster)
        }))
    }
}

/// Derive what an operation really does from simulation, and check it matches the request.
fn measure(shape: &Shape, effects: &SimulatedEffects) -> anyhow::Result<Exposure> {
    match shape {
        Shape::SendSol { to, lamports } => {
            let total = effects.sol_out();
            if total < *lamports || total > lamports + SEND_FEE_ALLOWANCE {
                return Err(anyhow::anyhow!(
                    "simulated SOL outflow {total} does not match a transfer of {lamports}"
                ));
            }
            Ok(Exposure {
                category: Category::Send,
                sol_out: *lamports,
                fees: total - lamports,
                token_out: None,
                recipient: Some(to.to_string()),
            })
        }
        Shape::SendToken { to, mint, amount } => {
            let (token_out, _) = effects.token_delta(mint);
            if token_out != *amount {
                return Err(anyhow::anyhow!(
                    "simulated token outflow {token_out} does not match the requested {amount}"
                ));
            }
            let fees = effects.sol_out();
            if fees > TOKEN_SEND_SOL_ALLOWANCE {
                return Err(anyhow::anyhow!(
                    "token transfer would also spend {fees} lamports"
                ));
            }
            Ok(Exposure {
                category: Category::Send,
                sol_out: 0,
                fees,
                token_out: Some((mint.clone(), token_out)),
                recipient: Some(to.to_string()),
            })
        }
        Shape::Convert {
            input,
            output,
            quote,
            input_value_lamports,
        } => {
            match input.mint {
                None => {
                    let sol_out = effects.sol_out();
                    if sol_out > quote.in_amount + SWAP_SOL_ALLOWANCE {
                        return Err(anyhow::anyhow!(
                            "swap would spend {sol_out} lamports for an input of {}",
                            quote.in_amount
                        ));
                    }
                }
                Some(mint) => {
                    let (spent, _) = effects.token_delta(&mint.to_string());
                    if spent != quote.in_amount {
                        return Err(anyhow::anyhow!(
                            "swap would move {spent} input units instead of {}",
                            quote.in_amount
                        ));
                    }
                    if effects.sol_out() > SWAP_SOL_ALLOWANCE {
                        return Err(anyhow::anyhow!("swap would also spend SOL beyond fees"));
                    }
                }
            }
            let received = match output.mint {
                None => effects.sol_after.saturating_sub(effects.sol_before) + SWAP_SOL_ALLOWANCE,
                Some(mint) => effects.token_delta(&mint.to_string()).1,
            };
            if received < quote.min_out_amount {
                return Err(anyhow::anyhow!(
                    "simulation received {received}, below the quoted minimum {}",
                    quote.min_out_amount
                ));
            }
            let fees = match input.mint {
                None => effects.sol_out().saturating_sub(quote.in_amount),
                Some(_) => effects.sol_out(),
            };
            Ok(Exposure {
                category: Category::Convert,
                sol_out: *input_value_lamports,
                fees,
                token_out: None,
                recipient: None,
            })
        }
    }
}

fn merge(decision: Decision, extra: &[String]) -> Decision {
    if extra.is_empty() {
        return decision;
    }
    match decision {
        Decision::Auto => Decision::NeedsApproval {
            reasons: extra.to_vec(),
        },
        Decision::NeedsApproval { mut reasons } => {
            reasons.extend_from_slice(extra);
            Decision::NeedsApproval { reasons }
        }
        deny => deny,
    }
}

/// A rebuilt transaction may not do more than the human approved.
fn within_approved(approved: &Exposure, rebuilt: &Exposure) -> bool {
    // Transfers are exact. A conversion's SOL valuation may move with a fresh quote.
    let sol_tolerance = match approved.category {
        Category::Send => 0,
        Category::Convert => approved.sol_out / 100,
    };
    approved.category == rebuilt.category
        && approved.recipient == rebuilt.recipient
        && rebuilt.sol_out <= approved.sol_out + sol_tolerance
        && match (&approved.token_out, &rebuilt.token_out) {
            (None, None) => true,
            (Some((a_mint, a_amount)), Some((b_mint, b_amount))) => {
                a_mint == b_mint && b_amount <= a_amount
            }
            _ => false,
        }
}

fn approval_reason(summary: &str, reasons: &[String]) -> String {
    let mut text = format!(
        "Manus agent wallet: {summary}. Reason: {}",
        reasons.join("; ")
    );
    if text.len() > 240 {
        text.truncate(237);
        text.push('…');
    }
    text
}

fn validate_request_id(id: &str) -> anyhow::Result<()> {
    if id.len() < 4
        || id.len() > 128
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '.'))
    {
        return Err(anyhow::anyhow!(
            "request_id must be 4-128 characters of letters, digits, '-', '_', ':' or '.'"
        ));
    }
    Ok(())
}

fn view(record: &OperationRecord) -> Value {
    json!({
        "id": record.id,
        "request_id": record.request_id,
        "kind": record.kind,
        "status": record.status,
        "summary": record.summary,
        "exposure": record.exposure.as_ref().map(exposure_view),
        "decision": record.decision,
        "approved_by": record.approval,
        "signature": record.signature,
        "explorer": record.signature.as_ref().map(|s| format!("https://solscan.io/tx/{s}")),
        "slot": record.slot,
        "error": record.error,
        "created_at": record.created_at,
        "updated_at": record.updated_at,
    })
}

fn exposure_view(exposure: &Exposure) -> Value {
    json!({
        "category": exposure.category,
        "sol_out": format_units(exposure.sol_out, 9),
        "fees_sol": format_units(exposure.fees, 9),
        "token_out": exposure.token_out.as_ref().map(|(mint, amount)| json!({
            "mint": mint, "symbol": tx::symbol_for_mint(mint), "base_units": amount
        })),
        "recipient": exposure.recipient,
    })
}

fn effects_view(effects: &SimulatedEffects) -> Value {
    json!({
        "sol_before": format_units(effects.sol_before, 9),
        "sol_after": format_units(effects.sol_after, 9),
        "tokens": effects.tokens.iter().map(|(mint, before, after)| json!({
            "mint": mint, "symbol": tx::symbol_for_mint(mint), "before": before, "after": after
        })).collect::<Vec<_>>(),
        "compute_units": effects.units_consumed,
    })
}

/// Human-unit view of a budget.
pub fn budget_view(budget: &Budget, _cluster: &str) -> Value {
    let limits = |l: &super::budget::Limits, decimals: u8| json!({ "per_op": format_units(l.per_op, decimals), "daily": format_units(l.daily, decimals) });
    json!({
        "send_sol": limits(&budget.send_sol, 9),
        "convert_sol": limits(&budget.convert_sol, 9),
        "send_tokens": budget.send_tokens.iter().map(|(mint, l)| {
            let decimals = if tx::symbol_for_mint(mint) == Some("USDC") || tx::symbol_for_mint(mint) == Some("USDT") { 6 } else { 9 };
            json!({ "mint": mint, "symbol": tx::symbol_for_mint(mint), "limits": limits(l, decimals) })
        }).collect::<Vec<_>>(),
        "trusted_recipients": budget.trusted_recipients,
        "require_trusted_recipients": budget.require_trusted_recipients,
        "max_slippage_bps": budget.max_slippage_bps,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn effects(sol_before: u64, sol_after: u64, tokens: Vec<(&str, u64, u64)>) -> SimulatedEffects {
        SimulatedEffects {
            sol_before,
            sol_after,
            tokens: tokens
                .into_iter()
                .map(|(m, b, a)| (m.to_string(), b, a))
                .collect(),
            units_consumed: None,
        }
    }

    #[test]
    fn sol_send_must_match_simulation() {
        let to = Pubkey::new_unique();
        let shape = Shape::SendSol {
            to,
            lamports: 1_000,
        };
        let exposure = measure(&shape, &effects(10_000, 3_995, vec![])).unwrap();
        assert_eq!(exposure.sol_out, 1_000);
        assert_eq!(exposure.fees, 5_005);
        assert_eq!(exposure.recipient, Some(to.to_string()));
        // A transaction draining far more than requested is rejected outright.
        assert!(measure(&shape, &effects(10_000_000, 0, vec![])).is_err());
    }

    #[test]
    fn token_send_must_move_exactly_the_requested_amount() {
        let shape = Shape::SendToken {
            to: Pubkey::new_unique(),
            mint: "Mint".into(),
            amount: 50,
        };
        let exposure = measure(&shape, &effects(10_000, 5_000, vec![("Mint", 100, 50)])).unwrap();
        assert_eq!(exposure.token_out, Some(("Mint".into(), 50)));
        assert_eq!((exposure.sol_out, exposure.fees), (0, 5_000));
        assert!(measure(&shape, &effects(10_000, 5_000, vec![("Mint", 100, 0)])).is_err());
    }

    #[test]
    fn rebuilt_transactions_cannot_exceed_approval() {
        let approved = Exposure {
            category: Category::Send,
            sol_out: 1_000_000_000,
            fees: 5_000,
            token_out: None,
            recipient: Some("A".into()),
        };
        let mut rebuilt = approved.clone();
        rebuilt.fees += 5_000;
        assert!(within_approved(&approved, &rebuilt));
        rebuilt.sol_out += 1;
        assert!(!within_approved(&approved, &rebuilt));
        let mut conversion = approved.clone();
        conversion.category = Category::Convert;
        conversion.recipient = None;
        let mut requoted = conversion.clone();
        requoted.sol_out += 5_000_000;
        assert!(within_approved(&conversion, &requoted));
        requoted.sol_out = 1_200_000_000;
        assert!(!within_approved(&conversion, &requoted));
        let mut redirected = approved.clone();
        redirected.recipient = Some("B".into());
        assert!(!within_approved(&approved, &redirected));
    }

    #[test]
    fn extra_reasons_turn_auto_into_approval() {
        assert_eq!(merge(Decision::Auto, &[]), Decision::Auto);
        assert!(matches!(
            merge(Decision::Auto, &["slippage".into()]),
            Decision::NeedsApproval { .. }
        ));
    }

    #[test]
    fn request_ids_are_validated() {
        assert!(validate_request_id("pay-2026-09-26").is_ok());
        assert!(validate_request_id("x").is_err());
        assert!(validate_request_id("has space").is_err());
    }
}
