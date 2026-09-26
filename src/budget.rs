//! Spending budget for an agent-operated hot wallet.
//!
//! The budget is the primary blast-radius control: inside it an agent acts on its
//! own, outside it a human must approve with a platform presence check. Budgets
//! are stored locally and can only be *raised* through that same approval gate.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const LAMPORTS_PER_SOL: u64 = 1_000_000_000;

/// Per-operation and rolling-day ceilings in base units.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct Limits {
    pub per_op: u64,
    pub daily: u64,
}

impl Limits {
    fn covers(&self, other: &Limits) -> bool {
        self.per_op >= other.per_op && self.daily >= other.daily
    }
}

/// What an operation does to value held by the wallet.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    /// Value leaves the wallet for another owner (send, pay).
    Send,
    /// Value stays in the wallet but changes form (swap, stake, unstake).
    Convert,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Budget {
    /// SOL outflow limits for transfers to other owners, in lamports.
    pub send_sol: Limits,
    /// SOL-denominated limits for swaps and liquid staking, in lamports.
    pub convert_sol: Limits,
    /// Per-mint limits for SPL token transfers, in the mint's base units.
    #[serde(default)]
    pub send_tokens: BTreeMap<String, Limits>,
    /// Recipients that never need approval inside the limits above.
    #[serde(default)]
    pub trusted_recipients: Vec<String>,
    /// When true, any recipient outside `trusted_recipients` needs approval.
    #[serde(default)]
    pub require_trusted_recipients: bool,
    /// Largest slippage an agent may request for conversions.
    pub max_slippage_bps: u16,
}

impl Budget {
    /// Conservative mainnet defaults agreed with the operator.
    pub fn default_for_cluster(cluster: &str) -> Self {
        let mut send_tokens = BTreeMap::new();
        let usdc = if cluster == "mainnet-beta" {
            crate::mints::USDC_MINT
        } else {
            DEVNET_USDC_MINT
        };
        send_tokens.insert(
            usdc.to_string(),
            Limits {
                per_op: 25 * 1_000_000,
                daily: 100 * 1_000_000,
            },
        );
        Self {
            send_sol: Limits {
                per_op: LAMPORTS_PER_SOL / 2,
                daily: 2 * LAMPORTS_PER_SOL,
            },
            convert_sol: Limits {
                per_op: LAMPORTS_PER_SOL / 2,
                daily: 2 * LAMPORTS_PER_SOL,
            },
            send_tokens,
            trusted_recipients: Vec::new(),
            require_trusted_recipients: false,
            max_slippage_bps: 100,
        }
    }

    /// True when `proposed` grants no permission this budget does not already grant.
    /// Only a non-widening change may be applied without human approval.
    pub fn permits_without_approval(&self, proposed: &Budget) -> bool {
        if !self.send_sol.covers(&proposed.send_sol)
            || !self.convert_sol.covers(&proposed.convert_sol)
            || proposed.max_slippage_bps > self.max_slippage_bps
            || (self.require_trusted_recipients && !proposed.require_trusted_recipients)
        {
            return false;
        }
        for (mint, limits) in &proposed.send_tokens {
            match self.send_tokens.get(mint) {
                Some(current) if current.covers(limits) => {}
                _ => return false,
            }
        }
        proposed
            .trusted_recipients
            .iter()
            .all(|recipient| self.trusted_recipients.contains(recipient))
    }
}

pub const DEVNET_USDC_MINT: &str = "4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU";

/// The value an operation moves, measured from simulation rather than from the
/// agent's request, so a request cannot understate what a transaction does.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Exposure {
    pub category: Category,
    /// SOL value leaving the wallet (or converted), excluding fees, in lamports.
    pub sol_out: u64,
    /// Network fees and account rent. Bounded per operation kind by the engine and
    /// reported, but not counted against limits.
    #[serde(default)]
    pub fees: u64,
    /// Net token outflow for SPL transfers: (mint, base units).
    pub token_out: Option<(String, u64)>,
    pub recipient: Option<String>,
}

