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
use super::invoice::{self, SignedInvoice};
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tag: Option<String>,
    },
    /// Liquid staking: SOL → JitoSOL.
    Stake { amount: String },
    /// Instant unstake through the market: JitoSOL → SOL. `amount` may be "all".
    Unstake { amount: String },
    /// Pay a signed invoice. The invoice id is the memo and the idempotency key.
    PayInvoice {
        invoice_id: String,
        payee: String,
        token: String,
        amount: String,
        description: String,
        expires_at: String,
        signature: String,
    },
    /// Pay an HTTP 402 (x402 v2) challenge.
    PayX402 {
        resource_url: String,
        domain: String,
        payee: String,
        token: String,
        amount: String,
        memo: String,
        #[serde(default)]
        description: Option<String>,
    },
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
            Request::PayInvoice { .. } => "invoice",
            Request::PayX402 { .. } => "x402",
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
    /// Set when this invoice pays someone who is not trusted yet.
    trust_after_approval: Option<String>,
    /// Strategy or signal tag attached to a conversion trade.
    tag: Option<String>,
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
    approval_lock: tokio::sync::Mutex<()>,
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
            approval_lock: tokio::sync::Mutex::new(()),
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
                let _ = self.refresh(&record.id).await;
                return Ok(json!({
                    "idempotent_replay": true,
                    "operation": view(&self.store.get(&record.id).await?.unwrap_or(record))
                }));
            }
        }

        if !dry_run {
            self.reconcile_unsettled().await?;
        }

        // Serialize within this process and across every MCP process using the
        // wallet database. Neither lock is held during approval or broadcast.
        let guard = self.spend_lock.lock().await;
        let file_guard = self.store.lock_spending().await?;
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
        let budget = self.budget().await?;
        let prepared = self.prepare_unique(&request, &budget).await?;
        let (effects, exposure) = self.evaluate(&prepared).await?;
        let spent = self.store.spent_last_day(None).await?;
        let mut extra_reasons = prepared.extra_reasons.clone();
        if exposure.category == Category::Convert && budget.swap_cooldown_secs > 0 {
            if let Some(elapsed) = self.store.last_conversion_age_secs(None).await? {
                if elapsed < budget.swap_cooldown_secs {
                    extra_reasons.push(format!(
                        "swap cooldown active ({elapsed}s elapsed, minimum {}s)",
                        budget.swap_cooldown_secs
                    ));
                }
            }
        }
        if exposure.category == Category::Convert && budget.max_daily_loss_sol > 0 {
            let is_defensive_exit = match &prepared.shape {
                Shape::Convert { output, .. } => {
                    matches!(output.symbol.as_str(), "SOL" | "USDC" | "USDT")
                }
                _ => false,
            };
            if !is_defensive_exit {
                let trades = self.store.committed_trades().await?;
                let realized_24h = crate::trading::realized_pnl_last_24h_lamports(&trades);
                if realized_24h < 0
                    && (realized_24h.unsigned_abs() as u64) >= budget.max_daily_loss_sol
                {
                    extra_reasons.push(format!(
                        "daily loss circuit breaker active (realized 24h PnL -{} SOL exceeds max_daily_loss_sol {} SOL)",
                        format_units(realized_24h.unsigned_abs() as u64, 9),
                        format_units(budget.max_daily_loss_sol, 9)
                    ));
                }
            }
        }
        let decision = merge(decide(&budget, &spent, &exposure), &extra_reasons);
        let trust_payee = prepared.trust_after_approval.clone();

        if dry_run {
            return Ok(json!({
                "dry_run": true,
                "summary": prepared.summary,
                "decision": decision,
                "exposure": exposure_view(&exposure),
                "simulation": effects_view(&effects),
                "approval_method": self.approver.describe(),
                "trusts_recipient_on_approval": trust_payee.is_some(),
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

        let reasons = match &decision {
            Decision::Deny { reason } => {
                self.store
                    .set_status(&id, "denied", None, None, Some(reason))
                    .await?;
                return self.result(&id).await;
            }
            Decision::Auto => {
                if let Some(stopped) = self.block_closed_invoice(&id, &request).await? {
                    return Ok(stopped);
                }
                self.reserve(&id, &prepared).await?;
                drop(file_guard);
                drop(guard);
                self.broadcast(&id, &prepared).await?;
                self.report(&id).await;
                return self.result(&id).await;
            }
            Decision::NeedsApproval { reasons } => reasons.clone(),
        };

        // Reserve the amount while the human decides, then let other calls proceed.
        let pending_signature = prepared.tx.signatures[0].to_string();
        self.store
            .set_status(
                &id,
                "awaiting_approval",
                Some(&pending_signature),
                None,
                None,
            )
            .await?;
        drop(file_guard);
        drop(guard);

        let reason = if trust_payee.is_some() {
            let mut text = "Manus: first payment to a new agent. Approving allows later payments to this address inside your budget.".to_string();
            let extra: Vec<&str> = reasons
                .iter()
                .filter(|reason| reason.as_str() != invoice::FIRST_PAYMENT_REASON)
                .map(String::as_str)
                .collect();
            if !extra.is_empty() {
                text.push(' ');
                text.push_str(&extra.join("; "));
            }
            text.push(' ');
            text.push_str(&prepared.summary);
            if text.len() > 240 {
                let mut end = 237;
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                text.truncate(end);
                text.push('…');
            }
            text
        } else {
            approval_reason(&prepared.summary, &reasons)
        };
        self.gbrain.emit(
            "wallet.approval",
            format!(
                "Manus asks for approval: {} ({})",
                prepared.summary,
                reasons.join("; ")
            ),
            5,
        );
        let outcome = {
            // One system prompt at a time.
            let _prompt = self.approval_lock.lock().await;
            let approver = self.approver.clone();
            tokio::task::spawn_blocking(move || approver.request(&reason)).await?
        };
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

        let guard = self.spend_lock.lock().await;
        let file_guard = self.store.lock_spending().await?;
        if self
            .store
            .get(&id)
            .await?
            .is_none_or(|record| record.status != "awaiting_approval")
        {
            return self
                .fail(&id, "approval reservation expired or changed; ask again")
                .await;
        }
        if self.budget().await? != budget {
            return self
                .fail(&id, "budget changed after approval; ask again")
                .await;
        }
        // The approved bytes may have outlived their blockhash while the human decided.
        let still_valid = self
            .rpc
            .is_blockhash_valid(&prepared.blockhash, CommitmentConfig::processed())
            .await
            .unwrap_or(false);
        let mut prepared = prepared;
        let signature = prepared.tx.signatures[0].to_string();
        let collides = self.store.signature_in_use(&signature, Some(&id)).await?;
        if !still_valid || collides {
            let rebuilt = match self.prepare_unique(&request, &budget).await {
                Ok(rebuilt) => rebuilt,
                Err(error) => return self.fail(&id, &error.to_string()).await,
            };
            match self.evaluate(&rebuilt).await {
                Ok((_, rebuilt_exposure)) if within_approved(&exposure, &rebuilt_exposure) => {
                    prepared = rebuilt;
                }
                Ok(_) => {
                    return self
                        .fail(&id, "effects changed after approval; ask again")
                        .await
                }
                Err(error) => return self.fail(&id, &error.to_string()).await,
            }
        } else if let Err(error) = self.evaluate(&prepared).await {
            return self.fail(&id, &error.to_string()).await;
        }
        if let Some(stopped) = self.block_closed_invoice(&id, &request).await? {
            return Ok(stopped);
        }
        self.reserve(&id, &prepared).await?;
        if let Some(payee) = &trust_payee {
            if let Err(error) = self.trust_recipient(payee).await {
                return self.fail(&id, &error.to_string()).await;
            }
        }
        drop(file_guard);
        drop(guard);
        self.broadcast(&id, &prepared).await?;
        self.report(&id).await;
        self.result(&id).await
    }

    /// Build the transaction, making sure its signature is not already owned by another
    /// operation. Identical payments built within one blockhash would otherwise be the
    /// same transaction, land once, and be counted twice. Call with both spending locks held.
    async fn prepare_unique(&self, request: &Request, budget: &Budget) -> anyhow::Result<Prepared> {
        for _ in 0..20 {
            let prepared = self.prepare(request, budget).await?;
            let signature = prepared.tx.signatures[0].to_string();
            if !self.store.signature_in_use(&signature, None).await? {
                return Ok(prepared);
            }
            // Wait for the next blockhash, which makes the bytes different.
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
        Err(anyhow::anyhow!(
            "could not build a distinct transaction; an identical one is still in flight"
        ))
    }

    async fn fail(&self, id: &str, error: &str) -> anyhow::Result<Value> {
        self.store
            .set_status(id, "failed", None, None, Some(error))
            .await?;
        self.report(id).await;
        self.result(id).await
    }

    /// Record the signature and blockhash before broadcast; from here the amount counts as spent.
    async fn reserve(&self, id: &str, prepared: &Prepared) -> anyhow::Result<()> {
        let signature = prepared.tx.signatures[0];
        self.store
            .set_submitting(id, &signature.to_string(), &prepared.blockhash.to_string())
            .await?;
        if let Shape::Convert {
            input,
            output,
            quote,
            input_value_lamports,
        } = &prepared.shape
        {
            let _ = self
                .store
                .record_trade(&crate::store::TradeRecord {
                    operation_id: id.to_string(),
                    kind: "convert".to_string(),
                    input_mint: input.mint_or_wsol(),
                    input_symbol: input.symbol.clone(),
                    input_amount: quote.in_amount,
                    input_decimals: input.decimals,
                    output_mint: output.mint_or_wsol(),
                    output_symbol: output.symbol.clone(),
                    output_amount: quote.out_amount,
                    output_decimals: output.decimals,
                    sol_value_lamports: *input_value_lamports,
                    tag: prepared.tag.clone(),
                    created_at: chrono::Utc::now().to_rfc3339(),
                })
                .await;
        }
        Ok(())
    }

    /// Send the reserved transaction and follow it to confirmation.
    async fn broadcast(&self, id: &str, prepared: &Prepared) -> anyhow::Result<()> {
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
                .advance_chain_status(id, "submission_unknown", None, Some(&error.to_string()))
                .await?;
            return Ok(());
        }
        self.store
            .advance_chain_status(id, "submitted", None, None)
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
            "finalized" | "failed" | "expired" | "denied" | "awaiting_approval"
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
                if expired && matches!(record.status.as_str(), "submitting" | "submitted") {
                    (
                        "submission_unknown".to_string(),
                        None,
                        Some("signature not found after blockhash expiry; payment outcome is unknown".to_string()),
                    )
                } else {
                    (record.status.clone(), None, record.error.clone())
                }
            }
        };
        if status != record.status {
            return self
                .store
                .advance_chain_status(id, &status, slot, error.as_deref())
                .await;
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
        match request {
            Request::Send {
                to,
                amount,
                token,
                memo,
            } => {
                self.prepare_send(to, amount, token, memo.as_deref(), None, budget)
                    .await
            }
            Request::PayInvoice {
                invoice_id,
                payee,
                token,
                amount,
                description,
                ..
            } => {
                self.prepare_send(
                    payee,
                    amount,
                    token,
                    Some(invoice_id),
                    Some((invoice_id, description)),
                    budget,
                )
                .await
            }
            Request::PayX402 {
                domain,
                payee,
                token,
                amount,
                memo,
                description,
                ..
            } => {
                let label = description.as_deref().unwrap_or(domain.as_str());
                self.prepare_send(
                    payee,
                    amount,
                    token,
                    Some(memo),
                    Some((domain, label)),
                    budget,
                )
                .await
            }
            Request::Swap {
                from,
                to,
                amount,
                slippage_bps,
                tag,
            } => {
                let mut prep = self
                    .prepare_conversion(from, to, amount, *slippage_bps, budget, "Swap")
                    .await?;
                prep.tag = tag.clone();
                Ok(prep)
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
        if budget.require_allowed_tokens
            && !crate::trading::token_is_allowed(&budget.allowed_tokens, &output, &self.cluster)
        {
            extra_reasons.push(format!(
                "output token {} ({}) is not in allowed_tokens",
                output.symbol,
                output.mint_or_wsol()
            ));
        }
        if let Ok(output_safety) = crate::trading::inspect_asset_safety(&self.rpc, &output).await {
            extra_reasons.extend(output_safety.approval_reasons);
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
            trust_after_approval: None,
            tag: None,
        })
    }

    async fn prepare_send(
        &self,
        to: &str,
        amount: &str,
        token: &str,
        memo: Option<&str>,
        invoice: Option<(&str, &str)>,
        budget: &Budget,
    ) -> anyhow::Result<Prepared> {
        let wallet = self.address();
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
        let amount_text = format_units(units, asset.decimals);
        let summary = if let Some((id, description)) = invoice {
            let description: String = description.chars().take(80).collect();
            if memo.is_some_and(|m| m.starts_with("x402:")) {
                format!(
                    "Pay x402 {id} ({description}): {amount_text} {} → {to}",
                    asset.symbol
                )
            } else {
                format!(
                    "Pay invoice {id} ({description}): {amount_text} {} → {to}",
                    asset.symbol
                )
            }
        } else {
            format!("Send {amount_text} {} → {to}", asset.symbol)
        };
        let (instructions, shape) = match asset.mint {
            None => (
                tx::sol_transfer_instructions(&wallet, &to, units, memo),
                Shape::SendSol {
                    to,
                    lamports: units,
                },
            ),
            Some(mint) => (
                tx::token_transfer_instructions(&wallet, &to, &asset, units, memo)?,
                Shape::SendToken {
                    to,
                    mint: mint.to_string(),
                    amount: units,
                },
            ),
        };
        let mut extra_reasons = Vec::new();
        let mut trust_after_approval = None;
        if invoice.is_some() && !invoice::recipient_is_trusted(&budget.trusted_recipients, &to) {
            extra_reasons.push(invoice::FIRST_PAYMENT_REASON.to_string());
            trust_after_approval = Some(to.to_string());
        }
        let signed = tx::sign_instructions(&self.rpc, &self.signer, &instructions).await?;
        Ok(Prepared {
            blockhash: *signed.message.recent_blockhash(),
            tx: signed,
            summary,
            watched: vec![asset],
            shape,
            extra_reasons,
            trust_after_approval,
            tag: None,
        })
    }

    /// Refuse to broadcast an invoice that expired or that someone already paid.
    async fn block_closed_invoice(
        &self,
        id: &str,
        request: &Request,
    ) -> anyhow::Result<Option<Value>> {
        let Request::PayInvoice {
            invoice_id,
            payee,
            token,
            amount,
            expires_at,
            ..
        } = request
        else {
            return Ok(None);
        };
        if invoice::is_expired(expires_at) {
            return Ok(Some(self.fail(id, "invoice expired").await?));
        }
        let payee = Pubkey::from_str(payee)?;
        let asset = tx::resolve_asset(&self.rpc, token, &self.cluster).await?;
        let units = parse_units(amount, asset.decimals)?;
        if let Some(found) = invoice::find_payment(
            &self.rpc,
            &invoice::ExpectedPayment {
                payee,
                invoice_id,
                units,
                mint: asset.mint,
                token_program: asset.token_program,
            },
            None,
        )
        .await?
        {
            let message = if found.finalized {
                "invoice was already paid"
            } else {
                "invoice payment is already confirming"
            };
            return Ok(Some(self.fail(id, message).await?));
        }
        Ok(None)
    }

    /// Record a payee the human just approved. Call while holding both spending locks.
    async fn trust_recipient(&self, payee: &str) -> anyhow::Result<()> {
        let payee = Pubkey::from_str(payee)?;
        let mut budget = self.budget().await?;
        if invoice::recipient_is_trusted(&budget.trusted_recipients, &payee) {
            return Ok(());
        }
        budget.trusted_recipients.push(payee.to_string());
        self.store.save_budget(&budget).await
    }

    /// Sign a bill this wallet can be paid. Hand the result to the paying agent.
    pub async fn create_invoice(
        &self,
        token: &str,
        amount: &str,
        description: &str,
        hours: u64,
    ) -> anyhow::Result<Value> {
        let asset = tx::resolve_asset(&self.rpc, token, &self.cluster).await?;
        let units = parse_units(amount, asset.decimals)?;
        let token_name = match asset.mint {
            None => "SOL".to_string(),
            Some(mint) => {
                let text = mint.to_string();
                tx::symbol_for_mint(&text)
                    .map(str::to_string)
                    .unwrap_or(text)
            }
        };
        let invoice = invoice::Invoice {
            invoice_id: format!("inv_{}", uuid::Uuid::new_v4().simple()),
            payee: self.address().to_string(),
            token: token_name,
            amount: format_units(units, asset.decimals),
            description: description.trim().to_string(),
            expires_at: invoice::expires_after(hours)?,
            cluster: self.cluster.clone(),
        };
        let signed = invoice::sign(&invoice, &self.signer)?;
        let document = serde_json::to_string(&signed)?;
        self.store
            .put_invoice(&invoice.invoice_id, "issued", &document)
            .await?;
        self.gbrain.emit(
            "wallet.invoice",
            format!(
                "Manus invoice {} for {} {}",
                invoice.invoice_id, invoice.amount, invoice.token
            ),
            1,
        );
        Ok(invoice_view(&signed, "open", Value::Null, false))
    }

    /// Pay a signed invoice. The first new payee asks the human, then is trusted.
    pub async fn pay_invoice(&self, document: &Value, dry_run: bool) -> anyhow::Result<Value> {
        let signed = invoice::parse_signed(document)?;
        let invoice = &signed.invoice;
        if invoice.cluster != self.cluster {
            return Err(anyhow::anyhow!(
                "invoice is for {}, this wallet is on {}",
                invoice.cluster,
                self.cluster
            ));
        }
        if invoice::is_expired(&invoice.expires_at) {
            return Err(anyhow::anyhow!("invoice expired"));
        }
        if invoice.payee == self.address().to_string() {
            return Err(anyhow::anyhow!(
                "this wallet issued the invoice; the other agent pays it"
            ));
        }
        let document_json = serde_json::to_string(&signed)?;
        if !dry_run {
            self.store
                .put_invoice(&invoice.invoice_id, "accepted", &document_json)
                .await?;
        }
        if let Some(found) = self.locate_invoice(&signed, None).await? {
            let status = if found.finalized {
                "paid"
            } else {
                "confirming"
            };
            if !dry_run {
                self.store
                    .update_invoice(
                        &invoice.invoice_id,
                        status,
                        Some(&found.signature),
                        None,
                        None,
                    )
                    .await?;
            }
            return Ok(invoice_view(
                &signed,
                status,
                json!({
                    "signature": found.signature,
                    "slot": found.slot,
                    "finalized": found.finalized,
                }),
                self.payee_is_trusted(&invoice.payee).await?,
            ));
        }

        let row = self.store.invoice(&invoice.invoice_id).await?;
        if let Some(row) = row.as_ref().filter(|_| !dry_run) {
            if let Some(operation_id) = &row.operation_id {
                let status = self.refresh(operation_id).await?;
                if !matches!(status.as_str(), "denied" | "failed" | "expired") {
                    return Ok(invoice_view(
                        &signed,
                        &invoice_status_name(&status),
                        self.result(operation_id).await?,
                        self.payee_is_trusted(&invoice.payee).await?,
                    ));
                }
                let record = self.store.get(operation_id).await?;
                if record.as_ref().and_then(|record| record.error.as_deref())
                    == Some("invoice was already paid")
                {
                    return self.invoice_status(&invoice.invoice_id).await;
                }
            }
            if row.attempts >= 5 {
                return Err(anyhow::anyhow!(
                    "invoice payment was attempted too many times"
                ));
            }
        }

        let attempts = row.as_ref().map(|row| row.attempts).unwrap_or(0);
        let request_id = if attempts == 0 {
            invoice.invoice_id.clone()
        } else {
            format!("{}.{}", invoice.invoice_id, attempts + 1)
        };
        let next_attempts = if attempts == 0 { 1 } else { attempts + 1 };
        let request = Request::PayInvoice {
            invoice_id: invoice.invoice_id.clone(),
            payee: invoice.payee.clone(),
            token: invoice.token.clone(),
            amount: invoice.amount.clone(),
            description: invoice.description.clone(),
            expires_at: invoice.expires_at.clone(),
            signature: signed.signature.clone(),
        };
        let payment = self.execute(request, Some(request_id), dry_run).await?;
        if dry_run {
            return Ok(json!({
                "invoice": signed.invoice,
                "signature": signed.signature,
                "payment": payment,
            }));
        }
        let operation_id = payment["operation"]["id"].as_str().map(str::to_string);
        let operation_status = payment["operation"]["status"].as_str().unwrap_or("open");
        let payment_signature = payment["operation"]["signature"]
            .as_str()
            .map(str::to_string);
        let status = invoice_status_name(operation_status);
        self.store
            .update_invoice(
                &invoice.invoice_id,
                &status,
                payment_signature.as_deref(),
                operation_id.as_deref(),
                Some(next_attempts),
            )
            .await?;
        Ok(invoice_view(
            &signed,
            &status,
            payment,
            self.payee_is_trusted(&invoice.payee).await?,
        ))
    }

    /// Whether an invoice this wallet issued or accepted has been paid on-chain.
    pub async fn invoice_status(&self, invoice_id: &str) -> anyhow::Result<Value> {
        let row = self
            .store
            .invoice(invoice_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("unknown invoice {invoice_id}"))?;
        let signed = invoice::parse_signed(&serde_json::from_str(&row.document_json)?)?;
        let hint = match &row.operation_id {
            Some(operation_id) => self
                .store
                .get(operation_id)
                .await?
                .and_then(|record| record.signature)
                .or(row.payment_signature.clone()),
            None => row.payment_signature.clone(),
        };
        let trusted = self.payee_is_trusted(&signed.invoice.payee).await?;
        match self.locate_invoice(&signed, hint.as_deref()).await {
            Ok(Some(found)) => {
                let status = if found.finalized {
                    "paid"
                } else {
                    "confirming"
                };
                self.store
                    .update_invoice(invoice_id, status, Some(&found.signature), None, None)
                    .await?;
                Ok(invoice_view(
                    &signed,
                    status,
                    json!({
                        "signature": found.signature,
                        "slot": found.slot,
                        "finalized": found.finalized,
                    }),
                    trusted,
                ))
            }
            Ok(None) => {
                let (operation_status, payment) = if let Some(operation_id) = &row.operation_id {
                    let status = self.refresh(operation_id).await?;
                    (Some(status), self.result(operation_id).await?)
                } else {
                    (None, Value::Null)
                };
                let expired = invoice::is_expired(&signed.invoice.expires_at);
                let can_expire = operation_status
                    .as_deref()
                    .is_none_or(|status| matches!(status, "denied" | "failed" | "expired"))
                    && (row.payment_signature.is_none() || operation_status.is_some())
                    && row.status != "paid";
                let status = if row.status == "paid" {
                    "paid".to_string()
                } else if expired && can_expire {
                    "expired".to_string()
                } else if expired
                    && row.status != "paid"
                    && row.payment_signature.is_some()
                    && operation_status.is_none()
                {
                    "unknown".to_string()
                } else {
                    operation_status
                        .as_deref()
                        .map(invoice_status_name)
                        .unwrap_or_else(|| row.status.clone())
                };
                if status != row.status {
                    self.store
                        .update_invoice(invoice_id, &status, None, None, None)
                        .await?;
                }
                Ok(invoice_view(&signed, &status, payment, trusted))
            }
            Err(error) => {
                if let Some(operation_id) = &row.operation_id {
                    let status = match self.refresh(operation_id).await {
                        Ok(status) => status,
                        Err(_) => self
                            .store
                            .get(operation_id)
                            .await?
                            .map(|record| record.status)
                            .unwrap_or_default(),
                    };
                    if status == "finalized" {
                        return Ok(invoice_view(
                            &signed,
                            "paid",
                            self.result(operation_id).await?,
                            trusted,
                        ));
                    }
                    if matches!(
                        status.as_str(),
                        "submitting"
                            | "submitted"
                            | "submission_unknown"
                            | "processed"
                            | "confirmed"
                    ) {
                        return Ok(invoice_view(
                            &signed,
                            &invoice_status_name(&status),
                            self.result(operation_id).await?,
                            trusted,
                        ));
                    }
                }
                Err(error)
            }
        }
    }

    async fn locate_invoice(
        &self,
        signed: &SignedInvoice,
        hint: Option<&str>,
    ) -> anyhow::Result<Option<invoice::FoundPayment>> {
        let asset = tx::resolve_asset(&self.rpc, &signed.invoice.token, &self.cluster).await?;
        let units = parse_units(&signed.invoice.amount, asset.decimals)?;
        invoice::find_payment(
            &self.rpc,
            &invoice::ExpectedPayment {
                payee: Pubkey::from_str(&signed.invoice.payee)?,
                invoice_id: &signed.invoice.invoice_id,
                units,
                mint: asset.mint,
                token_program: asset.token_program,
            },
            hint,
        )
        .await
    }

    async fn payee_is_trusted(&self, payee: &str) -> anyhow::Result<bool> {
        let Ok(payee) = Pubkey::from_str(payee) else {
            return Ok(false);
        };
        Ok(invoice::recipient_is_trusted(
            &self.budget().await?.trusted_recipients,
            &payee,
        ))
    }

    /// Pay an HTTP 402 (x402 v2) challenge inside the wallet's budget and return a
    /// portable `manus.receipt.v1` plus a base64 `payment_header` proof. When `http`
    /// (`url`) is provided, Manus fetches the resource, handles the 402 challenge,
    /// and automatically retries the request with the signed payment header.
    pub async fn pay_x402(
        &self,
        challenge: Option<&Value>,
        payment_required_header: Option<&str>,
        http: Option<crate::x402::HttpCall>,
        request_id: Option<String>,
        dry_run: bool,
    ) -> anyhow::Result<Value> {
        let parsed = match (challenge, payment_required_header, http.as_ref()) {
            (None, None, Some(call)) => match crate::x402::probe_url(call).await? {
                crate::x402::ProbeOutcome::NoPaymentNeeded(response) => {
                    return Ok(json!({
                        "paid": false,
                        "http_response": response,
                    }));
                }
                crate::x402::ProbeOutcome::PaymentRequired(parsed) => parsed,
            },
            _ => crate::x402::decode_challenge(challenge, payment_required_header)?,
        };
        let resolved = crate::x402::resolve_challenge(&parsed, &self.cluster)?;
        if let Some(call) = http.as_ref() {
            let requested_host = crate::x402::parse_http_host(&call.url)?;
            if requested_host != resolved.domain {
                return Err(anyhow::anyhow!(
                    "x402 challenge resource host '{}' does not match requested URL host '{requested_host}'",
                    resolved.domain
                ));
            }
        }
        if resolved.payee == self.address() {
            return Err(anyhow::anyhow!(
                "refusing to pay x402 challenge to the wallet itself"
            ));
        }
        let asset = tx::resolve_asset(&self.rpc, &resolved.asset, &self.cluster).await?;
        let amount = format_units(resolved.amount_atomic, asset.decimals);
        let req_id = request_id.unwrap_or_else(|| crate::x402::default_request_id(&resolved));
        let memo = crate::x402::memo_for(&resolved.domain, &req_id);
        let request = Request::PayX402 {
            resource_url: resolved.resource_url.clone(),
            domain: resolved.domain.clone(),
            payee: resolved.payee.to_string(),
            token: resolved.asset.clone(),
            amount: amount.clone(),
            memo: memo.clone(),
            description: resolved.description.clone(),
        };
        let payment = self.execute(request, Some(req_id), dry_run).await?;
        let receipt = crate::x402::build_receipt(
            &resolved,
            &asset.symbol,
            asset.decimals,
            &self.cluster,
            &self.address(),
            &memo,
            &payment,
        );
        let op = &payment["operation"];
        let payment_header = match (op["signature"].as_str(), op["status"].as_str()) {
            (
                Some(sig),
                Some("submitted" | "processed" | "confirmed" | "finalized"),
            ) => Some(crate::x402::encode_payment_header(
                &resolved,
                &self.address(),
                sig,
                op["slot"].as_i64(),
                receipt["receipt_id"].as_str().unwrap_or_default(),
            )),
            _ => None,
        };
        let http_response = match (dry_run, http.as_ref(), payment_header.as_deref()) {
            (false, Some(call), Some(header)) => {
                Some(crate::x402::retry_with_payment_header(call, header).await?)
            }
            _ => None,
        };
        let trusted = self.payee_is_trusted(&resolved.payee.to_string()).await?;
        Ok(json!({
            "x402": {
                "resource_url": resolved.resource_url,
                "domain": resolved.domain,
                "description": resolved.description,
                "network": resolved.network,
                "payee": resolved.payee.to_string(),
                "asset": resolved.accepted.asset,
                "symbol": asset.symbol,
                "amount_atomic": resolved.amount_atomic.to_string(),
                "amount": amount,
                "memo": memo,
            },
            "payment": payment,
            "receipt": receipt,
            "payment_header": payment_header,
            "http_response": http_response,
            "counterparty_trusted": trusted,
        }))
    }

    /// Verify a signed invoice, `manus.receipt.v1`, operation receipt, or local `op_`/`inv_` ID.
    pub async fn verify(&self, target: &Value, offline: bool) -> anyhow::Result<Value> {
        let resolved_doc = match target.as_str() {
            Some(text) if text.starts_with("op_") => self.operation(text).await?,
            Some(text) if text.starts_with("inv_") => {
                let row = self
                    .store
                    .invoice(text)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("unknown invoice {text}"))?;
                serde_json::from_str(&row.document_json)?
            }
            Some(text) => serde_json::from_str(text).map_err(|error| {
                anyhow::anyhow!("verify target must be JSON, an op_ id, or an inv_ id: {error}")
            })?,
            None => target.clone(),
        };
        let rpc = if offline {
            None
        } else {
            Some((&self.rpc, self.cluster.as_str()))
        };
        crate::verify::verify_document(&resolved_doc, rpc).await
    }

    /// Fetch a live swap quote (Jupiter on mainnet), inspect both tokens' on-chain safety
    /// (mint/freeze authority, Token-2022 transfer fees/hooks), and evaluate budget & trading guards.
    /// If `kelly_fraction` (e.g. from AIPricePatterns MCP) is passed, also computes a bankroll-aware
    /// SOL position size bounded by the wallet's live SOL balance and remaining `convert_sol` budget.
    pub async fn quote(
        &self,
        from: &str,
        to: &str,
        amount: &str,
        slippage_bps: Option<u16>,
        kelly_fraction: Option<f64>,
    ) -> anyhow::Result<Value> {
        let input = tx::resolve_asset(&self.rpc, from, &self.cluster).await?;
        let output = tx::resolve_asset(&self.rpc, to, &self.cluster).await?;
        let input_safety = crate::trading::inspect_asset_safety(&self.rpc, &input).await?;
        let output_safety = crate::trading::inspect_asset_safety(&self.rpc, &output).await?;
        let budget = self.budget().await?;
        let spent = self.store.spent_last_day(None).await?;
        let remaining_daily_lamports = budget.convert_sol.daily.saturating_sub(spent.convert_sol);

        let kelly_sizing = if let Some(kf) = kelly_fraction {
            let sol_lamports = self.rpc.get_balance(&self.address()).await.unwrap_or(0);
            let recommended_lamports = crate::trading::compute_kelly_size_lamports(
                kf,
                sol_lamports,
                budget.convert_sol.per_op,
                remaining_daily_lamports,
            );
            Some(json!({
                "kelly_fraction": kf,
                "wallet_sol_balance": format_units(sol_lamports, 9),
                "remaining_daily_convert_sol": format_units(remaining_daily_lamports, 9),
                "max_per_op_convert_sol": format_units(budget.convert_sol.per_op, 9),
                "recommended_sol_amount": format_units(recommended_lamports, 9),
            }))
        } else {
            None
        };

        let units = if amount.trim().eq_ignore_ascii_case("all") {
            if input.mint.is_none() {
                return Err(anyhow::anyhow!(
                    "'all' is only supported for tokens, not SOL"
                ));
            }
            self.token_balance(&input).await?
        } else if amount.trim().eq_ignore_ascii_case("kelly") {
            if input.mint.is_some() {
                return Err(anyhow::anyhow!(
                    "'kelly' amount is only supported when swapping from SOL"
                ));
            }
            let kf = kelly_fraction.ok_or_else(|| {
                anyhow::anyhow!("amount 'kelly' requires passing kelly_fraction (0.0..1.0)")
            })?;
            let sol_lamports = self.rpc.get_balance(&self.address()).await?;
            crate::trading::compute_kelly_size_lamports(
                kf,
                sol_lamports,
                budget.convert_sol.per_op,
                remaining_daily_lamports,
            )
        } else {
            parse_units(amount, input.decimals)?
        };
        if units == 0 {
            return Err(anyhow::anyhow!("amount must be greater than zero"));
        }

        let slippage = slippage_bps.unwrap_or(50);

        if self.cluster != "mainnet-beta" {
            return Ok(json!({
                "cluster": self.cluster,
                "quote_available": false,
                "reason": "Jupiter routing is available on mainnet-beta only; token safety was inspected on the current cluster",
                "from": {
                    "symbol": input.symbol,
                    "mint": input.mint_or_wsol(),
                    "amount": format_units(units, input.decimals),
                    "amount_atomic": units.to_string(),
                    "safety": input_safety,
                },
                "to": {
                    "symbol": output.symbol,
                    "mint": output.mint_or_wsol(),
                    "safety": output_safety,
                },
                "kelly_sizing": kelly_sizing,
            }));
        }

        if input.mint_or_wsol() == output.mint_or_wsol() {
            return Err(anyhow::anyhow!("input and output tokens are the same"));
        }

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

        let input_value_lamports = if input.mint.is_none() {
            units
        } else if output.mint.is_none() {
            quote.out_amount
        } else {
            self.jupiter
                .quote(&input.mint_or_wsol(), SOL_MINT, units, 100, 0)
                .await
                .map(|q| q.out_amount)
                .unwrap_or(0)
        };

        let mut extra_reasons = Vec::new();
        if slippage > budget.max_slippage_bps {
            extra_reasons.push(format!(
                "slippage {slippage} bps exceeds the agent limit of {} bps",
                budget.max_slippage_bps
            ));
        }
        let impact_percent = quote.price_impact_pct * 100.0;
        if impact_percent > MAX_PRICE_IMPACT_PCT {
            extra_reasons.push(format!(
                "price impact {impact_percent:.2}% is above {MAX_PRICE_IMPACT_PCT}%"
            ));
        }
        if budget.require_allowed_tokens
            && !crate::trading::token_is_allowed(&budget.allowed_tokens, &output, &self.cluster)
        {
            extra_reasons.push(format!(
                "output token {} ({}) is not in allowed_tokens",
                output.symbol,
                output.mint_or_wsol()
            ));
        }
        extra_reasons.extend(output_safety.approval_reasons.clone());
        if budget.swap_cooldown_secs > 0 {
            if let Some(elapsed) = self.store.last_conversion_age_secs(None).await? {
                if elapsed < budget.swap_cooldown_secs {
                    extra_reasons.push(format!(
                        "swap cooldown active ({elapsed}s elapsed, minimum {}s)",
                        budget.swap_cooldown_secs
                    ));
                }
            }
        }
        if budget.max_daily_loss_sol > 0
            && !matches!(output.symbol.as_str(), "SOL" | "USDC" | "USDT")
        {
            let trades = self.store.committed_trades().await?;
            let realized_24h = crate::trading::realized_pnl_last_24h_lamports(&trades);
            if realized_24h < 0
                && (realized_24h.unsigned_abs() as u64) >= budget.max_daily_loss_sol
            {
                extra_reasons.push(format!(
                    "daily loss circuit breaker active (realized 24h PnL -{} SOL exceeds max_daily_loss_sol {} SOL)",
                    format_units(realized_24h.unsigned_abs() as u64, 9),
                    format_units(budget.max_daily_loss_sol, 9)
                ));
            }
        }

        let exposure = Exposure {
            category: Category::Convert,
            sol_out: input_value_lamports,
            fees: 0,
            token_out: None,
            recipient: None,
        };
        let decision = merge(decide(&budget, &spent, &exposure), &extra_reasons);

        let in_whole = (units as f64) / 10f64.powi(input.decimals as i32);
        let out_whole = (quote.out_amount as f64) / 10f64.powi(output.decimals as i32);
        let rate = if in_whole > 0.0 {
            out_whole / in_whole
        } else {
            0.0
        };

        Ok(json!({
            "cluster": self.cluster,
            "quote_available": true,
            "from": {
                "symbol": input.symbol,
                "mint": input.mint_or_wsol(),
                "amount": format_units(units, input.decimals),
                "amount_atomic": units.to_string(),
                "decimals": input.decimals,
                "safety": input_safety,
            },
            "to": {
                "symbol": output.symbol,
                "mint": output.mint_or_wsol(),
                "estimated_out": format_units(quote.out_amount, output.decimals),
                "estimated_out_atomic": quote.out_amount.to_string(),
                "min_out": format_units(quote.min_out_amount, output.decimals),
                "min_out_atomic": quote.min_out_amount.to_string(),
                "decimals": output.decimals,
                "safety": output_safety,
            },
            "rate": format!("{rate:.8}"),
            "slippage_bps": slippage,
            "price_impact_pct": (impact_percent * 1000.0).round() / 1000.0,
            "route": quote.route,
            "platform_fee": {
                "bps": quote.fee_bps,
                "amount": format_units(quote.platform_fee, output.decimals),
                "symbol": output.symbol,
            },
            "sol_valuation": format_units(input_value_lamports, 9),
            "decision": decision,
            "kelly_sizing": kelly_sizing,
        }))
    }

    /// Trading portfolio: on-chain balances, per-token cost basis, average entry price,
    /// realized/unrealized PnL in SOL, strategy performance by tag, and active triggers.
    pub async fn portfolio(&self) -> anyhow::Result<Value> {
        self.reconcile_unsettled().await?;
        let wallet = self.address();
        let sol_lamports = self.rpc.get_balance(&wallet).await?;

        let mut onchain_tokens: Vec<(String, String, u64, u8)> = Vec::new();
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
                    let atomic: u64 = amount["amount"]
                        .as_str()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0);
                    if atomic == 0 {
                        continue;
                    }
                    let decimals = amount["decimals"].as_u64().unwrap_or(0) as u8;
                    let symbol = tx::symbol_for_mint(&mint)
                        .map(str::to_string)
                        .unwrap_or_else(|| {
                            Pubkey::from_str(&mint)
                                .map(|pk| tx::short(&pk))
                                .unwrap_or_else(|_| mint.clone())
                        });
                    onchain_tokens.push((mint, symbol, atomic, decimals));
                }
            }
        }

        let mut valuations = std::collections::BTreeMap::new();
        if self.cluster == "mainnet-beta" {
            for (mint, _sym, atomic, _dec) in &onchain_tokens {
                if *atomic > 0 && mint != SOL_MINT {
                    if let Ok(q) = self.jupiter.quote(mint, SOL_MINT, *atomic, 100, 0).await {
                        valuations.insert(mint.clone(), q.out_amount);
                    }
                }
            }
        }

        let trades = self.store.committed_trades().await?;
        let (positions, realized_lamports, unrealized_lamports) =
            crate::trading::compute_positions(&trades, &onchain_tokens, &valuations);
        let performance = crate::trading::compute_strategy_performance(&trades);
        let active_triggers = self.store.list_triggers(true).await.unwrap_or_default();

        let tokens_sol_lamports: u64 = valuations.values().copied().sum();
        let total_portfolio_lamports = sol_lamports.saturating_add(tokens_sol_lamports);

        Ok(json!({
            "address": wallet.to_string(),
            "cluster": self.cluster,
            "sol_balance": format_units(sol_lamports, 9),
            "tokens_value_sol": if self.cluster == "mainnet-beta" {
                Value::String(format_units(tokens_sol_lamports, 9))
            } else {
                Value::Null
            },
            "total_portfolio_value_sol": if self.cluster == "mainnet-beta" {
                Value::String(format_units(total_portfolio_lamports, 9))
            } else {
                Value::String(format_units(sol_lamports, 9))
            },
            "realized_pnl_sol": crate::trading::format_signed_lamports(realized_lamports),
            "unrealized_pnl_sol": crate::trading::format_signed_lamports(unrealized_lamports),
            "total_trades": trades.len(),
            "performance": performance,
            "positions": positions,
            "active_triggers": active_triggers,
        }))
    }

    /// Execute a swap and optionally attach an automatic Take-Profit / Stop-Loss / Trailing-Stop
    /// exit trigger (`to -> from`) on the exact received output token amount.
    #[allow(clippy::too_many_arguments)]
    pub async fn swap_with_bracket(
        &self,
        from: &str,
        to: &str,
        amount: &str,
        slippage_bps: Option<u16>,
        take_profit_pct: Option<f64>,
        stop_loss_pct: Option<f64>,
        trailing_stop_bps: Option<u16>,
        tag: Option<String>,
        request_id: Option<String>,
        dry_run: bool,
    ) -> anyhow::Result<Value> {
        let has_bracket =
            take_profit_pct.is_some() || stop_loss_pct.is_some() || trailing_stop_bps.is_some();
        if let Some(tp) = take_profit_pct {
            if !tp.is_finite() || tp <= 0.0 {
                return Err(anyhow::anyhow!("take_profit_pct must be > 0"));
            }
        }
        if let Some(sl) = stop_loss_pct {
            let drop = sl.abs();
            if !drop.is_finite() || drop <= 0.0 || drop >= 100.0 {
                return Err(anyhow::anyhow!("stop_loss_pct must be between 0 and 100"));
            }
        }
        if let Some(bps) = trailing_stop_bps {
            if bps == 0 || bps >= 10_000 {
                return Err(anyhow::anyhow!(
                    "trailing_stop_bps must be between 1 and 9999"
                ));
            }
        }

        let clean_tag = tag
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let req = Request::Swap {
            from: from.to_string(),
            to: to.to_string(),
            amount: amount.to_string(),
            slippage_bps,
            tag: clean_tag.clone(),
        };
        let mut res = self.execute(req, request_id, dry_run).await?;

        if !has_bracket {
            return Ok(res);
        }

        if dry_run {
            res["bracket_trigger_preview"] = json!({
                "exit_pair": format!("{to} -> {from}"),
                "take_profit_pct": take_profit_pct,
                "stop_loss_pct": stop_loss_pct,
                "trailing_stop_bps": trailing_stop_bps,
                "tag": clean_tag,
            });
            return Ok(res);
        }

        let op_id = res["operation"]["id"].as_str().unwrap_or_default().to_string();
        let op_status = res["operation"]["status"].as_str().unwrap_or_default();
        if matches!(
            op_status,
            "submitted" | "processed" | "confirmed" | "finalized"
        ) {
            let trades = self.store.committed_trades().await?;
            if let Some(trade) = trades.iter().find(|t| t.operation_id == op_id) {
                let in_whole =
                    (trade.input_amount as f64) / 10f64.powi(trade.input_decimals as i32);
                let out_whole =
                    (trade.output_amount as f64) / 10f64.powi(trade.output_decimals as i32);
                if in_whole > 0.0 && out_whole > 0.0 {
                    // Exit direction is `output -> input`, so 1 output unit = (in_whole / out_whole) input units.
                    let exit_entry_rate = in_whole / out_whole;
                    let tp_rate = take_profit_pct.map(|pct| exit_entry_rate * (1.0 + pct / 100.0));
                    let sl_rate =
                        stop_loss_pct.map(|pct| exit_entry_rate * (1.0 - pct.abs() / 100.0));
                    let now = chrono::Utc::now().to_rfc3339();
                    let record = crate::store::TriggerRecord {
                        id: format!("trg_{}", uuid::Uuid::new_v4().simple()),
                        from_token: to.trim().to_string(),
                        to_token: from.trim().to_string(),
                        amount: format_units(trade.output_amount, trade.output_decimals),
                        slippage_bps: slippage_bps.unwrap_or(50),
                        take_profit_rate: tp_rate,
                        stop_loss_rate: sl_rate,
                        trailing_stop_bps,
                        peak_rate: Some(exit_entry_rate),
                        tag: clean_tag,
                        status: "active".to_string(),
                        triggered_reason: None,
                        operation_id: None,
                        created_at: now.clone(),
                        updated_at: now,
                    };
                    self.store.put_trigger(&record).await?;
                    res["bracket_trigger"] = serde_json::to_value(&record)?;
                }
            }
        }
        Ok(res)
    }

    /// Create a local Take-Profit, Stop-Loss, OCO, or Trailing-Stop trigger order.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_trigger(
        &self,
        from: &str,
        to: &str,
        amount: &str,
        slippage_bps: Option<u16>,
        take_profit_rate: Option<f64>,
        stop_loss_rate: Option<f64>,
        take_profit_pct: Option<f64>,
        stop_loss_pct: Option<f64>,
        trailing_stop_bps: Option<u16>,
        tag: Option<String>,
        dry_run: bool,
    ) -> anyhow::Result<Value> {
        let input = tx::resolve_asset(&self.rpc, from, &self.cluster).await?;
        let output = tx::resolve_asset(&self.rpc, to, &self.cluster).await?;
        if input.mint_or_wsol() == output.mint_or_wsol() {
            return Err(anyhow::anyhow!("input and output tokens are the same"));
        }
        if !amount.trim().eq_ignore_ascii_case("all") {
            let _ = parse_units(amount, input.decimals)?;
        }

        let mut tp = take_profit_rate;
        let mut sl = stop_loss_rate;
        let mut reference_rate = None;

        if (tp.is_none() && take_profit_pct.is_some())
            || (sl.is_none() && stop_loss_pct.is_some())
            || (trailing_stop_bps.is_some() && self.cluster == "mainnet-beta")
        {
            if let Ok(q) = self.quote(from, to, amount, slippage_bps, None).await {
                if let Some(current_rate) = q["rate"].as_str().and_then(|s| s.parse::<f64>().ok()) {
                    reference_rate = Some(current_rate);
                }
            }
            if (tp.is_none() && take_profit_pct.is_some())
                || (sl.is_none() && stop_loss_pct.is_some())
            {
                let current_rate = reference_rate.ok_or_else(|| {
                    anyhow::anyhow!(
                        "percentage trigger offsets require a live mainnet Jupiter quote; pass explicit take_profit_rate / stop_loss_rate on {}",
                        self.cluster
                    )
                })?;
                if tp.is_none() {
                    if let Some(pct) = take_profit_pct {
                        if pct <= 0.0 {
                            return Err(anyhow::anyhow!("take_profit_pct must be > 0"));
                        }
                        tp = Some(current_rate * (1.0 + pct / 100.0));
                    }
                }
                if sl.is_none() {
                    if let Some(pct) = stop_loss_pct {
                        let drop = pct.abs();
                        if drop <= 0.0 || drop >= 100.0 {
                            return Err(anyhow::anyhow!("stop_loss_pct must be between 0 and 100"));
                        }
                        sl = Some(current_rate * (1.0 - drop / 100.0));
                    }
                }
            }
        }

        crate::trading::validate_trigger_rates(tp, sl, trailing_stop_bps)?;

        let effective_slippage = slippage_bps.unwrap_or(50);
        let budget = self.budget().await?;
        let policy_reasons =
            preflight_trigger_policy(&budget, &output, &self.cluster, effective_slippage);
        let policy_ok = policy_reasons.is_empty();

        let now = chrono::Utc::now().to_rfc3339();
        let record = crate::store::TriggerRecord {
            id: if dry_run {
                "trg_dry_run_preview".to_string()
            } else {
                format!("trg_{}", uuid::Uuid::new_v4().simple())
            },
            from_token: from.trim().to_string(),
            to_token: to.trim().to_string(),
            amount: amount.trim().to_string(),
            slippage_bps: effective_slippage,
            take_profit_rate: tp,
            stop_loss_rate: sl,
            trailing_stop_bps,
            peak_rate: reference_rate,
            tag: tag
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            status: "active".to_string(),
            triggered_reason: None,
            operation_id: None,
            created_at: now.clone(),
            updated_at: now,
        };
        let pair = format!("{} -> {}", input.symbol, output.symbol);
        if dry_run {
            return Ok(json!({
                "dry_run": true,
                "trigger_preview": record,
                "reference_rate": reference_rate,
                "pair": pair,
                "policy_ok": policy_ok,
                "policy_reasons": policy_reasons,
            }));
        }
        self.store.put_trigger(&record).await?;
        Ok(json!({
            "trigger": record,
            "reference_rate": reference_rate,
            "pair": pair,
            "policy_ok": policy_ok,
            "policy_reasons": policy_reasons,
        }))
    }

    pub async fn list_triggers(&self, active_only: bool) -> anyhow::Result<Value> {
        let triggers = self.store.list_triggers(active_only).await?;
        Ok(json!({ "triggers": triggers }))
    }

    pub async fn cancel_trigger(&self, id: &str) -> anyhow::Result<Value> {
        let updated = self
            .store
            .cancel_trigger(id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("unknown trigger {id}"))?;
        Ok(json!({ "trigger": updated }))
    }

    /// Evaluate all active Take-Profit / Stop-Loss / Trailing-Stop triggers against live Jupiter quotes
    /// and automatically execute any triggered swap inside the wallet's budget.
    pub async fn evaluate_triggers(&self) -> anyhow::Result<Value> {
        let active = self.store.list_triggers(true).await?;
        if active.is_empty() || self.cluster != "mainnet-beta" {
            return Ok(json!({
                "checked": active.len(),
                "triggered": [],
            }));
        }

        let mut fired = Vec::new();
        for trg in active {
            let Ok(q) = self
                .quote(
                    &trg.from_token,
                    &trg.to_token,
                    &trg.amount,
                    Some(trg.slippage_bps),
                    None,
                )
                .await
            else {
                continue;
            };
            let Some(current_rate) = q["rate"].as_str().and_then(|s| s.parse::<f64>().ok()) else {
                continue;
            };
            let (updated_peak, reason) = crate::trading::evaluate_trigger_condition(
                trg.take_profit_rate,
                trg.stop_loss_rate,
                trg.trailing_stop_bps,
                trg.peak_rate,
                current_rate,
            );
            if let Some(new_peak) = updated_peak {
                let _ = self.store.update_trigger_peak(&trg.id, new_peak).await;
            }
            let Some(reason) = reason else {
                continue;
            };
            if !self.store.claim_trigger(&trg.id, &reason).await? {
                continue;
            }
            let swap_req = Request::Swap {
                from: trg.from_token.clone(),
                to: trg.to_token.clone(),
                amount: trg.amount.clone(),
                slippage_bps: Some(trg.slippage_bps),
                tag: trg.tag.clone(),
            };
            match self.execute(swap_req, Some(trg.id.clone()), false).await {
                Ok(res) => {
                    let op_id = res["operation"]["id"].as_str().map(str::to_string);
                    let op_status = res["operation"]["status"].as_str().unwrap_or("unknown");
                    let final_status = if matches!(op_status, "denied" | "failed" | "expired") {
                        "failed"
                    } else {
                        "executed"
                    };
                    let _ = self
                        .store
                        .finish_trigger(&trg.id, final_status, op_id.as_deref(), Some(&reason))
                        .await;
                    self.gbrain.emit(
                        "wallet.trigger",
                        format!("Manus trigger {} {final_status}: {reason}", trg.id),
                        2,
                    );
                    fired.push(json!({
                        "trigger_id": trg.id,
                        "status": final_status,
                        "reason": reason,
                        "operation": res["operation"],
                    }));
                }
                Err(err) => {
                    let err_msg = format!("{reason}; execution error: {err}");
                    let _ = self
                        .store
                        .finish_trigger(&trg.id, "failed", None, Some(&err_msg))
                        .await;
                    fired.push(json!({
                        "trigger_id": trg.id,
                        "status": "failed",
                        "error": err_msg,
                    }));
                }
            }
        }

        Ok(json!({
            "checked": fired.len(),
            "triggered": fired,
        }))
    }

    /// Inspect and simulate a prepared transaction, then measure its exposure.
    async fn evaluate(&self, prepared: &Prepared) -> anyhow::Result<(SimulatedEffects, Exposure)> {
        let wallet = self.address();
        tx::inspect_structure(&prepared.tx, &wallet)?;
        let watched: Vec<&Asset> = prepared.watched.iter().collect();
        let effects = tx::simulate(&self.rpc, &prepared.tx, &wallet, &watched).await?;
        let exposure = measure(&prepared.shape, &effects, tx::max_network_fee(&prepared.tx))?;
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
        self.reconcile_unsettled().await?;
        let budget = self.budget().await?;
        let spent = self.store.spent_last_day(None).await?;
        let balances = self.balances().await?;
        Ok(json!({
            "address": self.address().to_string(),
            "cluster": self.cluster,
            "balances": balances,
            "budget": self.budget_view(&budget).await,
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
                "Transfers are free. Swaps, stake and unstake carry a 0.1% Manus fee, shown in the summary.",
                "create_invoice bills another agent. pay_invoice settles a signed bill. The first payment to a new agent asks you, then that address is trusted inside the budget.",
                "pay_x402 inspects and settles an HTTP 402 (x402 v2) challenge and returns a manus.receipt.v1 plus a payment_header proof. verify checks any invoice or receipt on-chain.",
                "For trading: call market_signals (AIPricePatterns), quote (price impact + token mint/freeze safety), and portfolio (positions + PnL) before executing swap."
            ]
        }))
    }

    pub async fn history(&self, limit: i64) -> anyhow::Result<Value> {
        self.reconcile_unsettled().await?;
        let records = self.store.recent(limit).await?;
        Ok(json!({ "operations": records.iter().map(view).collect::<Vec<_>>() }))
    }

    async fn reconcile_unsettled(&self) -> anyhow::Result<()> {
        for pending in self.store.unsettled().await? {
            // A failed RPC lookup must keep the durable reservation intact.
            let _ = self.refresh(&pending.id).await;
            self.store.mark_reconciled(&pending.id).await?;
        }
        Ok(())
    }

    /// Change the budget. Tightening applies at once; widening needs human approval.
    pub async fn change_budget(&self, proposed: Budget, why: &str) -> anyhow::Result<Value> {
        let current = self.budget().await?;
        if current == proposed {
            return Ok(json!({ "changed": false, "budget": self.budget_view(&current).await }));
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
            let outcome = {
                let _prompt = self.approval_lock.lock().await;
                let approver = self.approver.clone();
                tokio::task::spawn_blocking(move || approver.request(&reason)).await?
            };
            match outcome {
                ApprovalOutcome::Approved { method } => approved_by = method,
                ApprovalOutcome::Denied { reason } => {
                    return Ok(json!({ "changed": false, "denied": reason }));
                }
            }
        }
        {
            let _guard = self.spend_lock.lock().await;
            let _file_guard = self.store.lock_spending().await?;
            // The approval covered a change from `current`; refuse if that moved meanwhile.
            if self.budget().await? != current {
                return Err(anyhow::anyhow!(
                    "the budget changed while waiting for approval; ask again"
                ));
            }
            self.store.save_budget(&proposed).await?;
        }
        self.gbrain
            .emit("wallet.budget", format!("Manus budget changed: {why}"), 2);
        Ok(json!({
            "changed": true,
            "approved_by": approved_by,
            "budget": self.budget_view(&proposed).await
        }))
    }

    /// Human-unit view of a budget, using each token's on-chain decimals.
    pub async fn budget_view(&self, budget: &Budget) -> Value {
        let mut decimals = std::collections::BTreeMap::new();
        for mint in budget.send_tokens.keys() {
            if let Ok(asset) = tx::resolve_asset(&self.rpc, mint, &self.cluster).await {
                decimals.insert(mint.clone(), asset.decimals);
            }
        }
        budget_view(budget, &decimals)
    }
}

