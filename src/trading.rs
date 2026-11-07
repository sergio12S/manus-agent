//! Crypto trading execution guards, SPL Token / Token-2022 mint safety inspection,
//! OCO / Trailing-Stop trigger evaluation, and portfolio PnL & strategy analytics.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{commitment_config::CommitmentConfig, pubkey::Pubkey};
use spl_token_2022::extension::{
    default_account_state::DefaultAccountState, non_transferable::NonTransferable,
    permanent_delegate::PermanentDelegate, transfer_fee::TransferFeeConfig,
    transfer_hook::TransferHook, BaseStateWithExtensions, StateWithExtensions,
};
use std::collections::BTreeMap;

use crate::budget::{format_units, DEVNET_USDC_MINT, LAMPORTS_PER_SOL};
use crate::mints::{JITO_SOL_MINT, M_SOL_MINT, SOL_MINT, USDC_MINT};
use crate::store::TradeRecord;
use crate::tx::Asset;

const USDT_MINT: &str = "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB";

/// On-chain safety profile of an SPL Token or Token-2022 mint.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TokenSafety {
    pub symbol: String,
    pub mint: Option<String>,
    pub decimals: u8,
    pub token_program: String,
    pub is_canonical: bool,
    pub supply: Option<String>,
    pub mint_authority: Option<String>,
    pub freeze_authority: Option<String>,
    pub transfer_fee_bps: u16,
    pub max_transfer_fee: u64,
    pub permanent_delegate: Option<String>,
    pub transfer_hook_program: Option<String>,
    pub non_transferable: bool,
    pub default_account_frozen: bool,
    /// `"safe"`, `"caution"`, or `"high_risk"`.
    pub risk_level: String,
    pub warnings: Vec<String>,
    /// Reasons that must trigger human Touch ID approval when buying this token.
    pub approval_reasons: Vec<String>,
}

impl TokenSafety {
    pub fn native_sol() -> Self {
        Self {
            symbol: "SOL".into(),
            mint: None,
            decimals: 9,
            token_program: "native".into(),
            is_canonical: true,
            supply: None,
            mint_authority: None,
            freeze_authority: None,
            transfer_fee_bps: 0,
            max_transfer_fee: 0,
            permanent_delegate: None,
            transfer_hook_program: None,
            non_transferable: false,
            default_account_frozen: false,
            risk_level: "safe".into(),
            warnings: Vec::new(),
            approval_reasons: Vec::new(),
        }
    }
}

pub fn is_canonical_mint(mint: &str) -> bool {
    matches!(
        mint,
        SOL_MINT | USDC_MINT | DEVNET_USDC_MINT | USDT_MINT | JITO_SOL_MINT | M_SOL_MINT
    )
}

/// Whether `asset` is matched by an entry in `Budget.allowed_tokens` (by symbol or mint).
pub fn token_is_allowed(allowed_tokens: &[String], asset: &Asset, cluster: &str) -> bool {
    let asset_mint = asset.mint_or_wsol();
    for entry in allowed_tokens {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.eq_ignore_ascii_case("SOL") && asset.mint.is_none() {
            return true;
        }
        if trimmed.eq_ignore_ascii_case(&asset.symbol) {
            return true;
        }
        if trimmed == asset_mint {
            return true;
        }
        if let Some(known) = canonical_mint_for_symbol(trimmed, cluster) {
            if known == asset_mint {
                return true;
            }
        }
    }
    false
}

pub fn canonical_mint_for_symbol(symbol: &str, cluster: &str) -> Option<&'static str> {
    let mainnet = cluster == "mainnet-beta";
    match symbol.trim().to_ascii_uppercase().as_str() {
        "SOL" => Some(SOL_MINT),
        "USDC" if mainnet => Some(USDC_MINT),
        "USDC" => Some(DEVNET_USDC_MINT),
        "USDT" if mainnet => Some(USDT_MINT),
        "JITOSOL" if mainnet => Some(JITO_SOL_MINT),
        "MSOL" if mainnet => Some(M_SOL_MINT),
        _ => None,
    }
}

