//! A bill one agent wallet signs and another pays.
//!
//! The signed fields are the whole agreement: who is paid, how much, for what,
//! and until when. Paying it is an ordinary transfer with the invoice id as the
//! memo and as the idempotency key. The chain, not either agent's word, decides
//! when the bill is settled.

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_client::GetConfirmedSignaturesForAddress2Config;
use solana_client::rpc_config::RpcTransactionConfig;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    pubkey::Pubkey,
    signature::{Keypair, Signature, Signer},
    transaction::VersionedTransaction,
};
use solana_transaction_status::{option_serializer::OptionSerializer, UiTransactionEncoding};
use spl_associated_token_account::get_associated_token_address_with_program_id;
use std::collections::HashSet;
use std::str::FromStr;

const MEMO_PROGRAM: &str = "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr";
const MAX_SIGNATURE_FETCHES: usize = 16;

/// Shown to the human the first time an invoice pays someone new.
pub const FIRST_PAYMENT_REASON: &str = "first payment to this agent; approving also allows later \
payments to this address inside the budget";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Invoice {
    pub invoice_id: String,
    pub payee: String,
    pub token: String,
    pub amount: String,
    pub description: String,
    pub expires_at: String,
    pub cluster: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SignedInvoice {
    pub invoice: Invoice,
    pub signature: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundPayment {
    pub signature: String,
    pub slot: u64,
    pub finalized: bool,
}

pub struct ExpectedPayment<'a> {
    pub payee: Pubkey,
    pub invoice_id: &'a str,
    pub units: u64,
    pub mint: Option<Pubkey>,
    pub token_program: Option<Pubkey>,
}

pub struct TokenDelta {
    pub owner: Pubkey,
    pub mint: Pubkey,
    pub delta: i128,
}

/// Bytes the payee signs. Field order is fixed; a changed amount or description
/// produces a different signature.
pub fn canonical(invoice: &Invoice) -> String {
    format!(
        "manus.invoice.v1\ninvoice_id={}\npayee={}\ntoken={}\namount={}\ndescription={}\nexpires_at={}\ncluster={}\n",
        invoice.invoice_id,
        invoice.payee,
        invoice.token,
        invoice.amount,
        invoice.description,
        invoice.expires_at,
        invoice.cluster
    )
}

pub fn sign(invoice: &Invoice, signer: &Keypair) -> anyhow::Result<SignedInvoice> {
    validate_fields(invoice)?;
    if signer.pubkey().to_string() != invoice.payee {
        return Err(anyhow::anyhow!("only the payee can sign an invoice"));
    }
    let signature = signer.sign_message(canonical(invoice).as_bytes());
    let signed = SignedInvoice {
        invoice: invoice.clone(),
        signature: signature.to_string(),
    };
    verify(&signed)?;
    Ok(signed)
}

pub fn verify(signed: &SignedInvoice) -> anyhow::Result<()> {
    validate_fields(&signed.invoice)?;
    let payee = Pubkey::from_str(&signed.invoice.payee)?;
    let signature = Signature::from_str(&signed.signature)
        .map_err(|_| anyhow::anyhow!("invoice signature is not valid base58"))?;
    if !signature.verify(payee.as_ref(), canonical(&signed.invoice).as_bytes()) {
        return Err(anyhow::anyhow!(
            "invoice signature does not match the payee"
        ));
    }
    Ok(())
}

pub fn parse_signed(value: &Value) -> anyhow::Result<SignedInvoice> {
    let invoice = value
        .get("invoice")
        .ok_or_else(|| anyhow::anyhow!("invoice document needs an 'invoice' object"))?;
    let signature = value
        .get("signature")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("invoice document needs a 'signature'"))?;
    let signed = SignedInvoice {
        invoice: serde_json::from_value(invoice.clone())?,
        signature: signature.to_string(),
    };
    verify(&signed)?;
    Ok(signed)
}

pub fn is_expired(expires_at: &str) -> bool {
    match DateTime::parse_from_rfc3339(expires_at) {
        Ok(when) => Utc::now() > when,
        Err(_) => true,
    }
}

