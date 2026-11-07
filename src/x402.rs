//! x402 v2 payment challenges for the agent wallet.
//!
//! When an agent calls a paid HTTP endpoint and receives `402 Payment Required`,
//! the server returns an x402 v2 challenge (either as JSON or as a base64
//! `PAYMENT-REQUIRED` header). Manus inspects the challenge against the wallet's
//! active cluster, resolves the token and atomic amount, runs the transfer
//! through the normal simulation and budget/Touch ID gate, and emits both a
//! `manus.receipt.v1` evidence object and a base64 `payment_header` proof.

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

use crate::budget::format_units;
use crate::mints::SOL_MINT;

pub const RECEIPT_SCHEMA: &str = "manus.receipt.v1";
pub const SOLANA_MAINNET_CAIP2: &str = "solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp";
pub const SOLANA_DEVNET_CAIP2: &str = "solana:EtWTRABZaYq6iMfeYKouRu166VU2xqa1";
pub const SOLANA_LOCALNET_CAIP2: &str = "solana:localnet";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PaymentRequired {
    pub x402_version: u8,
    pub resource: Resource,
    pub accepts: Vec<Accept>,
    #[serde(default)]
    pub extensions: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Resource {
    pub url: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub mime_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Accept {
    pub scheme: String,
    pub network: String,
    pub amount: String,
    pub asset: String,
    pub pay_to: String,
    #[serde(default)]
    pub max_timeout_seconds: Option<u64>,
    #[serde(default)]
    pub extra: Option<Value>,
}

/// Normalized x402 payment requirement matched to the wallet's cluster.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolvedChallenge {
    pub resource_url: String,
    pub domain: String,
    pub description: Option<String>,
    pub network: String,
    pub payee: Pubkey,
    pub asset: String,
    pub amount_atomic: u64,
    pub max_timeout_seconds: Option<u64>,
    pub payment_id: Option<String>,
    pub accepted: Accept,
}

/// Decode a challenge from either a JSON object/string or a base64 `PAYMENT-REQUIRED` header.
pub fn decode_challenge(
    challenge: Option<&Value>,
    payment_required_header: Option<&str>,
) -> anyhow::Result<PaymentRequired> {
    let raw: Value = match (challenge, payment_required_header) {
        (Some(_), Some(_)) => {
            return Err(anyhow::anyhow!(
                "provide either 'challenge' or 'payment_required_header', not both"
            ))
        }
        (Some(Value::String(text)), None) => decode_text_or_base64(text)?,
        (Some(value), None) => value.clone(),
        (None, Some(header)) => decode_text_or_base64(header)?,
        (None, None) => {
            return Err(anyhow::anyhow!(
                "missing x402 input: provide 'challenge' or 'payment_required_header'"
            ))
        }
    };
    let parsed: PaymentRequired = serde_json::from_value(raw)
        .map_err(|error| anyhow::anyhow!("invalid x402 v2 challenge: {error}"))?;
    if parsed.x402_version != 2 {
        return Err(anyhow::anyhow!(
            "unsupported x402Version {}; Manus requires version 2",
            parsed.x402_version
        ));
    }
    if parsed.accepts.is_empty() {
        return Err(anyhow::anyhow!(
            "x402 challenge contains no payment requirements"
        ));
    }
    Ok(parsed)
}

fn decode_text_or_base64(input: &str) -> anyhow::Result<Value> {
    let trimmed = input.trim();
    if trimmed.starts_with('{') {
        return serde_json::from_str(trimmed)
            .map_err(|error| anyhow::anyhow!("invalid x402 challenge JSON: {error}"));
    }
    let bytes = B64
        .decode(trimmed)
        .map_err(|error| anyhow::anyhow!("invalid base64 payment-required header: {error}"))?;
    serde_json::from_slice(&bytes)
        .map_err(|error| anyhow::anyhow!("payment-required header is not valid JSON: {error}"))
}

/// Select and validate the payment option matching the wallet's cluster.
pub fn resolve_challenge(
    challenge: &PaymentRequired,
    cluster: &str,
) -> anyhow::Result<ResolvedChallenge> {
    let domain = parse_http_host(&challenge.resource.url)?;
    let candidate = challenge
        .accepts
        .iter()
        .find(|accept| accept.scheme == "exact" && network_matches_cluster(&accept.network, cluster))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no exact Solana payment option in x402 challenge matches wallet cluster '{cluster}'"
            )
        })?;

    let payee = Pubkey::from_str(candidate.pay_to.trim())
        .map_err(|_| anyhow::anyhow!("x402 payTo '{}' is not a Solana address", candidate.pay_to))?;
    let amount_atomic = candidate
        .amount
        .trim()
        .parse::<u64>()
        .ok()
        .filter(|amount| *amount > 0)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "x402 amount '{}' must be a positive integer in atomic units",
                candidate.amount
            )
        })?;

    let asset = normalize_asset(&candidate.asset)?;
    let payment_id = extract_payment_id(challenge.extensions.as_ref());

    Ok(ResolvedChallenge {
        resource_url: challenge.resource.url.clone(),
        domain,
        description: challenge.resource.description.clone(),
        network: candidate.network.clone(),
        payee,
        asset,
        amount_atomic,
        max_timeout_seconds: candidate.max_timeout_seconds,
        payment_id,
        accepted: candidate.clone(),
    })
}