/// Amounts already committed today by operations that may have landed.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpentToday {
    pub send_sol: u64,
    pub convert_sol: u64,
    pub send_tokens: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum Decision {
    Auto,
    NeedsApproval { reasons: Vec<String> },
    Deny { reason: String },
}

pub fn decide(budget: &Budget, spent: &SpentToday, exposure: &Exposure) -> Decision {
    let mut reasons = Vec::new();

    let (limits, spent_sol, label) = match exposure.category {
        Category::Send => (&budget.send_sol, spent.send_sol, "send"),
        Category::Convert => (&budget.convert_sol, spent.convert_sol, "convert"),
    };
    check_limits(
        limits,
        spent_sol,
        exposure.sol_out,
        &format!("{label} SOL"),
        &mut reasons,
    );

    if let Some((mint, amount)) = &exposure.token_out {
        match budget.send_tokens.get(mint) {
            Some(token_limits) => check_limits(
                token_limits,
                spent.send_tokens.get(mint).copied().unwrap_or(0),
                *amount,
                &format!("token {mint}"),
                &mut reasons,
            ),
            None => reasons.push(format!("token {mint} has no agent budget")),
        }
    }

    if let Some(recipient) = &exposure.recipient {
        if budget.require_trusted_recipients
            && !budget.trusted_recipients.iter().any(|r| r == recipient)
        {
            reasons.push(format!("recipient {recipient} is not trusted"));
        }
    }

    if reasons.is_empty() {
        Decision::Auto
    } else {
        Decision::NeedsApproval { reasons }
    }
}

fn check_limits(limits: &Limits, spent: u64, amount: u64, label: &str, reasons: &mut Vec<String>) {
    if amount > limits.per_op {
        reasons.push(format!(
            "{label}: {amount} exceeds per-operation limit {}",
            limits.per_op
        ));
    }
    let projected = spent.saturating_add(amount);
    if projected > limits.daily {
        reasons.push(format!(
            "{label}: {projected} today would exceed daily limit {}",
            limits.daily
        ));
    }
}

/// Parse a decimal amount such as "0.25" into base units without floating point.
pub fn parse_units(text: &str, decimals: u8) -> anyhow::Result<u64> {
    let text = text.trim();
    if text.is_empty() || text.starts_with('-') || text.starts_with('+') {
        return Err(anyhow::anyhow!("amount must be a positive decimal number"));
    }
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    if !whole.chars().all(|c| c.is_ascii_digit()) || !fraction.chars().all(|c| c.is_ascii_digit()) {
        return Err(anyhow::anyhow!("amount '{text}' is not a decimal number"));
    }
    if fraction.len() > decimals as usize {
        return Err(anyhow::anyhow!(
            "amount '{text}' has more than {decimals} decimal places"
        ));
    }
    let scale = 10u64
        .checked_pow(decimals as u32)
        .ok_or_else(|| anyhow::anyhow!("unsupported decimals {decimals}"))?;
    let whole: u64 = if whole.is_empty() { 0 } else { whole.parse()? };
    let padded = format!("{fraction:0<width$}", width = decimals as usize);
    let fraction: u64 = if padded.is_empty() {
        0
    } else {
        padded.parse()?
    };
    let units = whole
        .checked_mul(scale)
        .and_then(|value| value.checked_add(fraction))
        .ok_or_else(|| anyhow::anyhow!("amount '{text}' is too large"))?;
    if units == 0 {
        return Err(anyhow::anyhow!("amount must be greater than zero"));
    }
    Ok(units)
}