pub fn expires_after(hours: u64) -> anyhow::Result<String> {
    if !(1..=720).contains(&hours) {
        return Err(anyhow::anyhow!(
            "invoice lifetime must be between 1 and 720 hours"
        ));
    }
    let when = Utc::now() + chrono::Duration::hours(hours as i64);
    Ok(when.to_rfc3339_opts(SecondsFormat::Secs, true))
}

pub fn recipient_is_trusted(recipients: &[String], payee: &Pubkey) -> bool {
    recipients.iter().any(|recipient| {
        Pubkey::from_str(recipient)
            .ok()
            .is_some_and(|key| key == *payee)
    })
}

pub fn valid_id(id: &str) -> bool {
    (4..=80).contains(&id.len())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '.'))
}

fn validate_fields(invoice: &Invoice) -> anyhow::Result<()> {
    if !valid_id(&invoice.invoice_id) {
        return Err(anyhow::anyhow!(
            "invoice_id must be 4-80 characters of letters, digits, '-', '_', ':' or '.'"
        ));
    }
    Pubkey::from_str(&invoice.payee)
        .map_err(|_| anyhow::anyhow!("invoice payee is not a Solana address"))?;
    if invoice.token.is_empty()
        || invoice.token.len() > 64
        || invoice.token.chars().any(|c| c.is_whitespace())
    {
        return Err(anyhow::anyhow!("invoice token is missing or not a symbol"));
    }
    if invoice.amount.is_empty()
        || invoice.amount.starts_with('-')
        || invoice.amount.starts_with('+')
    {
        return Err(anyhow::anyhow!("invoice amount must be a positive decimal"));
    }
    let description = invoice.description.chars().count();
    if !(1..=160).contains(&description)
        || invoice
            .description
            .chars()
            .any(|c| c.is_control() || c == '\u{2028}' || c == '\u{2029}')
    {
        return Err(anyhow::anyhow!(
            "description must be 1-160 characters without line breaks"
        ));
    }
    DateTime::parse_from_rfc3339(&invoice.expires_at)
        .map_err(|_| anyhow::anyhow!("expires_at must be an RFC3339 timestamp"))?;
    if !matches!(
        invoice.cluster.as_str(),
        "devnet" | "localnet" | "mainnet-beta"
    ) {
        return Err(anyhow::anyhow!(
            "invoice cluster must be devnet, localnet or mainnet-beta"
        ));
    }
    Ok(())
}

/// A finalized transfer to `payee` of the exact amount, carrying this invoice id.
pub fn payment_matches(
    tx: &VersionedTransaction,
    failed: bool,
    pre_lamports: &[u64],
    post_lamports: &[u64],
    token_deltas: &[TokenDelta],
    expected: &ExpectedPayment<'_>,
) -> bool {
    if failed || !has_memo(tx, expected.invoice_id) {
        return false;
    }
    match (expected.mint, expected.token_program) {
        (None, _) => sol_paid(
            tx,
            pre_lamports,
            post_lamports,
            &expected.payee,
            expected.units,
        ),
        (Some(mint), Some(program)) => token_paid(
            tx,
            token_deltas,
            &expected.payee,
            &mint,
            &program,
            expected.units,
        ),
        (Some(_), None) => false,
    }
}

