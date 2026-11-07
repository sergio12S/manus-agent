//! Verification for Manus invoices, `manus.receipt.v1` documents, and operation receipts.
//!
//! `manus verify` (and the `verify` MCP tool) lets any agent or operator check a
//! payment artifact without trusting the sender's word:
//! - For a signed invoice (`manus.invoice.v1`), it verifies the payee's Ed25519
//!   signature over the canonical bill fields and checks Solana RPC for a matching
//!   on-chain transfer carrying the invoice memo.
//! - For a `manus.receipt.v1` or operation receipt, it validates the receipt
//!   structure and verifies the recorded transaction signature on Solana RPC,
//!   confirming that the recipient actually received the claimed SOL or token amount.

use serde_json::{json, Value};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_config::RpcTransactionConfig;
use solana_sdk::{commitment_config::CommitmentConfig, pubkey::Pubkey, signature::Signature};
use solana_transaction_status::UiTransactionEncoding;
use std::str::FromStr;

use crate::budget::parse_units;
use crate::invoice::{self, ExpectedPayment};
use crate::tx;
use crate::x402::RECEIPT_SCHEMA;

/// Verify any Manus artifact JSON (`SignedInvoice`, `manus.receipt.v1`, or operation receipt).
/// When `rpc` is `Some`, on-chain settlement and balance effects are verified against Solana.
pub async fn verify_document(
    document: &Value,
    rpc: Option<(&RpcClient, &str)>,
) -> anyhow::Result<Value> {
    let target = unwrap_envelope(document);
    if target.get("invoice").is_some() && target.get("signature").is_some() {
        return verify_invoice(target, rpc).await;
    }
    if target.get("schema_version").and_then(Value::as_str) == Some(RECEIPT_SCHEMA) {
        return verify_receipt_v1(target, rpc).await;
    }
    if is_operation_record(target) {
        return verify_operation(target, rpc).await;
    }
    Err(anyhow::anyhow!(
        "unrecognized document: expected a signed invoice, manus.receipt.v1, or agent wallet operation receipt"
    ))
}

fn unwrap_envelope(document: &Value) -> &Value {
    if let Some(inner) = document.get("receipt").filter(|v| v.is_object()) {
        return inner;
    }
    if let Some(inner) = document
        .get("operation")
        .filter(|v| v.is_object() && document.get("invoice").is_none())
    {
        return inner;
    }
    document
}

fn is_operation_record(value: &Value) -> bool {
    value
        .get("id")
        .and_then(Value::as_str)
        .is_some_and(|id| id.starts_with("op_"))
        && value.get("kind").is_some()
        && value.get("status").is_some()
}

async fn verify_invoice(
    document: &Value,
    rpc: Option<(&RpcClient, &str)>,
) -> anyhow::Result<Value> {
    let signed = match invoice::parse_signed(document) {
        Ok(signed) => signed,
        Err(error) => {
            return Ok(json!({
                "kind": "invoice",
                "valid_signature": false,
                "verified": false,
                "error": error.to_string(),
            }));
        }
    };
    let bill = &signed.invoice;
    let expired = invoice::is_expired(&bill.expires_at);
    let hint = document
        .pointer("/payment/signature")
        .or_else(|| document.pointer("/payment/operation/signature"))
        .and_then(Value::as_str);

    let Some((rpc, cluster)) = rpc else {
        return Ok(json!({
            "kind": "invoice",
            "invoice_id": bill.invoice_id,
            "payee": bill.payee,
            "token": bill.token,
            "amount": bill.amount,
            "cluster": bill.cluster,
            "expires_at": bill.expires_at,
            "expired": expired,
            "valid_signature": true,
            "onchain_checked": false,
            "verified": true,
        }));
    };

    if cluster != bill.cluster {
        return Err(anyhow::anyhow!(
            "invoice is for cluster '{}', but verifier is connected to '{}'",
            bill.cluster,
            cluster
        ));
    }

    let payee = Pubkey::from_str(&bill.payee)?;
    let asset = tx::resolve_asset(rpc, &bill.token, cluster).await?;
    let units = parse_units(&bill.amount, asset.decimals)?;
    let found = invoice::find_payment(
        rpc,
        &ExpectedPayment {
            payee,
            invoice_id: &bill.invoice_id,
            units,
            mint: asset.mint,
            token_program: asset.token_program,
        },
        hint,
    )
    .await?;

    let (status, paid, finalized, signature, slot) = match found {
        Some(payment) => (
            if payment.finalized {
                "paid"
            } else {
                "confirming"
            },
            payment.finalized,
            payment.finalized,
            Some(payment.signature),
            Some(payment.slot),
        ),
        None => (
            if expired { "expired" } else { "open" },
            false,
            false,
            None,
            None,
        ),
    };

    Ok(json!({
        "kind": "invoice",
        "invoice_id": bill.invoice_id,
        "payee": bill.payee,
        "token": bill.token,
        "amount": bill.amount,
        "cluster": bill.cluster,
        "expires_at": bill.expires_at,
        "expired": expired,
        "valid_signature": true,
        "onchain_checked": true,
        "status": status,
        "paid": paid,
        "finalized": finalized,
        "payment_signature": signature,
        "slot": slot,
        "verified": paid,
    }))
}