/// Derive what an operation really does from simulation, and check it matches the request.
fn measure(
    shape: &Shape,
    effects: &SimulatedEffects,
    network_fee_cap: u64,
) -> anyhow::Result<Exposure> {
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
                // The wallet pays the network fee out of the SOL it receives.
                None => effects
                    .sol_after
                    .saturating_sub(effects.sol_before)
                    .saturating_add(network_fee_cap),
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

fn invoice_view(signed: &SignedInvoice, status: &str, payment: Value, trusted: bool) -> Value {
    json!({
        "invoice": signed.invoice,
        "signature": signed.signature,
        "status": status,
        "payment": payment,
        "counterparty_trusted": trusted,
    })
}

fn invoice_status_name(operation_status: &str) -> String {
    match operation_status {
        "finalized" => "paid".into(),
        "awaiting_approval" => "awaiting_approval".into(),
        "denied" => "denied".into(),
        "submission_unknown" => "unknown".into(),
        "confirmed" | "processed" | "submitted" | "submitting" => "confirming".into(),
        _ => "open".into(),
    }
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

/// Human-unit view of a budget. Token limits whose decimals are unknown are shown in base units.
pub fn budget_view(
    budget: &Budget,
    token_decimals: &std::collections::BTreeMap<String, u8>,
) -> Value {
    let limits = |l: &crate::budget::Limits, decimals: u8| json!({ "per_op": format_units(l.per_op, decimals), "daily": format_units(l.daily, decimals) });
    json!({
        "send_sol": limits(&budget.send_sol, 9),
        "convert_sol": limits(&budget.convert_sol, 9),
        "send_tokens": budget.send_tokens.iter().map(|(mint, l)| {
            match token_decimals.get(mint) {
                Some(decimals) => json!({
                    "mint": mint, "symbol": tx::symbol_for_mint(mint), "limits": limits(l, *decimals)
                }),
                None => json!({
                    "mint": mint, "symbol": tx::symbol_for_mint(mint),
                    "limits_base_units": { "per_op": l.per_op, "daily": l.daily }
                }),
            }
        }).collect::<Vec<_>>(),
        "trusted_recipients": budget.trusted_recipients,
        "require_trusted_recipients": budget.require_trusted_recipients,
        "max_slippage_bps": budget.max_slippage_bps,
        "allowed_tokens": budget.allowed_tokens,
        "require_allowed_tokens": budget.require_allowed_tokens,
        "swap_cooldown_secs": budget.swap_cooldown_secs,
        "max_daily_loss_sol": format_units(budget.max_daily_loss_sol, 9),
    })
}

/// Preflight-check trigger execution parameters against the wallet's swap policy.
fn preflight_trigger_policy(
    budget: &Budget,
    output: &Asset,
    cluster: &str,
    slippage_bps: u16,
) -> Vec<String> {
    let mut reasons = Vec::new();
    if slippage_bps > budget.max_slippage_bps {
        reasons.push(format!(
            "slippage {slippage_bps} bps exceeds the agent limit of {} bps",
            budget.max_slippage_bps
        ));
    }
    if budget.require_allowed_tokens
        && !crate::trading::token_is_allowed(&budget.allowed_tokens, output, cluster)
    {
        reasons.push(format!(
            "output token {} ({}) is not in allowed_tokens",
            output.symbol,
            output.mint_or_wsol()
        ));
    }
    reasons
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
        let exposure = measure(&shape, &effects(10_000, 3_995, vec![]), 5_000).unwrap();
        assert_eq!(exposure.sol_out, 1_000);
        assert_eq!(exposure.fees, 5_005);
        assert_eq!(exposure.recipient, Some(to.to_string()));
        // A transaction draining far more than requested is rejected outright.
        assert!(measure(&shape, &effects(10_000_000, 0, vec![]), 5_000).is_err());
    }

    #[test]
    fn token_send_must_move_exactly_the_requested_amount() {
        let shape = Shape::SendToken {
            to: Pubkey::new_unique(),
            mint: "Mint".into(),
            amount: 50,
        };
        let exposure = measure(
            &shape,
            &effects(10_000, 5_000, vec![("Mint", 100, 50)]),
            5_000,
        )
        .unwrap();
        assert_eq!(exposure.token_out, Some(("Mint".into(), 50)));
        assert_eq!((exposure.sol_out, exposure.fees), (0, 5_000));
        assert!(measure(
            &shape,
            &effects(10_000, 5_000, vec![("Mint", 100, 0)]),
            5_000
        )
        .is_err());
    }

    #[test]
    fn sol_output_must_reach_the_quoted_minimum_after_fees() {
        let jito = Pubkey::new_unique();
        let input = Asset {
            symbol: "JitoSOL".into(),
            mint: Some(jito),
            decimals: 9,
            token_program: Some(spl_token::id()),
        };
        let quote = Quote {
            raw: json!({}),
            in_amount: 1_000,
            out_amount: 810_000,
            min_out_amount: 800_000,
            price_impact_pct: 0.0,
            route: String::new(),
            platform_fee: 0,
            fee_bps: 0,
        };
        let shape = Shape::Convert {
            input,
            output: Asset::sol(),
            quote: Box::new(quote),
            input_value_lamports: 810_000,
        };
        let jito = jito.to_string();
        let exact = effects(1_000_000, 1_795_000, vec![(jito.as_str(), 1_000, 0)]);
        let exposure = measure(&shape, &exact, 5_000).unwrap();
        assert_eq!(exposure.category, Category::Convert);
        let short = effects(1_000_000, 1_794_999, vec![(jito.as_str(), 1_000, 0)]);
        assert!(measure(&shape, &short, 5_000).is_err());
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

    fn mock_mint_rpc(decimals: u8) -> RpcClient {
        use base64::Engine as _;
        let mut mint_data = vec![0u8; 82];
        mint_data[44] = decimals;
        mint_data[45] = 1; // is_initialized = true
        let encoded = base64::engine::general_purpose::STANDARD.encode(&mint_data);
        let account_json = json!({
            "context": { "slot": 1 },
            "value": {
                "data": [encoded, "base64"],
                "executable": false,
                "lamports": 1_461_600,
                "owner": spl_token::id().to_string(),
                "rentEpoch": 0,
                "space": 82
            }
        });
        RpcClient::new_mock_with_mocks(
            "http://mock.local".into(),
            std::collections::HashMap::from([(
                solana_client::rpc_request::RpcRequest::GetAccountInfo,
                account_json,
            )]),
        )
    }

    async fn test_wallet(rpc: RpcClient) -> (AgentWallet, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "manus-agent-engine-test-{}.db",
            uuid::Uuid::new_v4().simple()
        ));
        let store = Store::open(&path).await.unwrap();
        let wallet = AgentWallet::new(
            rpc,
            Keypair::new(),
            "devnet".to_string(),
            store,
            Arc::new(crate::approval::NoApprover),
            Gbrain::disabled(),
        );
        (wallet, path)
    }

    #[tokio::test]
    async fn create_trigger_dry_run_previews_and_persists_nothing() {
        let (wallet, path) = test_wallet(mock_mint_rpc(6)).await;
        let res = wallet
            .create_trigger(
                "SOL",
                "USDC",
                "0.1",
                Some(50),
                Some(150.0),
                Some(90.0),
                None,
                None,
                None,
                Some("TEST".into()),
                true,
            )
            .await
            .unwrap();

        assert_eq!(res["dry_run"], true);
        assert_eq!(res["policy_ok"], true);
        assert_eq!(res["policy_reasons"], json!([]));
        assert_eq!(res["trigger_preview"]["from_token"], "SOL");
        assert_eq!(res["trigger_preview"]["to_token"], "USDC");
        assert_eq!(res["trigger_preview"]["slippage_bps"], 50);
        assert_eq!(res["trigger_preview"]["take_profit_rate"], 150.0);
        assert_eq!(res["trigger_preview"]["stop_loss_rate"], 90.0);
        assert_eq!(res["trigger_preview"]["tag"], "TEST");
        assert!(
            wallet.store.list_triggers(false).await.unwrap().is_empty(),
            "dry_run must not persist triggers to SQLite"
        );
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn create_trigger_dry_run_flags_policy_violations() {
        let (wallet, path) = test_wallet(mock_mint_rpc(6)).await;
        let mut budget = wallet.budget().await.unwrap();
        budget.max_slippage_bps = 50;
        budget.require_allowed_tokens = true;
        budget.allowed_tokens = vec!["SOL".to_string()];
        wallet.store.save_budget(&budget).await.unwrap();

        let random_mint = Pubkey::new_unique().to_string();
        let res = wallet
            .create_trigger(
                "SOL",
                &random_mint,
                "0.1",
                Some(250),
                Some(150.0),
                Some(90.0),
                None,
                None,
                None,
                None,
                true,
            )
            .await
            .unwrap();

        assert_eq!(res["dry_run"], true);
        assert_eq!(res["policy_ok"], false);
        let reasons = res["policy_reasons"].as_array().unwrap();
        assert_eq!(reasons.len(), 2);
        assert!(reasons[0].as_str().unwrap().contains("slippage 250 bps"));
        assert!(reasons[1].as_str().unwrap().contains("allowed_tokens"));
        assert!(
            wallet.store.list_triggers(false).await.unwrap().is_empty(),
            "dry_run must not persist triggers to SQLite"
        );
        let _ = std::fs::remove_file(path);
    }
}