fn normalize_asset(asset: &str) -> anyhow::Result<String> {
    let trimmed = asset.trim();
    if trimmed.eq_ignore_ascii_case("SOL")
        || trimmed.eq_ignore_ascii_case("native")
        || trimmed == SOL_MINT
    {
        return Ok("SOL".into());
    }
    Pubkey::from_str(trimmed)
        .map_err(|_| anyhow::anyhow!("x402 asset '{trimmed}' is not a valid Solana mint"))?;
    Ok(trimmed.to_string())
}

pub fn cluster_caip2(cluster: &str) -> &'static str {
    match cluster {
        "mainnet-beta" => SOLANA_MAINNET_CAIP2,
        "localnet" => SOLANA_LOCALNET_CAIP2,
        _ => SOLANA_DEVNET_CAIP2,
    }
}

pub fn network_matches_cluster(network: &str, cluster: &str) -> bool {
    let net = network.trim();
    match cluster {
        "mainnet-beta" => matches!(
            net,
            SOLANA_MAINNET_CAIP2 | "solana:mainnet" | "solana:mainnet-beta" | "mainnet-beta"
        ),
        "devnet" => matches!(net, SOLANA_DEVNET_CAIP2 | "solana:devnet" | "devnet"),
        "localnet" => matches!(
            net,
            SOLANA_LOCALNET_CAIP2
                | "localnet"
                | SOLANA_DEVNET_CAIP2
                | "solana:devnet"
        ),
        _ => false,
    }
}

pub fn parse_http_host(url: &str) -> anyhow::Result<String> {
    let trimmed = url.trim();
    let rest = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
        .ok_or_else(|| anyhow::anyhow!("x402 resource URL must start with https:// or http://"))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host_port = authority.rsplit('@').next().unwrap_or("");
    let host = if host_port.starts_with('[') {
        host_port
            .split(']')
            .next()
            .map(|h| format!("{h}]"))
            .unwrap_or_default()
    } else {
        host_port.split(':').next().unwrap_or("").to_string()
    };
    let host = host.trim().to_ascii_lowercase();
    if host.is_empty() || host.chars().any(char::is_whitespace) {
        return Err(anyhow::anyhow!("x402 resource URL has an invalid host"));
    }
    Ok(host)
}

fn extract_payment_id(extensions: Option<&Value>) -> Option<String> {
    let ext = extensions?;
    ext.get("payment-identifier")
        .and_then(|v| {
            v.as_str()
                .or_else(|| v.get("info").and_then(|info| info.get("id")).and_then(Value::as_str))
                .or_else(|| v.get("id").and_then(Value::as_str))
        })
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

/// Deterministic idempotency key and on-chain memo for an x402 payment.
pub fn memo_for(domain: &str, request_id: &str) -> String {
    let clean_domain: String = domain
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.'))
        .take(40)
        .collect();
    let clean_id: String = request_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '.'))
        .take(64)
        .collect();
    format!("x402:{clean_domain}:{clean_id}")
}