/// Look for a payment of this invoice. A signature hint is checked first; otherwise
/// the latest transactions on the payee (and their token account) are scanned.
pub async fn find_payment(
    rpc: &RpcClient,
    expected: &ExpectedPayment<'_>,
    hint: Option<&str>,
) -> anyhow::Result<Option<FoundPayment>> {
    if let Some(signature) = hint {
        if let Some(found) = lookup_signature(rpc, signature, expected).await? {
            return Ok(Some(found));
        }
    }

    let mut addresses = vec![expected.payee];
    if let (Some(mint), Some(program)) = (expected.mint, expected.token_program) {
        addresses.push(get_associated_token_address_with_program_id(
            &expected.payee,
            &mint,
            &program,
        ));
    }

    let mut seen = HashSet::new();
    let mut fallback = Vec::new();
    let mut confirming = None;
    let mut fetches = 0usize;
    for address in addresses {
        let signatures = rpc
            .get_signatures_for_address_with_config(
                &address,
                GetConfirmedSignaturesForAddress2Config {
                    before: None,
                    until: None,
                    limit: Some(50),
                    commitment: Some(solana_sdk::commitment_config::CommitmentConfig::confirmed()),
                },
            )
            .await?;
        for status in signatures {
            if !seen.insert(status.signature.clone()) {
                continue;
            }
            if status.err.is_some() {
                continue;
            }
            match status.memo.as_deref() {
                Some(memo) if memo.contains(expected.invoice_id) => {}
                Some(_) => continue,
                None => {
                    if fallback.len() < 8 {
                        fallback.push(status.signature);
                    }
                    continue;
                }
            }
            if let Some(found) = consider(
                rpc,
                &status.signature,
                expected,
                &mut confirming,
                &mut fetches,
            )
            .await?
            {
                return Ok(Some(found));
            }
        }
    }
    for signature in fallback {
        if fetches >= MAX_SIGNATURE_FETCHES {
            break;
        }
        if let Some(found) =
            consider(rpc, &signature, expected, &mut confirming, &mut fetches).await?
        {
            return Ok(Some(found));
        }
    }
    Ok(confirming)
}

async fn consider(
    rpc: &RpcClient,
    signature: &str,
    expected: &ExpectedPayment<'_>,
    confirming: &mut Option<FoundPayment>,
    fetches: &mut usize,
) -> anyhow::Result<Option<FoundPayment>> {
    if *fetches >= MAX_SIGNATURE_FETCHES {
        return Ok(None);
    }
    *fetches += 1;
    match lookup_signature(rpc, signature, expected).await? {
        Some(found) if found.finalized => Ok(Some(found)),
        Some(found) => {
            if confirming.is_none() {
                *confirming = Some(found);
            }
            Ok(None)
        }
        None => Ok(None),
    }
}

async fn lookup_signature(
    rpc: &RpcClient,
    signature: &str,
    expected: &ExpectedPayment<'_>,
) -> anyhow::Result<Option<FoundPayment>> {
    let Ok(signature_key) = Signature::from_str(signature) else {
        return Ok(None);
    };
    if let Some(found) = fetch_match(rpc, &signature_key, signature, expected, None).await? {
        return Ok(Some(found));
    }
    fetch_match(
        rpc,
        &signature_key,
        signature,
        expected,
        Some(CommitmentConfig::confirmed()),
    )
    .await
}

async fn fetch_match(
    rpc: &RpcClient,
    signature_key: &Signature,
    signature: &str,
    expected: &ExpectedPayment<'_>,
    commitment: Option<CommitmentConfig>,
) -> anyhow::Result<Option<FoundPayment>> {
    let fetched = match commitment {
        None => match rpc
            .get_transaction(signature_key, UiTransactionEncoding::Base64)
            .await
        {
            Ok(fetched) => fetched,
            Err(_) => return Ok(None),
        },
        Some(commitment) => {
            match rpc
                .get_transaction_with_config(
                    signature_key,
                    RpcTransactionConfig {
                        encoding: Some(UiTransactionEncoding::Base64),
                        commitment: Some(commitment),
                        max_supported_transaction_version: Some(0),
                    },
                )
                .await
            {
                Ok(fetched) => fetched,
                Err(_) => return Ok(None),
            }
        }
    };
    let Some(tx) = fetched.transaction.transaction.decode() else {
        return Ok(None);
    };
    let meta = fetched.transaction.meta.as_ref();
    let failed = meta.is_none_or(|meta| meta.err.is_some());
    let (pre, post) = meta
        .map(|meta| (meta.pre_balances.as_slice(), meta.post_balances.as_slice()))
        .unwrap_or((&[], &[]));
    let deltas = meta.map(token_deltas).unwrap_or_default();
    if !payment_matches(&tx, failed, pre, post, &deltas, expected) {
        return Ok(None);
    }
    Ok(Some(FoundPayment {
        signature: signature.to_string(),
        slot: fetched.slot,
        finalized: commitment.is_none(),
    }))
}