/// Inspect an asset's on-chain mint account for rugpull/honeypot authorities and Token-2022 extensions.
pub async fn inspect_asset_safety(rpc: &RpcClient, asset: &Asset) -> anyhow::Result<TokenSafety> {
    let Some(mint) = asset.mint else {
        return Ok(TokenSafety::native_sol());
    };
    let account = rpc
        .get_account_with_commitment(&mint, CommitmentConfig::confirmed())
        .await?
        .value
        .ok_or_else(|| anyhow::anyhow!("mint {mint} does not exist on-chain"))?;
    parse_mint_safety(&mint, &asset.symbol, &account.owner, &account.data)
}

pub fn parse_mint_safety(
    mint: &Pubkey,
    symbol: &str,
    owner: &Pubkey,
    data: &[u8],
) -> anyhow::Result<TokenSafety> {
    if *owner != spl_token::id() && *owner != spl_token_2022::id() {
        return Err(anyhow::anyhow!("{mint} is not owned by an SPL token program"));
    }
    let state = StateWithExtensions::<spl_token_2022::state::Mint>::unpack(data)
        .map_err(|e| anyhow::anyhow!("failed to unpack mint {mint}: {e}"))?;
    let base = state.base;
    let mint_str = mint.to_string();
    let is_canonical = is_canonical_mint(&mint_str);
    let token_program = if *owner == spl_token_2022::id() {
        "token-2022".to_string()
    } else {
        "spl-token".to_string()
    };

    let mint_authority = Option::<Pubkey>::from(base.mint_authority).map(|p| p.to_string());
    let freeze_authority = Option::<Pubkey>::from(base.freeze_authority).map(|p| p.to_string());
    let supply = Some(format_units(base.supply, base.decimals));

    let mut transfer_fee_bps = 0u16;
    let mut max_transfer_fee = 0u64;
    let mut permanent_delegate = None;
    let mut transfer_hook_program = None;
    let mut non_transferable = false;
    let mut default_account_frozen = false;

    if *owner == spl_token_2022::id() {
        if let Ok(fee_cfg) = state.get_extension::<TransferFeeConfig>() {
            let newer = &fee_cfg.newer_transfer_fee;
            transfer_fee_bps = u16::from(newer.transfer_fee_basis_points);
            max_transfer_fee = u64::from(newer.maximum_fee);
        }
        if let Ok(delegate) = state.get_extension::<PermanentDelegate>() {
            if let Some(pk) = Option::<Pubkey>::from(delegate.delegate) {
                permanent_delegate = Some(pk.to_string());
            }
        }
        if let Ok(hook) = state.get_extension::<TransferHook>() {
            if let Some(pk) = Option::<Pubkey>::from(hook.program_id) {
                transfer_hook_program = Some(pk.to_string());
            }
        }
        if state.get_extension::<NonTransferable>().is_ok() {
            non_transferable = true;
        }
        if let Ok(default_state) = state.get_extension::<DefaultAccountState>() {
            // AccountState::Frozen == 2 in SPL Token-2022
            if default_state.state == 2 {
                default_account_frozen = true;
            }
        }
    }

    let mut warnings = Vec::new();
    let mut approval_reasons = Vec::new();
    let mut high_risk = false;

    if non_transferable {
        let msg = format!("token {symbol} ({mint_str}) is non-transferable (soulbound)");
        warnings.push(msg.clone());
        approval_reasons.push(msg);
        high_risk = true;
    }
    if default_account_frozen {
        let msg = format!("token {symbol} ({mint_str}) initializes new token accounts as frozen");
        warnings.push(msg.clone());
        approval_reasons.push(msg);
        high_risk = true;
    }
    if let Some(delegate) = &permanent_delegate {
        let msg = format!(
            "token {symbol} ({mint_str}) has a Token-2022 permanent delegate ({delegate}) that can seize tokens"
        );
        warnings.push(msg.clone());
        approval_reasons.push(msg);
        high_risk = true;
    }
    if let Some(freeze) = &freeze_authority {
        if !is_canonical {
            let msg = format!(
                "token {symbol} ({mint_str}) has an active freeze_authority ({freeze})"
            );
            warnings.push(msg.clone());
            approval_reasons.push(msg);
            high_risk = true;
        }
    }
    if transfer_fee_bps > 0 {
        let msg = format!(
            "token {symbol} ({mint_str}) charges a Token-2022 transfer fee of {transfer_fee_bps} bps"
        );
        warnings.push(msg.clone());
        approval_reasons.push(msg);
        if transfer_fee_bps >= 500 {
            high_risk = true;
        }
    }
    if let Some(hook) = &transfer_hook_program {
        let msg = format!(
            "token {symbol} ({mint_str}) executes a custom Token-2022 transfer hook ({hook})"
        );
        warnings.push(msg.clone());
        approval_reasons.push(msg);
    }
    if let Some(mint_auth) = &mint_authority {
        if !is_canonical {
            warnings.push(format!(
                "token {symbol} ({mint_str}) has an active mint_authority ({mint_auth}); supply is not fixed"
            ));
        }
    }

    let risk_level = if high_risk {
        "high_risk"
    } else if !warnings.is_empty() {
        "caution"
    } else {
        "safe"
    };

    Ok(TokenSafety {
        symbol: symbol.to_string(),
        mint: Some(mint_str),
        decimals: base.decimals,
        token_program,
        is_canonical,
        supply,
        mint_authority,
        freeze_authority,
        transfer_fee_bps,
        max_transfer_fee,
        permanent_delegate,
        transfer_hook_program,
        non_transferable,
        default_account_frozen,
        risk_level: risk_level.to_string(),
        warnings,
        approval_reasons,
    })
}