async fn verify_receipt_v1(
    receipt: &Value,
    rpc: Option<(&RpcClient, &str)>,
) -> anyhow::Result<Value> {
    let receipt_id = receipt["receipt_id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| anyhow::anyhow!("manus.receipt.v1 is missing 'receipt_id'"))?;
    let resource_url = receipt
        .pointer("/resource/url")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("manus.receipt.v1 is missing 'resource.url'"))?;

    // Extract payment expectations from either agent-wallet x402 format (`payment`)
    // or daemon firewall format (`candidates[selected_candidate].payment`).
    let payment = if receipt["payment"].is_object() {
        Some(&receipt["payment"])
    } else if let Some(index) = receipt["selected_candidate"].as_u64() {
        receipt
            .pointer(&format!("/candidates/{index}/payment"))
            .filter(|v| v.is_object())
    } else {
        None
    };

    let (pay_to, asset_str, amount_atomic, memo) = match payment {
        Some(p) => {
            let pay_to = p["pay_to"]
                .as_str()
                .or_else(|| p["payTo"].as_str())
                .ok_or_else(|| anyhow::anyhow!("receipt payment is missing pay_to"))?;
            let asset = p["asset"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("receipt payment is missing asset"))?;
            let amount_raw = p["amount_atomic"]
                .as_str()
                .or_else(|| p["amount"].as_str())
                .and_then(|s| s.parse::<u64>().ok())
                .or_else(|| p["amount_atomic"].as_u64())
                .ok_or_else(|| anyhow::anyhow!("receipt payment has invalid atomic amount"))?;
            let memo = p["memo"].as_str();
            (
                Pubkey::from_str(pay_to)
                    .map_err(|_| anyhow::anyhow!("receipt pay_to is not a Solana address"))?,
                asset.to_string(),
                amount_raw,
                memo.map(str::to_string),
            )
        }
        None => {
            return Err(anyhow::anyhow!(
                "manus.receipt.v1 has no selected payment candidate to verify"
            ))
        }
    };

    let signature = receipt
        .pointer("/execution/signature")
        .and_then(Value::as_str);
    let execution_status = receipt
        .pointer("/execution/status")
        .and_then(Value::as_str)
        .unwrap_or("unknown");

    let Some((rpc, cluster)) = rpc else {
        return Ok(json!({
            "kind": RECEIPT_SCHEMA,
            "receipt_id": receipt_id,
            "resource_url": resource_url,
            "pay_to": pay_to.to_string(),
            "asset": asset_str,
            "amount_atomic": amount_atomic.to_string(),
            "execution_status": execution_status,
            "signature": signature,
            "structure_valid": true,
            "onchain_checked": false,
            "verified": signature.is_some(),
        }));
    };

    let Some(signature) = signature else {
        return Ok(json!({
            "kind": RECEIPT_SCHEMA,
            "receipt_id": receipt_id,
            "resource_url": resource_url,
            "pay_to": pay_to.to_string(),
            "asset": asset_str,
            "amount_atomic": amount_atomic.to_string(),
            "execution_status": execution_status,
            "signature": Value::Null,
            "structure_valid": true,
            "onchain_checked": true,
            "verified": false,
            "reason": "receipt has no transaction signature (inspection or unexecuted receipt)",
        }));
    };

    let asset = tx::resolve_asset(rpc, &asset_str, cluster).await?;
    let onchain = verify_onchain_transfer(
        rpc,
        signature,
        &pay_to,
        amount_atomic,
        asset.mint,
        asset.token_program,
        memo.as_deref(),
    )
    .await?;

    Ok(json!({
        "kind": RECEIPT_SCHEMA,
        "receipt_id": receipt_id,
        "resource_url": resource_url,
        "pay_to": pay_to.to_string(),
        "asset": asset_str,
        "symbol": asset.symbol,
        "amount_atomic": amount_atomic.to_string(),
        "signature": signature,
        "structure_valid": true,
        "onchain_checked": true,
        "onchain": onchain,
        "verified": onchain["matches_effects"].as_bool() == Some(true)
            && onchain["finalized"].as_bool() == Some(true),
    }))
}