fn token_deltas(meta: &solana_transaction_status::UiTransactionStatusMeta) -> Vec<TokenDelta> {
    let mut totals: Vec<(Pubkey, Pubkey, i128)> = Vec::new();
    let apply = |totals: &mut Vec<(Pubkey, Pubkey, i128)>,
                 balances: &OptionSerializer<
        Vec<solana_transaction_status::UiTransactionTokenBalance>,
    >,
                 sign: i128| {
        let OptionSerializer::Some(balances) = balances else {
            return;
        };
        for balance in balances {
            let OptionSerializer::Some(owner) = &balance.owner else {
                continue;
            };
            let (Ok(owner), Ok(mint)) = (Pubkey::from_str(owner), Pubkey::from_str(&balance.mint))
            else {
                continue;
            };
            let Ok(amount) = balance.ui_token_amount.amount.parse::<i128>() else {
                continue;
            };
            if let Some(entry) = totals
                .iter_mut()
                .find(|(have_owner, have_mint, _)| *have_owner == owner && *have_mint == mint)
            {
                entry.2 += sign * amount;
            } else {
                totals.push((owner, mint, sign * amount));
            }
        }
    };
    apply(&mut totals, &meta.pre_token_balances, -1);
    apply(&mut totals, &meta.post_token_balances, 1);
    totals
        .into_iter()
        .map(|(owner, mint, delta)| TokenDelta { owner, mint, delta })
        .collect()
}

fn has_memo(tx: &VersionedTransaction, invoice_id: &str) -> bool {
    let keys = tx.message.static_account_keys();
    tx.message.instructions().iter().any(|ix| {
        keys.get(ix.program_id_index as usize)
            .is_some_and(|program| program.to_string() == MEMO_PROGRAM)
            && ix.data == invoice_id.as_bytes()
    })
}

fn sol_paid(
    tx: &VersionedTransaction,
    pre: &[u64],
    post: &[u64],
    payee: &Pubkey,
    units: u64,
) -> bool {
    let keys = tx.message.static_account_keys();
    let Some(index) = keys.iter().position(|key| key == payee) else {
        return false;
    };
    let Some(gained) = post
        .get(index)
        .copied()
        .zip(pre.get(index).copied())
        .and_then(|(after, before)| after.checked_sub(before))
    else {
        return false;
    };
    if gained != units {
        return false;
    }
    let system = solana_system_interface::program::id();
    tx.message.instructions().iter().any(|ix| {
        let Some(program) = keys.get(ix.program_id_index as usize) else {
            return false;
        };
        if *program != system {
            return false;
        }
        let Ok(decoded) = bincode::deserialize::<
            solana_system_interface::instruction::SystemInstruction,
        >(&ix.data) else {
            return false;
        };
        match decoded {
            solana_system_interface::instruction::SystemInstruction::Transfer { lamports } => {
                lamports == units
                    && ix
                        .accounts
                        .get(1)
                        .and_then(|account| keys.get(*account as usize))
                        == Some(payee)
            }
            _ => false,
        }
    })
}