pub fn format_units(units: u64, decimals: u8) -> String {
    if decimals == 0 {
        return units.to_string();
    }
    let scale = 10u64.pow(decimals as u32);
    let fraction = format!("{:0width$}", units % scale, width = decimals as usize);
    let fraction = fraction.trim_end_matches('0');
    if fraction.is_empty() {
        (units / scale).to_string()
    } else {
        format!("{}.{}", units / scale, fraction)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn send(sol_out: u64) -> Exposure {
        Exposure {
            category: Category::Send,
            sol_out,
            fees: 5_000,
            token_out: None,
            recipient: Some("Recipient1111".into()),
        }
    }

    #[test]
    fn inside_budget_is_automatic() {
        let budget = Budget::default_for_cluster("mainnet-beta");
        let decision = decide(
            &budget,
            &SpentToday::default(),
            &send(LAMPORTS_PER_SOL / 10),
        );
        assert_eq!(decision, Decision::Auto);
    }

    #[test]
    fn a_limit_is_reachable_exactly_despite_fees() {
        let budget = Budget::default_for_cluster("mainnet-beta");
        let decision = decide(
            &budget,
            &SpentToday::default(),
            &send(budget.send_sol.per_op),
        );
        assert_eq!(decision, Decision::Auto);
    }

    #[test]
    fn per_operation_and_daily_limits_need_approval() {
        let budget = Budget::default_for_cluster("mainnet-beta");
        let over_op = decide(&budget, &SpentToday::default(), &send(LAMPORTS_PER_SOL));
        assert!(matches!(over_op, Decision::NeedsApproval { .. }));

        let spent = SpentToday {
            send_sol: 2 * LAMPORTS_PER_SOL - 1,
            ..Default::default()
        };
        let over_day = decide(&budget, &spent, &send(2));
        assert!(matches!(over_day, Decision::NeedsApproval { .. }));
    }

    #[test]
    fn conversions_use_their_own_allowance() {
        let budget = Budget::default_for_cluster("mainnet-beta");
        let spent = SpentToday {
            send_sol: 2 * LAMPORTS_PER_SOL,
            ..Default::default()
        };
        let exposure = Exposure {
            category: Category::Convert,
            sol_out: LAMPORTS_PER_SOL / 4,
            fees: 0,
            token_out: None,
            recipient: None,
        };
        assert_eq!(decide(&budget, &spent, &exposure), Decision::Auto);
    }

    #[test]
    fn unbudgeted_tokens_and_untrusted_recipients_need_approval() {
        let mut budget = Budget::default_for_cluster("mainnet-beta");
        let exposure = Exposure {
            category: Category::Send,
            sol_out: 0,
            fees: 5_000,
            token_out: Some(("UnknownMint".into(), 1)),
            recipient: Some("Stranger".into()),
        };
        assert!(matches!(
            decide(&budget, &SpentToday::default(), &exposure),
            Decision::NeedsApproval { .. }
        ));

        budget.require_trusted_recipients = true;
        let decision = decide(&budget, &SpentToday::default(), &send(1));
        match decision {
            Decision::NeedsApproval { reasons } => {
                assert!(reasons[0].contains("not trusted"))
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn widening_a_budget_requires_approval() {
        let current = Budget::default_for_cluster("mainnet-beta");
        let mut tighter = current.clone();
        tighter.send_sol.daily /= 2;
        assert!(current.permits_without_approval(&tighter));

        let mut wider = current.clone();
        wider.send_sol.per_op *= 2;
        assert!(!current.permits_without_approval(&wider));

        let mut new_recipient = current.clone();
        new_recipient.trusted_recipients.push("Someone".into());
        assert!(!current.permits_without_approval(&new_recipient));

        let mut new_token = current.clone();
        new_token.send_tokens.insert(
            "Mint".into(),
            Limits {
                per_op: 1,
                daily: 1,
            },
        );
        assert!(!current.permits_without_approval(&new_token));
    }

    #[test]
    fn decimal_parsing_is_exact() {
        assert_eq!(parse_units("0.5", 9).unwrap(), 500_000_000);
        assert_eq!(parse_units("12", 6).unwrap(), 12_000_000);
        assert_eq!(parse_units(".25", 2).unwrap(), 25);
        assert!(parse_units("0.0000000001", 9).is_err());
        assert!(parse_units("-1", 9).is_err());
        assert!(parse_units("0", 9).is_err());
        assert!(parse_units("1e3", 9).is_err());
        assert!(parse_units("99999999999999999999", 9).is_err());
        assert_eq!(format_units(500_000_000, 9), "0.5");
        assert_eq!(format_units(12_000_000, 6), "12");
    }
}