async fn verify_operation(
    operation: &Value,
    rpc: Option<(&RpcClient, &str)>,
) -> anyhow::Result<Value> {
    let id = operation["id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("operation is missing 'id'"))?;
    let op_kind = operation["kind"].as_str().unwrap_or("unknown");
    let status = operation["status"].as_str().unwrap_or("unknown");
    let signature = operation["signature"].as_str();
    let exposure = &operation["exposure"];

    let Some((rpc, cluster)) = rpc else {
        return Ok(json!({
            "kind": "operation",
            "id": id,
            "operation_kind": op_kind,
            "status": status,
            "signature": signature,
            "structure_valid": true,
            "onchain_checked": false,
            "verified": signature.is_some() && matches!(status, "confirmed" | "finalized"),
        }));
    };

    let Some(signature) = signature else {
        return Ok(json!({
            "kind": "operation",
            "id": id,
            "operation_kind": op_kind,
            "status": status,
            "signature": Value::Null,
            "structure_valid": true,
            "onchain_checked": true,
            "verified": false,
            "reason": "operation has no transaction signature",
        }));
    };

    if let Some(recipient_str) = exposure["recipient"].as_str() {
        let recipient = Pubkey::from_str(recipient_str)?;
        let (amount_atomic, mint, token_program) = if let Some(token_out) =
            exposure.get("token_out").filter(|v| v.is_object())
        {
            let mint_str = token_out["mint"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("exposure.token_out is missing 'mint'"))?;
            let units = token_out["base_units"]
                .as_u64()
                .or_else(|| token_out.get("1").and_then(Value::as_u64))
                .ok_or_else(|| anyhow::anyhow!("exposure.token_out is missing 'base_units'"))?;
            let asset = tx::resolve_asset(rpc, mint_str, cluster).await?;
            (units, asset.mint, asset.token_program)
        } else if let Some(token_tuple) = exposure.get("token_out").and_then(Value::as_array) {
            let mint_str = token_tuple
                .first()
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("invalid token_out tuple"))?;
            let units = token_tuple
                .get(1)
                .and_then(Value::as_u64)
                .ok_or_else(|| anyhow::anyhow!("invalid token_out amount"))?;
            let asset = tx::resolve_asset(rpc, mint_str, cluster).await?;
            (units, asset.mint, asset.token_program)
        } else {
            let sol_out = match &exposure["sol_out"] {
                Value::String(s) => parse_units(s, 9)?,
                Value::Number(n) => n.as_u64().unwrap_or(0),
                _ => 0,
            };
            (sol_out, None, None)
        };

        let onchain = verify_onchain_transfer(
            rpc,
            signature,
            &recipient,
            amount_atomic,
            mint,
            token_program,
            None,
        )
        .await?;

        return Ok(json!({
            "kind": "operation",
            "id": id,
            "operation_kind": op_kind,
            "status": status,
            "signature": signature,
            "recipient": recipient.to_string(),
            "amount_atomic": amount_atomic.to_string(),
            "structure_valid": true,
            "onchain_checked": true,
            "onchain": onchain,
            "verified": onchain["matches_effects"].as_bool() == Some(true)
                && onchain["finalized"].as_bool() == Some(true),
        }));
    }

    // Conversion operation (swap/stake/unstake): verify transaction landed without error.
    let onchain = verify_transaction_landed(rpc, signature).await?;
    Ok(json!({
        "kind": "operation",
        "id": id,
        "operation_kind": op_kind,
        "status": status,
        "signature": signature,
        "structure_valid": true,
        "onchain_checked": true,
        "onchain": onchain,
        "verified": onchain["succeeded"].as_bool() == Some(true)
            && onchain["finalized"].as_bool() == Some(true),
    }))
}