// ------------------------------------------------ Portfolio & PnL Tracking

#[derive(Debug, Clone, Default)]
struct PositionAccumulator {
    symbol: String,
    decimals: u8,
    units_acquired: u128,
    cost_lamports: u128,
    realized_pnl_lamports: i128,
    trade_count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PositionView {
    pub mint: String,
    pub symbol: String,
    pub decimals: u8,
    pub balance: String,
    pub balance_atomic: String,
    pub tracked_cost_basis_sol: String,
    pub avg_entry_price_sol: Option<String>,
    pub current_value_sol: Option<String>,
    pub unrealized_pnl_sol: Option<String>,
    pub unrealized_pnl_pct: Option<f64>,
    pub realized_pnl_sol: String,
    pub trade_count: u64,
}

/// Compute per-token weighted-average cost basis, realized PnL, and unrealized PnL from
/// committed trade history and current on-chain token balances.
pub fn compute_positions(
    trades: &[TradeRecord],
    onchain_balances: &[(String, String, u64, u8)], // (mint, symbol, atomic_balance, decimals)
    current_values_lamports: &BTreeMap<String, u64>, // mint -> current value of onchain_balance in lamports
) -> (Vec<PositionView>, i128, i128) {
    let mut map: BTreeMap<String, PositionAccumulator> = BTreeMap::new();

    for trade in trades {
        let sol_val = trade.sol_value_lamports as u128;

        // If input is a non-SOL token, reduce its cost basis and book realized PnL in SOL terms.
        if trade.input_mint != SOL_MINT && trade.input_amount > 0 {
            let entry = map
                .entry(trade.input_mint.clone())
                .or_insert_with(|| PositionAccumulator {
                    symbol: trade.input_symbol.clone(),
                    decimals: trade.input_decimals,
                    ..Default::default()
                });
            entry.trade_count += 1;
            let sold = (trade.input_amount as u128).min(entry.units_acquired);
            if sold > 0 && entry.units_acquired > 0 {
                let cost_removed = (entry.cost_lamports * sold) / entry.units_acquired;
                entry.units_acquired = entry.units_acquired.saturating_sub(sold);
                entry.cost_lamports = entry.cost_lamports.saturating_sub(cost_removed);
                entry.realized_pnl_lamports += (sol_val as i128) - (cost_removed as i128);
            }
        }

        // If output is a non-SOL token, add to its cost basis.
        if trade.output_mint != SOL_MINT && trade.output_amount > 0 {
            let entry = map
                .entry(trade.output_mint.clone())
                .or_insert_with(|| PositionAccumulator {
                    symbol: trade.output_symbol.clone(),
                    decimals: trade.output_decimals,
                    ..Default::default()
                });
            entry.symbol = trade.output_symbol.clone();
            entry.decimals = trade.output_decimals;
            entry.units_acquired = entry
                .units_acquired
                .saturating_add(trade.output_amount as u128);
            entry.cost_lamports = entry.cost_lamports.saturating_add(sol_val);
            entry.trade_count += 1;
        }
    }

    // Ensure every currently held non-zero token appears in the positions list.
    for (mint, symbol, _balance, decimals) in onchain_balances {
        if mint == SOL_MINT {
            continue;
        }
        map.entry(mint.clone())
            .or_insert_with(|| PositionAccumulator {
                symbol: symbol.clone(),
                decimals: *decimals,
                ..Default::default()
            });
    }

    let balance_lookup: BTreeMap<String, (u64, u8, String)> = onchain_balances
        .iter()
        .map(|(mint, sym, bal, dec)| (mint.clone(), (*bal, *dec, sym.clone())))
        .collect();

    let mut positions = Vec::new();
    let mut total_realized_lamports: i128 = 0;
    let mut total_unrealized_lamports: i128 = 0;

    for (mint, acc) in map {
        let (onchain_units, decimals, symbol) = balance_lookup
            .get(&mint)
            .cloned()
            .unwrap_or((0, acc.decimals, acc.symbol.clone()));

        if onchain_units == 0 && acc.trade_count == 0 {
            continue;
        }

        // Scale cost basis to currently held units if wallet balance is smaller than tracked units
        // (e.g., if tokens were sent out), or keep tracked cost basis for the held portion.
        let effective_cost_lamports: u64 = if onchain_units == 0 || acc.units_acquired == 0 {
            0
        } else {
            let held = (onchain_units as u128).min(acc.units_acquired);
            ((acc.cost_lamports * held) / acc.units_acquired).min(u64::MAX as u128) as u64
        };

        let avg_entry_price_sol = if acc.units_acquired > 0 && acc.cost_lamports > 0 {
            let scale = 10f64.powi(decimals as i32);
            let whole_tokens = (acc.units_acquired as f64) / scale;
            let cost_sol = (acc.cost_lamports as f64) / (LAMPORTS_PER_SOL as f64);
            if whole_tokens > 0.0 {
                Some(format!("{:.8}", cost_sol / whole_tokens))
            } else {
                None
            }
        } else {
            None
        };

        let current_val = current_values_lamports.get(&mint).copied();
        let (unrealized_pnl_sol, unrealized_pnl_pct) = match current_val {
            Some(cur_lamports) if effective_cost_lamports > 0 => {
                let diff = (cur_lamports as i128) - (effective_cost_lamports as i128);
                total_unrealized_lamports += diff;
                let pct = ((diff as f64) / (effective_cost_lamports as f64)) * 100.0;
                (
                    Some(format_signed_lamports(diff)),
                    Some((pct * 100.0).round() / 100.0),
                )
            }
            _ => (None, None),
        };

        total_realized_lamports += acc.realized_pnl_lamports;

        positions.push(PositionView {
            mint,
            symbol,
            decimals,
            balance: format_units(onchain_units, decimals),
            balance_atomic: onchain_units.to_string(),
            tracked_cost_basis_sol: format_units(effective_cost_lamports, 9),
            avg_entry_price_sol,
            current_value_sol: current_val.map(|v| format_units(v, 9)),
            unrealized_pnl_sol,
            unrealized_pnl_pct,
            realized_pnl_sol: format_signed_lamports(acc.realized_pnl_lamports),
            trade_count: acc.trade_count,
        });
    }

    (
        positions,
        total_realized_lamports,
        total_unrealized_lamports,
    )
}

pub fn format_signed_lamports(lamports: i128) -> String {
    if lamports == 0 {
        return "0".to_string();
    }
    let abs = lamports.unsigned_abs().min(u64::MAX as u128) as u64;
    let formatted = format_units(abs, 9);
    if lamports < 0 {
        format!("-{formatted}")
    } else {
        format!("+{formatted}")
    }
}

/// Compute net realized PnL (in SOL lamports) for trades closed in the trailing 24 hours,
/// maintaining historical cost basis from all earlier trades.
pub fn realized_pnl_last_24h_lamports(trades: &[TradeRecord]) -> i128 {
    let cutoff = (chrono::Utc::now() - chrono::Duration::hours(24)).to_rfc3339();
    let mut map: BTreeMap<String, PositionAccumulator> = BTreeMap::new();
    let mut realized_24h: i128 = 0;

    for trade in trades {
        let sol_val = trade.sol_value_lamports as u128;

        if trade.input_mint != SOL_MINT && trade.input_amount > 0 {
            let entry = map.entry(trade.input_mint.clone()).or_default();
            let sold = (trade.input_amount as u128).min(entry.units_acquired);
            if sold > 0 && entry.units_acquired > 0 {
                let cost_removed = (entry.cost_lamports * sold) / entry.units_acquired;
                entry.units_acquired = entry.units_acquired.saturating_sub(sold);
                entry.cost_lamports = entry.cost_lamports.saturating_sub(cost_removed);
                let pnl = (sol_val as i128) - (cost_removed as i128);
                if trade.created_at >= cutoff {
                    realized_24h += pnl;
                }
            }
        }

        if trade.output_mint != SOL_MINT && trade.output_amount > 0 {
            let entry = map.entry(trade.output_mint.clone()).or_default();
            entry.units_acquired = entry
                .units_acquired
                .saturating_add(trade.output_amount as u128);
            entry.cost_lamports = entry.cost_lamports.saturating_add(sol_val);
        }
    }

    realized_24h
}

/// Validate Take-Profit, Stop-Loss, and Trailing-Stop trigger parameters.
pub fn validate_trigger_rates(
    take_profit_rate: Option<f64>,
    stop_loss_rate: Option<f64>,
    trailing_stop_bps: Option<u16>,
) -> anyhow::Result<()> {
    if take_profit_rate.is_none() && stop_loss_rate.is_none() && trailing_stop_bps.is_none() {
        return Err(anyhow::anyhow!(
            "trigger requires at least one of take_profit_rate (or take_profit_pct), stop_loss_rate (or stop_loss_pct), or trailing_stop_bps"
        ));
    }
    if let Some(tp) = take_profit_rate {
        if !tp.is_finite() || tp <= 0.0 {
            return Err(anyhow::anyhow!(
                "take_profit_rate must be a positive number"
            ));
        }
    }
    if let Some(sl) = stop_loss_rate {
        if !sl.is_finite() || sl <= 0.0 {
            return Err(anyhow::anyhow!("stop_loss_rate must be a positive number"));
        }
    }
    if let (Some(tp), Some(sl)) = (take_profit_rate, stop_loss_rate) {
        if tp <= sl {
            return Err(anyhow::anyhow!(
                "take_profit_rate ({tp}) must be strictly greater than stop_loss_rate ({sl})"
            ));
        }
    }
    if let Some(bps) = trailing_stop_bps {
        if bps == 0 || bps >= 10_000 {
            return Err(anyhow::anyhow!(
                "trailing_stop_bps must be between 1 and 9999 (0.01% to 99.99%)"
            ));
        }
    }
    Ok(())
}

/// Evaluate whether `current_rate` hits Take-Profit, Stop-Loss, or Trailing-Stop.
/// Returns `(new_peak_rate_if_higher, triggered_reason)`.
pub fn evaluate_trigger_condition(
    take_profit_rate: Option<f64>,
    stop_loss_rate: Option<f64>,
    trailing_stop_bps: Option<u16>,
    peak_rate: Option<f64>,
    current_rate: f64,
) -> (Option<f64>, Option<String>) {
    if !current_rate.is_finite() || current_rate <= 0.0 {
        return (None, None);
    }
    let effective_peak = match peak_rate {
        Some(prev) if prev.is_finite() && prev >= current_rate => prev,
        _ => current_rate,
    };
    let updated_peak = if peak_rate.is_none_or(|prev| current_rate > prev) {
        Some(current_rate)
    } else {
        None
    };

    if let Some(tp) = take_profit_rate {
        if current_rate >= tp {
            return (
                updated_peak,
                Some(format!(
                    "take_profit triggered: rate {current_rate:.8} >= target {tp:.8}"
                )),
            );
        }
    }
    if let Some(sl) = stop_loss_rate {
        if current_rate <= sl {
            return (
                updated_peak,
                Some(format!(
                    "stop_loss triggered: rate {current_rate:.8} <= stop {sl:.8}"
                )),
            );
        }
    }
    if let Some(bps) = trailing_stop_bps {
        let floor = effective_peak * (1.0 - (bps as f64) / 10_000.0);
        if current_rate <= floor {
            return (
                updated_peak,
                Some(format!(
                    "trailing_stop triggered: rate {current_rate:.8} <= trailing floor {floor:.8} ({bps} bps below peak {effective_peak:.8})"
                )),
            );
        }
    }
    (updated_peak, None)
}

/// Compute closed-trade performance metrics (win rate, profit factor, gross profit/loss)
/// overall and grouped by strategy `tag`.
pub fn compute_strategy_performance(trades: &[TradeRecord]) -> Value {
    #[derive(Default)]
    struct TagPosition {
        units: u128,
        cost_lamports: u128,
        last_tag: Option<String>,
    }

    #[derive(Default)]
    struct TagStats {
        closed_trades: usize,
        winning_trades: usize,
        losing_trades: usize,
        gross_profit_lamports: u128,
        gross_loss_lamports: u128,
        net_pnl_lamports: i128,
    }

    impl TagStats {
        fn record(&mut self, pnl: i128) {
            self.closed_trades += 1;
            self.net_pnl_lamports += pnl;
            if pnl > 0 {
                self.winning_trades += 1;
                self.gross_profit_lamports += pnl as u128;
            } else if pnl < 0 {
                self.losing_trades += 1;
                self.gross_loss_lamports += pnl.unsigned_abs();
            }
        }

        fn to_json(&self) -> Value {
            let win_rate_pct = if self.closed_trades > 0 {
                Some(
                    (((self.winning_trades as f64) / (self.closed_trades as f64)) * 10_000.0)
                        .round()
                        / 100.0,
                )
            } else {
                None
            };
            let profit_factor = if self.gross_loss_lamports > 0 {
                Some(
                    (((self.gross_profit_lamports as f64) / (self.gross_loss_lamports as f64))
                        * 100.0)
                        .round()
                        / 100.0,
                )
            } else {
                None
            };
            json!({
                "closed_trades": self.closed_trades,
                "winning_trades": self.winning_trades,
                "losing_trades": self.losing_trades,
                "win_rate_pct": win_rate_pct,
                "realized_pnl_sol": format_signed_lamports(self.net_pnl_lamports),
                "gross_profit_sol": format_units(self.gross_profit_lamports.min(u64::MAX as u128) as u64, 9),
                "gross_loss_sol": format_units(self.gross_loss_lamports.min(u64::MAX as u128) as u64, 9),
                "profit_factor": profit_factor,
            })
        }
    }

    let mut positions: BTreeMap<String, TagPosition> = BTreeMap::new();
    let mut overall = TagStats::default();
    let mut by_tag: BTreeMap<String, TagStats> = BTreeMap::new();

    for trade in trades {
        let sol_val = trade.sol_value_lamports as u128;
        let clean_tag = trade
            .tag
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        if trade.input_mint != SOL_MINT && trade.input_amount > 0 {
            let pos = positions.entry(trade.input_mint.clone()).or_default();
            let sold = (trade.input_amount as u128).min(pos.units);
            if sold > 0 && pos.units > 0 {
                let cost_removed = (pos.cost_lamports * sold) / pos.units;
                pos.units = pos.units.saturating_sub(sold);
                pos.cost_lamports = pos.cost_lamports.saturating_sub(cost_removed);
                let pnl = (sol_val as i128) - (cost_removed as i128);
                let tag_key = clean_tag
                    .clone()
                    .or_else(|| pos.last_tag.clone())
                    .unwrap_or_else(|| "untagged".to_string());
                overall.record(pnl);
                by_tag.entry(tag_key).or_default().record(pnl);
                if pos.units == 0 {
                    pos.last_tag = None;
                }
            }
        }

        if trade.output_mint != SOL_MINT && trade.output_amount > 0 {
            let pos = positions.entry(trade.output_mint.clone()).or_default();
            pos.units = pos.units.saturating_add(trade.output_amount as u128);
            pos.cost_lamports = pos.cost_lamports.saturating_add(sol_val);
            if clean_tag.is_some() {
                pos.last_tag = clean_tag;
            }
        }
    }

    let tags_json: BTreeMap<String, Value> =
        by_tag.into_iter().map(|(k, v)| (k, v.to_json())).collect();
    let mut result = overall.to_json();
    result["by_tag"] = json!(tags_json);
    result
}

/// Compute a bankroll- and budget-aware SOL position size from a Kelly fraction (0.0..1.0).
pub fn compute_kelly_size_lamports(
    kelly_fraction: f64,
    sol_balance_lamports: u64,
    per_op_limit_lamports: u64,
    remaining_daily_lamports: u64,
) -> u64 {
    if !kelly_fraction.is_finite() || kelly_fraction <= 0.0 {
        return 0;
    }
    let clamped_kelly = kelly_fraction.clamp(0.0, 0.50);
    let spendable_sol = sol_balance_lamports.saturating_sub(20_000_000); // keep 0.02 SOL fee reserve
    let raw_kelly = ((spendable_sol as f64) * clamped_kelly).round() as u64;
    raw_kelly
        .min(spendable_sol)
        .min(per_op_limit_lamports)
        .min(remaining_daily_lamports)
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::program_option::COption;
    use solana_sdk::program_pack::Pack;
    use std::str::FromStr;

    fn pack_legacy_mint(
        mint_authority: Option<Pubkey>,
        freeze_authority: Option<Pubkey>,
        decimals: u8,
        supply: u64,
    ) -> Vec<u8> {
        let mint = spl_token::state::Mint {
            mint_authority: mint_authority.map(COption::Some).unwrap_or(COption::None),
            supply,
            decimals,
            is_initialized: true,
            freeze_authority: freeze_authority.map(COption::Some).unwrap_or(COption::None),
        };
        let mut bytes = vec![0u8; spl_token::state::Mint::LEN];
        spl_token::state::Mint::pack(mint, &mut bytes).unwrap();
        bytes
    }

    #[test]
    fn detects_freeze_authority_on_unknown_tokens_but_trusts_canonical_usdc() {
        let authority = Pubkey::new_unique();
        let random_mint = Pubkey::new_unique();
        let bytes = pack_legacy_mint(Some(authority), Some(authority), 6, 1_000_000_000);

        let unknown = parse_mint_safety(&random_mint, "MEME", &spl_token::id(), &bytes).unwrap();
        assert_eq!(unknown.risk_level, "high_risk");
        assert!(!unknown.is_canonical);
        assert!(!unknown.approval_reasons.is_empty());
        assert!(unknown.approval_reasons[0].contains("freeze_authority"));

        let usdc_mint = Pubkey::from_str(USDC_MINT).unwrap();
        let canonical = parse_mint_safety(&usdc_mint, "USDC", &spl_token::id(), &bytes).unwrap();
        assert_eq!(canonical.risk_level, "safe");
        assert!(canonical.is_canonical);
        assert!(canonical.approval_reasons.is_empty());
    }

    #[test]
    fn allowed_tokens_matches_symbols_and_mints() {
        let allowed = vec!["SOL".to_string(), "USDC".to_string()];
        assert!(token_is_allowed(&allowed, &Asset::sol(), "mainnet-beta"));
        let usdc = Asset {
            symbol: "USDC".into(),
            mint: Some(Pubkey::from_str(USDC_MINT).unwrap()),
            decimals: 6,
            token_program: Some(spl_token::id()),
        };
        assert!(token_is_allowed(&allowed, &usdc, "mainnet-beta"));

        let bonk = Asset {
            symbol: "BONK".into(),
            mint: Some(Pubkey::new_unique()),
            decimals: 5,
            token_program: Some(spl_token::id()),
        };
        assert!(!token_is_allowed(&allowed, &bonk, "mainnet-beta"));
    }

    #[test]
    fn computes_portfolio_cost_basis_and_realized_and_unrealized_pnl() {
        let token_mint = "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263".to_string();
        let trades = vec![
            // Buy 1,000 TOK for 1.0 SOL
            TradeRecord {
                operation_id: "op_1".into(),
                kind: "swap".into(),
                input_mint: SOL_MINT.into(),
                input_symbol: "SOL".into(),
                input_amount: 1_000_000_000,
                input_decimals: 9,
                output_mint: token_mint.clone(),
                output_symbol: "BONK".into(),
                output_amount: 1_000_000_000, // 1000.000000 (6 decimals)
                output_decimals: 6,
                sol_value_lamports: 1_000_000_000,
                tag: Some("BULL_TREND_15m".into()),
                created_at: "2026-11-07T10:00:00Z".into(),
            },
            // Sell 500 TOK for 0.8 SOL (cost of 500 TOK was 0.5 SOL -> +0.3 SOL realized PnL)
            TradeRecord {
                operation_id: "op_2".into(),
                kind: "swap".into(),
                input_mint: token_mint.clone(),
                input_symbol: "BONK".into(),
                input_amount: 500_000_000,
                input_decimals: 6,
                output_mint: SOL_MINT.into(),
                output_symbol: "SOL".into(),
                output_amount: 800_000_000,
                output_decimals: 9,
                sol_value_lamports: 800_000_000,
                tag: None, // inherits "BULL_TREND_15m" from the open position
                created_at: "2026-11-07T11:00:00Z".into(),
            },
        ];

        // Remaining 500 TOK is now worth 0.75 SOL on-chain (cost basis 0.5 SOL -> +0.25 SOL / +50% unrealized)
        let onchain = vec![(token_mint.clone(), "BONK".into(), 500_000_000u64, 6u8)];
        let mut valuations = BTreeMap::new();
        valuations.insert(token_mint.clone(), 750_000_000u64);

        let (positions, realized, unrealized) = compute_positions(&trades, &onchain, &valuations);
        assert_eq!(positions.len(), 1);
        let pos = &positions[0];
        assert_eq!(pos.balance, "500");
        assert_eq!(pos.tracked_cost_basis_sol, "0.5");
        assert_eq!(pos.realized_pnl_sol, "+0.3");
        assert_eq!(pos.unrealized_pnl_sol.as_deref(), Some("+0.25"));
        assert_eq!(pos.unrealized_pnl_pct, Some(50.0));
        assert_eq!(realized, 300_000_000);
        assert_eq!(unrealized, 250_000_000);

        let perf = compute_strategy_performance(&trades);
        assert_eq!(perf["closed_trades"], 1);
        assert_eq!(perf["winning_trades"], 1);
        assert_eq!(perf["win_rate_pct"], 100.0);
        assert_eq!(perf["by_tag"]["BULL_TREND_15m"]["realized_pnl_sol"], "+0.3");
        assert_eq!(perf["by_tag"]["BULL_TREND_15m"]["win_rate_pct"], 100.0);
    }

    #[test]
    fn validates_and_evaluates_oco_take_profit_stop_loss_and_trailing_triggers() {
        assert!(validate_trigger_rates(None, None, None).is_err());
        assert!(validate_trigger_rates(Some(180.0), Some(190.0), None).is_err());
        assert!(validate_trigger_rates(None, None, Some(0)).is_err());
        assert!(validate_trigger_rates(Some(195.0), Some(178.0), Some(200)).is_ok());

        let (peak, fired) =
            evaluate_trigger_condition(Some(195.0), Some(178.0), Some(200), Some(185.0), 190.0);
        assert_eq!(peak, Some(190.0));
        assert!(fired.is_none());

        // Trailing stop: peak is 190.0, 200 bps (2%) floor is 186.2. Rate 186.0 triggers trailing_stop!
        let (_peak, trailing) =
            evaluate_trigger_condition(Some(195.0), Some(178.0), Some(200), Some(190.0), 186.0);
        assert!(trailing.unwrap().contains("trailing_stop triggered"));

        let (_peak, tp) =
            evaluate_trigger_condition(Some(195.0), Some(178.0), None, None, 195.5);
        assert!(tp.unwrap().contains("take_profit triggered"));

        let (_peak, sl) =
            evaluate_trigger_condition(Some(195.0), Some(178.0), None, None, 177.2);
        assert!(sl.unwrap().contains("stop_loss triggered"));
    }

    #[test]
    fn computes_kelly_position_size_within_wallet_and_budget_bounds() {
        // 10 SOL balance (9.98 spendable after 0.02 reserve), 20% Kelly -> ~1.996 SOL, capped by 0.5 SOL per_op
        let capped = compute_kelly_size_lamports(0.20, 10_000_000_000, 500_000_000, 2_000_000_000);
        assert_eq!(capped, 500_000_000);

        // 2 SOL balance (1.98 spendable), 10% Kelly -> 0.198 SOL (198_000_000 lamports)
        let exact = compute_kelly_size_lamports(0.10, 2_000_000_000, 500_000_000, 2_000_000_000);
        assert_eq!(exact, 198_000_000);
    }
}