fn token_paid(
    tx: &VersionedTransaction,
    deltas: &[TokenDelta],
    payee: &Pubkey,
    mint: &Pubkey,
    program: &Pubkey,
    units: u64,
) -> bool {
    let destination = get_associated_token_address_with_program_id(payee, mint, program);
    let received = deltas
        .iter()
        .any(|delta| delta.owner == *payee && delta.mint == *mint && delta.delta == units as i128);
    if !received {
        return false;
    }
    let keys = tx.message.static_account_keys();
    tx.message.instructions().iter().any(|ix| {
        let Some(program_key) = keys.get(ix.program_id_index as usize) else {
            return false;
        };
        if program_key != program || ix.data.first() != Some(&12) || ix.data.len() < 9 {
            return false;
        }
        let amount = u64::from_le_bytes(ix.data[1..9].try_into().unwrap_or([0; 8]));
        amount == units
            && ix
                .accounts
                .get(2)
                .and_then(|account| keys.get(*account as usize))
                == Some(&destination)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tx::{self, Asset};
    use solana_sdk::{
        hash::Hash,
        message::{Message, VersionedMessage},
    };

    fn sample(payee: &Pubkey) -> Invoice {
        Invoice {
            invoice_id: "inv_test_1001".into(),
            payee: payee.to_string(),
            token: "USDC".into(),
            amount: "1.5".into(),
            description: "crawl the docs".into(),
            expires_at: "2099-01-01T00:00:00Z".into(),
            cluster: "devnet".into(),
        }
    }

    #[test]
    fn signature_covers_the_amount_and_the_payee() {
        let payee = Keypair::new();
        let signed = sign(&sample(&payee.pubkey()), &payee).unwrap();
        verify(&signed).unwrap();

        let mut tampered = signed.clone();
        tampered.invoice.amount = "1.6".into();
        assert!(verify(&tampered).is_err());

        let other = Keypair::new();
        assert!(sign(&sample(&payee.pubkey()), &other).is_err());
    }

    #[test]
    fn expired_and_malformed_bills_are_rejected() {
        assert!(is_expired("2000-01-01T00:00:00Z"));
        assert!(!is_expired("2099-01-01T00:00:00Z"));
        let payee = Keypair::new();
        let mut invoice = sample(&payee.pubkey());
        invoice.description = "line\nbreak".into();
        assert!(sign(&invoice, &payee).is_err());
        invoice = sample(&payee.pubkey());
        invoice.invoice_id = "no".into();
        assert!(sign(&invoice, &payee).is_err());
    }

    fn signed_transfer(
        payer: &Keypair,
        instructions: &[solana_sdk::instruction::Instruction],
    ) -> VersionedTransaction {
        let message =
            Message::new_with_blockhash(instructions, Some(&payer.pubkey()), &Hash::default());
        VersionedTransaction::try_new(VersionedMessage::Legacy(message), &[payer]).unwrap()
    }

    #[test]
    fn a_sol_transfer_with_the_memo_settles_the_invoice() {
        let payer = Keypair::new();
        let payee = Keypair::new().pubkey();
        let invoice_id = "inv_test_1001";
        let tx = signed_transfer(
            &payer,
            &tx::sol_transfer_instructions(&payer.pubkey(), &payee, 1_000, Some(invoice_id)),
        );
        let keys = tx.message.static_account_keys();
        let mut pre = vec![0u64; keys.len()];
        let mut post = pre.clone();
        let index = keys.iter().position(|key| *key == payee).unwrap();
        pre[index] = 5;
        post[index] = 1_005;
        let expected = ExpectedPayment {
            payee,
            invoice_id,
            units: 1_000,
            mint: None,
            token_program: None,
        };
        assert!(payment_matches(&tx, false, &pre, &post, &[], &expected));
        post[index] = 1_004;
        assert!(!payment_matches(&tx, false, &pre, &post, &[], &expected));
        post[index] = 1_005;
        assert!(!payment_matches(&tx, true, &pre, &post, &[], &expected));
    }

    #[test]
    fn a_token_transfer_must_reach_the_payees_account() {
        let payer = Keypair::new();
        let payee = Keypair::new().pubkey();
        let mint = Pubkey::new_unique();
        let asset = Asset {
            symbol: "USDC".into(),
            mint: Some(mint),
            decimals: 6,
            token_program: Some(spl_token::id()),
        };
        let invoice_id = "inv_test_1001";
        let tx = signed_transfer(
            &payer,
            &tx::token_transfer_instructions(
                &payer.pubkey(),
                &payee,
                &asset,
                1_500_000,
                Some(invoice_id),
            )
            .unwrap(),
        );
        let expected = ExpectedPayment {
            payee,
            invoice_id,
            units: 1_500_000,
            mint: Some(mint),
            token_program: Some(spl_token::id()),
        };
        let deltas = vec![TokenDelta {
            owner: payee,
            mint,
            delta: 1_500_000,
        }];
        assert!(payment_matches(&tx, false, &[], &[], &deltas, &expected));
        let short = vec![TokenDelta {
            owner: payee,
            mint,
            delta: 1_500_000 - 1,
        }];
        assert!(!payment_matches(&tx, false, &[], &[], &short, &expected));
    }
}