async fn verify_onchain_transfer(
    rpc: &RpcClient,
    signature: &str,
    recipient: &Pubkey,
    units: u64,
    mint: Option<Pubkey>,
    token_program: Option<Pubkey>,
    expected_memo: Option<&str>,
) -> anyhow::Result<Value> {
    let sig = Signature::from_str(signature)
        .map_err(|_| anyhow::anyhow!("'{signature}' is not a valid Solana signature"))?;
    let (fetched, finalized) = match rpc
        .get_transaction(&sig, UiTransactionEncoding::Base64)
        .await
    {
        Ok(tx) => (tx, true),
        Err(_) => match rpc
            .get_transaction_with_config(
                &sig,
                RpcTransactionConfig {
                    encoding: Some(UiTransactionEncoding::Base64),
                    commitment: Some(CommitmentConfig::confirmed()),
                    max_supported_transaction_version: Some(0),
                },
            )
            .await
        {
            Ok(tx) => (tx, false),
            Err(error) => {
                return Ok(json!({
                    "found": false,
                    "finalized": false,
                    "succeeded": false,
                    "matches_effects": false,
                    "error": error.to_string(),
                }))
            }
        },
    };

    let Some(tx) = fetched.transaction.transaction.decode() else {
        return Err(anyhow::anyhow!("cannot decode transaction {signature}"));
    };
    let Some(meta) = fetched.transaction.meta.as_ref() else {
        return Err(anyhow::anyhow!("missing transaction metadata for {signature}"));
    };
    let succeeded = meta.err.is_none();
    let memo_ok = expected_memo.is_none_or(|memo| invoice::has_memo(&tx, memo));
    let transfer_ok = if !succeeded || !memo_ok {
        false
    } else {
        match (mint, token_program) {
            (None, _) => invoice::sol_paid(
                &tx,
                &meta.pre_balances,
                &meta.post_balances,
                recipient,
                units,
            ),
            (Some(mint), Some(program)) => {
                let deltas = invoice::token_deltas(meta);
                invoice::token_paid(&tx, &deltas, recipient, &mint, &program, units)
            }
            (Some(_), None) => false,
        }
    };

    Ok(json!({
        "found": true,
        "slot": fetched.slot,
        "finalized": finalized,
        "succeeded": succeeded,
        "memo_matched": memo_ok,
        "matches_effects": transfer_ok,
    }))
}