/// Default request_id when the caller did not supply one and the challenge has no payment-identifier.
pub fn default_request_id(resolved: &ResolvedChallenge) -> String {
    if let Some(id) = &resolved.payment_id {
        let sanitized: String = id
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '.'))
            .take(100)
            .collect();
        if sanitized.len() >= 4 {
            return format!("x402_{sanitized}");
        }
    }
    let digest = Sha256::digest(format!(
        "manus-x402-v2:{}:{}:{}:{}",
        resolved.resource_url, resolved.payee, resolved.asset, resolved.amount_atomic
    ));
    format!("x402_{}", &hex::encode(digest)[..24])
}

/// Build a portable `manus.receipt.v1` evidence document from an agent wallet x402 operation.
pub fn build_receipt(
    resolved: &ResolvedChallenge,
    symbol: &str,
    decimals: u8,
    cluster: &str,
    payer: &Pubkey,
    memo: &str,
    operation: &Value,
) -> Value {
    let op = &operation["operation"];
    let op_id = op["id"].as_str().unwrap_or("preview");
    let status = op["status"].as_str().unwrap_or("dry_run");
    let signature = op["signature"].as_str();
    let slot = op["slot"].as_i64();
    let approved_by = op["approved_by"].clone();
    let decision = match status {
        "denied" => "deny",
        "awaiting_approval" => "requires_approval",
        _ if !approved_by.is_null() => "allow",
        _ => "allow",
    };
    let created_at = op["created_at"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
    json!({
        "schema_version": RECEIPT_SCHEMA,
        "receipt_id": format!("mrcpt_{op_id}"),
        "created_at": created_at,
        "operation": if operation["dry_run"].as_bool() == Some(true) {
            "x402.payment.inspect"
        } else {
            "x402.payment.settle"
        },
        "cluster": cluster,
        "payer": payer.to_string(),
        "resource": {
            "url": resolved.resource_url,
            "domain": resolved.domain,
            "description": resolved.description,
        },
        "payment": {
            "scheme": "exact",
            "network": resolved.network,
            "pay_to": resolved.payee.to_string(),
            "asset": resolved.accepted.asset,
            "symbol": symbol,
            "decimals": decimals,
            "amount_atomic": resolved.amount_atomic.to_string(),
            "amount": format_units(resolved.amount_atomic, decimals),
            "memo": memo,
        },
        "decision": decision,
        "approved_by": approved_by,
        "execution": {
            "operation_id": op["id"],
            "request_id": op["request_id"],
            "status": status,
            "signature": signature,
            "slot": slot,
            "finalized": status == "finalized",
            "explorer": op["explorer"],
            "error": op["error"],
        }
    })
}

/// Encode the x402 v2 `PAYMENT-SIGNATURE` / `X-PAYMENT` header value once a payment is signed.
pub fn encode_payment_header(
    resolved: &ResolvedChallenge,
    payer: &Pubkey,
    signature: &str,
    slot: Option<i64>,
    receipt_id: &str,
) -> String {
    let payload = json!({
        "x402Version": 2,
        "resource": {
            "url": resolved.resource_url,
            "description": resolved.description,
        },
        "accepted": resolved.accepted,
        "payload": {
            "signature": signature,
            "payer": payer.to_string(),
            "slot": slot,
            "receiptId": receipt_id,
        }
    });
    B64.encode(serde_json::to_vec(&payload).unwrap_or_default())
}

/// Optional HTTP request parameters when the agent wants `pay_x402` to fetch the URL,
/// handle the 402 challenge, and retry automatically with the signed payment header.
#[derive(Debug, Clone, Default)]
pub struct HttpCall {
    pub url: String,
    pub method: Option<String>,
    pub headers: Option<Value>,
    pub body: Option<Value>,
}

pub enum ProbeOutcome {
    /// The endpoint returned a non-402 response without needing payment.
    NoPaymentNeeded(Value),
    /// The endpoint returned HTTP 402 with an x402 v2 challenge.
    PaymentRequired(PaymentRequired),
}

/// Call `http.url`; if it returns HTTP 402, extract and validate the x402 v2 challenge.
pub async fn probe_url(http: &HttpCall) -> anyhow::Result<ProbeOutcome> {
    let requested_host = parse_http_host(&http.url)?;
    let response = build_http_request(http, None)?.send().await?;
    let status = response.status().as_u16();
    let header_challenge = response
        .headers()
        .get("payment-required")
        .or_else(|| response.headers().get("x-payment-required"))
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let text = response.text().await.unwrap_or_default();
    let body_json: Value = serde_json::from_str(&text).unwrap_or(Value::String(text));

    if status != 402 {
        return Ok(ProbeOutcome::NoPaymentNeeded(json!({
            "status": status,
            "body": body_json,
        })));
    }

    let challenge = if let Some(header) = header_challenge {
        decode_challenge(None, Some(&header))?
    } else if body_json.is_object() {
        decode_challenge(Some(&body_json), None)?
    } else {
        return Err(anyhow::anyhow!(
            "HTTP 402 response from {} did not include a PAYMENT-REQUIRED header or JSON challenge",
            http.url
        ));
    };

    let resource_host = parse_http_host(&challenge.resource.url)?;
    if resource_host != requested_host {
        return Err(anyhow::anyhow!(
            "x402 challenge resource host '{resource_host}' does not match requested URL host '{requested_host}'"
        ));
    }
    Ok(ProbeOutcome::PaymentRequired(challenge))
}

/// Retry `http.url` with the signed `PAYMENT-SIGNATURE` and `X-PAYMENT` headers attached.
pub async fn retry_with_payment_header(
    http: &HttpCall,
    payment_header: &str,
) -> anyhow::Result<Value> {
    let response = build_http_request(http, Some(payment_header))?
        .send()
        .await?;
    let status = response.status().as_u16();
    let payment_response = response
        .headers()
        .get("payment-response")
        .or_else(|| response.headers().get("x-payment-response"))
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let text = response.text().await.unwrap_or_default();
    let body_json: Value = serde_json::from_str(&text).unwrap_or(Value::String(text));
    Ok(json!({
        "status": status,
        "ok": (200..300).contains(&status),
        "payment_response_header": payment_response,
        "body": body_json,
    }))
}

fn build_http_request(
    http: &HttpCall,
    payment_header: Option<&str>,
) -> anyhow::Result<reqwest::RequestBuilder> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let method_str = http
        .method
        .as_deref()
        .unwrap_or(if http.body.is_some() { "POST" } else { "GET" })
        .to_ascii_uppercase();
    let method = reqwest::Method::from_bytes(method_str.as_bytes())
        .map_err(|_| anyhow::anyhow!("unsupported HTTP method '{method_str}'"))?;
    let mut builder = client.request(method, http.url.trim());
    if let Some(Value::Object(map)) = &http.headers {
        for (k, v) in map {
            if let Some(val) = v.as_str() {
                builder = builder.header(k.as_str(), val);
            }
        }
    }
    if let Some(proof) = payment_header {
        builder = builder
            .header("PAYMENT-SIGNATURE", proof)
            .header("X-PAYMENT", proof);
    }
    if let Some(body) = &http.body {
        match body {
            Value::Null => {}
            Value::String(s) => {
                builder = builder.body(s.clone());
            }
            other => {
                builder = builder.json(other);
            }
        }
    }
    Ok(builder)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_challenge(pay_to: &Pubkey, network: &str, asset: &str) -> Value {
        json!({
            "x402Version": 2,
            "resource": {
                "url": "https://api.example.com/v1/report",
                "description": "Market risk report"
            },
            "accepts": [{
                "scheme": "exact",
                "network": network,
                "amount": "250000",
                "asset": asset,
                "payTo": pay_to.to_string(),
                "maxTimeoutSeconds": 120
            }],
            "extensions": {
                "payment-identifier": {
                    "info": { "id": "pay_req_42" }
                }
            }
        })
    }

    #[test]
    fn decodes_json_and_base64_challenges() {
        let payee = Pubkey::new_unique();
        let raw = sample_challenge(&payee, SOLANA_DEVNET_CAIP2, "4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU");
        let parsed = decode_challenge(Some(&raw), None).unwrap();
        assert_eq!(parsed.x402_version, 2);

        let encoded = B64.encode(serde_json::to_vec(&raw).unwrap());
        let from_header = decode_challenge(None, Some(&encoded)).unwrap();
        assert_eq!(parsed, from_header);
    }

    #[test]
    fn resolves_devnet_and_localnet_candidates_and_rejects_wrong_cluster() {
        let payee = Pubkey::new_unique();
        let raw = sample_challenge(&payee, SOLANA_DEVNET_CAIP2, "SOL");
        let challenge = decode_challenge(Some(&raw), None).unwrap();

        let resolved = resolve_challenge(&challenge, "devnet").unwrap();
        assert_eq!(resolved.domain, "api.example.com");
        assert_eq!(resolved.payee, payee);
        assert_eq!(resolved.amount_atomic, 250_000);
        assert_eq!(resolved.asset, "SOL");
        assert_eq!(resolved.payment_id.as_deref(), Some("pay_req_42"));

        // Localnet accepts devnet CAIP-2 challenges for local testing.
        assert!(resolve_challenge(&challenge, "localnet").is_ok());
        // Mainnet wallet refuses a devnet challenge.
        assert!(resolve_challenge(&challenge, "mainnet-beta").is_err());
    }

    #[tokio::test]
    async fn probes_http_402_and_retries_with_payment_header() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let payee = Pubkey::new_unique();
        let url = format!("http://127.0.0.1:{}/v1/paid", addr.port());
        let challenge_json = json!({
            "x402Version": 2,
            "resource": {
                "url": url,
                "description": "Paid endpoint"
            },
            "accepts": [{
                "scheme": "exact",
                "network": SOLANA_DEVNET_CAIP2,
                "amount": "100000",
                "asset": "SOL",
                "payTo": payee.to_string(),
                "maxTimeoutSeconds": 60
            }]
        });
        let challenge_header = B64.encode(serde_json::to_vec(&challenge_json).unwrap());

        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 4096];
                let n = stream.read(&mut buf).await.unwrap();
                let req = String::from_utf8_lossy(&buf[..n]);
                if req.to_ascii_lowercase().contains("payment-signature:") {
                    let body = r#"{"unlocked":true,"data":"secret-alpha"}"#;
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nPAYMENT-RESPONSE: settled\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    stream.write_all(resp.as_bytes()).await.unwrap();
                } else {
                    let body = r#"{"error":"payment required"}"#;
                    let resp = format!(
                        "HTTP/1.1 402 Payment Required\r\nContent-Type: application/json\r\nPAYMENT-REQUIRED: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        challenge_header,
                        body.len(),
                        body
                    );
                    stream.write_all(resp.as_bytes()).await.unwrap();
                }
            }
        });

        let call = HttpCall {
            url: url.clone(),
            method: Some("GET".into()),
            headers: None,
            body: None,
        };
        let outcome = probe_url(&call).await.unwrap();
        let parsed = match outcome {
            ProbeOutcome::PaymentRequired(ch) => ch,
            ProbeOutcome::NoPaymentNeeded(_) => panic!("expected 402 challenge"),
        };
        let resolved = resolve_challenge(&parsed, "devnet").unwrap();
        assert_eq!(resolved.payee, payee);
        assert_eq!(resolved.amount_atomic, 100_000);

        let retried = retry_with_payment_header(&call, "dGVzdC1wcm9vZg==").await.unwrap();
        assert_eq!(retried["status"], 200);
        assert_eq!(retried["ok"], true);
        assert_eq!(retried["payment_response_header"], "settled");
        assert_eq!(retried["body"]["unlocked"], true);
        server.await.unwrap();
    }
}