async fn verify_transaction_landed(rpc: &RpcClient, signature: &str) -> anyhow::Result<Value> {
    let sig = Signature::from_str(signature)
        .map_err(|_| anyhow::anyhow!("'{signature}' is not a valid Solana signature"))?;
    let (fetched, finalized) = match rpc
        .get_transaction(&sig, UiTransactionEncoding::Base64)
        .await
    {
        Ok(tx) => (tx, true),
        Err(_) => match rpc
            .get_transaction_with_config(
                &sig,
                RpcTransactionConfig {
                    encoding: Some(UiTransactionEncoding::Base64),
                    commitment: Some(CommitmentConfig::confirmed()),
                    max_supported_transaction_version: Some(0),
                },
            )
            .await
        {
            Ok(tx) => (tx, false),
            Err(error) => {
                return Ok(json!({
                    "found": false,
                    "finalized": false,
                    "succeeded": false,
                    "error": error.to_string(),
                }))
            }
        },
    };
    let succeeded = fetched
        .transaction
        .meta
        .as_ref()
        .is_some_and(|meta| meta.err.is_none());
    Ok(json!({
        "found": true,
        "slot": fetched.slot,
        "finalized": finalized,
        "succeeded": succeeded,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::signature::{Keypair, Signer};

    #[tokio::test]
    async fn verifies_signed_invoice_offline_and_rejects_tampering() {
        let payee = Keypair::new();
        let bill = invoice::Invoice {
            invoice_id: "inv_verify_01".into(),
            payee: payee.pubkey().to_string(),
            token: "USDC".into(),
            amount: "2.5".into(),
            description: "verify test".into(),
            expires_at: "2099-01-01T00:00:00Z".into(),
            cluster: "devnet".into(),
        };
        let signed = invoice::sign(&bill, &payee).unwrap();
        let doc = serde_json::to_value(&signed).unwrap();

        let report = verify_document(&doc, None).await.unwrap();
        assert_eq!(report["kind"], "invoice");
        assert_eq!(report["valid_signature"], true);
        assert_eq!(report["verified"], true);

        let mut tampered = doc.clone();
        tampered["invoice"]["amount"] = json!("25.0");
        let bad = verify_document(&tampered, None).await.unwrap();
        assert_eq!(bad["valid_signature"], false);
        assert_eq!(bad["verified"], false);
    }

    #[tokio::test]
    async fn verifies_manus_receipt_v1_structure_offline() {
        let pay_to = Pubkey::new_unique();
        let receipt = json!({
            "schema_version": "manus.receipt.v1",
            "receipt_id": "mrcpt_op_123",
            "created_at": "2026-10-03T00:00:00Z",
            "operation": "x402.payment.settle",
            "cluster": "devnet",
            "resource": { "url": "https://api.example.com/v1/query" },
            "payment": {
                "scheme": "exact",
                "network": "solana:EtWTRABZaYq6iMfeYKouRu166VU2xqa1",
                "pay_to": pay_to.to_string(),
                "asset": "SOL",
                "amount_atomic": "1000000",
                "memo": "x402:api.example.com:req1"
            },
            "execution": {
                "status": "finalized",
                "signature": Keypair::new().sign_message(b"tx").to_string(),
                "slot": 42
            }
        });
        let report = verify_document(&receipt, None).await.unwrap();
        assert_eq!(report["kind"], "manus.receipt.v1");
        assert_eq!(report["structure_valid"], true);
        assert_eq!(report["verified"], true);
    }

    #[tokio::test]
    async fn verifies_manus_receipt_v1_and_operation_against_onchain_rpc() {
        use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
        use solana_client::rpc_request::RpcRequest;
        use solana_sdk::{
            hash::Hash,
            message::{Message, VersionedMessage},
            transaction::VersionedTransaction,
        };

        let payer = Keypair::new();
        let payee = Keypair::new().pubkey();
        let memo = "x402:api.example.com:x402_pay_req_42";
        let instructions =
            tx::sol_transfer_instructions(&payer.pubkey(), &payee, 250_000, Some(memo));
        let message =
            Message::new_with_blockhash(&instructions, Some(&payer.pubkey()), &Hash::default());
        let signed_tx =
            VersionedTransaction::try_new(VersionedMessage::Legacy(message), &[&payer]).unwrap();
        let sig = signed_tx.signatures[0].to_string();
        let wire_b64 = B64.encode(bincode::serialize(&signed_tx).unwrap());

        let keys = signed_tx.message.static_account_keys();
        let mut pre = vec![1_000_000u64; keys.len()];
        let mut post = pre.clone();
        let payer_idx = keys.iter().position(|k| *k == payer.pubkey()).unwrap();
        let payee_idx = keys.iter().position(|k| *k == payee).unwrap();
        pre[payer_idx] = 10_000_000;
        post[payer_idx] = 10_000_000 - 250_000 - 5_000;
        pre[payee_idx] = 0;
        post[payee_idx] = 250_000;

        let rpc_tx_json = json!({
            "slot": 999,
            "transaction": [wire_b64, "base64"],
            "meta": {
                "err": null,
                "status": { "Ok": null },
                "fee": 5000,
                "preBalances": pre,
                "postBalances": post,
                "innerInstructions": [],
                "logMessages": [],
                "preTokenBalances": [],
                "postTokenBalances": [],
                "rewards": []
            },
            "blockTime": 1700000000,
            "version": "legacy"
        });
        let parsed_check: solana_transaction_status::EncodedConfirmedTransactionWithStatusMeta =
            serde_json::from_value(rpc_tx_json.clone()).expect("valid EncodedConfirmedTransactionWithStatusMeta");
        assert!(parsed_check.transaction.transaction.decode().is_some(), "decoded tx: {:?}", parsed_check.transaction.transaction);

        let rpc = RpcClient::new_mock_with_mocks(
            "http://mock.local".into(),
            std::collections::HashMap::from([(RpcRequest::GetTransaction, rpc_tx_json.clone())]),
        );

        let receipt = json!({
            "schema_version": "manus.receipt.v1",
            "receipt_id": "mrcpt_op_x402_1",
            "created_at": "2026-10-03T00:00:00Z",
            "operation": "x402.payment.settle",
            "cluster": "devnet",
            "resource": { "url": "https://api.example.com/v1/report" },
            "payment": {
                "scheme": "exact",
                "network": "solana:EtWTRABZaYq6iMfeYKouRu166VU2xqa1",
                "pay_to": payee.to_string(),
                "asset": "SOL",
                "amount_atomic": "250000",
                "memo": memo
            },
            "execution": {
                "status": "finalized",
                "signature": sig,
                "slot": 999
            }
        });

        let verified = verify_document(&receipt, Some((&rpc, "devnet")))
            .await
            .unwrap();
        assert_eq!(verified["verified"], true, "{verified}");
        assert_eq!(verified["onchain"]["matches_effects"], true);

        // If the receipt claims 500_000 lamports instead of the 250_000 actually paid on-chain,
        // verification must fail!
        let mut forged = receipt.clone();
        forged["payment"]["amount_atomic"] = json!("500000");
        let rpc2 = RpcClient::new_mock_with_mocks(
            "http://mock.local".into(),
            std::collections::HashMap::from([(RpcRequest::GetTransaction, rpc_tx_json.clone())]),
        );
        let rejected = verify_document(&forged, Some((&rpc2, "devnet")))
            .await
            .unwrap();
        assert_eq!(rejected["verified"], false, "{rejected}");
        assert_eq!(rejected["onchain"]["matches_effects"], false);

        // Verify an operation receipt envelope against the same on-chain transaction.
        let op_doc = json!({
            "operation": {
                "id": "op_test_99",
                "kind": "x402",
                "status": "finalized",
                "signature": sig,
                "exposure": {
                    "category": "send",
                    "sol_out": "0.00025",
                    "fees_sol": "0.000005",
                    "token_out": null,
                    "recipient": payee.to_string()
                }
            }
        });
        let rpc3 = RpcClient::new_mock_with_mocks(
            "http://mock.local".into(),
            std::collections::HashMap::from([(RpcRequest::GetTransaction, rpc_tx_json)]),
        );
        let op_verified = verify_document(&op_doc, Some((&rpc3, "devnet")))
            .await
            .unwrap();
        assert_eq!(op_verified["verified"], true, "{op_verified}");
    }
}
