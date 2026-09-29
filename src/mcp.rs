//! A small MCP server over stdio: the agent wallet's whole surface is twelve tools.

use serde_json::{json, Value};
use std::sync::Arc;

use super::budget::{parse_units, Budget, Limits};
use super::engine::{AgentWallet, Request};
use super::tx;

const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

const INSTRUCTIONS: &str = "Manus is your own Solana wallet with a spending budget set by your human. \
Inside the budget, send/swap/stake/unstake execute immediately. Above it, the human is asked to approve \
with Touch ID on their Mac; wait for the tool result, never try to approve yourself. \
Every operation is simulated first and judged by real balance changes. \
Call wallet_status before spending, pass dry_run=true to preview, and always pass a stable request_id \
for payments so a retry can never pay twice. Amounts are decimal strings in whole tokens (\"0.25\" SOL). \
Transfers are free; swap, stake and unstake include a 0.1% Manus fee that dry_run shows in the summary. \
To bill another agent, create_invoice and give them the returned object. To pay a bill, pay_invoice. \
The first payment to a new agent asks the human and then trusts that address inside the budget. \
invoice_status reads the chain. Invoice payments are ordinary transfers and carry no Manus fee.";

pub async fn run_stdio(wallet: Arc<AgentWallet>) -> anyhow::Result<()> {
    use tokio::io::{stdin, AsyncBufReadExt, AsyncWriteExt, BufReader};

    // Each request runs on its own task so a call waiting for Touch ID never blocks
    // status queries or in-budget payments; one writer keeps responses whole.
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<String>();
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(mut text) = receiver.recv().await {
            text.push('\n');
            if stdout.write_all(text.as_bytes()).await.is_err() || stdout.flush().await.is_err() {
                break;
            }
        }
    });

    let mut lines = BufReader::new(stdin()).lines();
    let mut tasks = tokio::task::JoinSet::new();
    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(request) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if request.get("id").is_none() {
            continue;
        }
        let wallet = wallet.clone();
        let sender = sender.clone();
        tasks.spawn(async move {
            let response = handle(&wallet, &request).await;
            if let Ok(text) = serde_json::to_string(&response) {
                let _ = sender.send(text);
            }
        });
    }
    // Input closed: let in-flight operations finish and report before exiting.
    while tasks.join_next().await.is_some() {}
    drop(sender);
    let _ = writer.await;
    Ok(())
}

async fn handle(wallet: &AgentWallet, request: &Value) -> Value {
    let id = request["id"].clone();
    match request["method"].as_str().unwrap_or("") {
        "initialize" => {
            let requested = request["params"]["protocolVersion"].as_str().unwrap_or("");
            let version = PROTOCOL_VERSIONS
                .iter()
                .find(|v| **v == requested)
                .copied()
                .unwrap_or(PROTOCOL_VERSIONS[PROTOCOL_VERSIONS.len() - 1]);
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": version,
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "manus", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": INSTRUCTIONS
                }
            })
        }
        "ping" => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
        "tools/list" => json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": tools() } }),
        "tools/call" => {
            let name = request["params"]["name"].as_str().unwrap_or("");
            let args = &request["params"]["arguments"];
            let (text, is_error) = match call(wallet, name, args).await {
                Ok(value) => (
                    serde_json::to_string_pretty(&value).unwrap_or_default(),
                    false,
                ),
                Err(error) => (format!("{error:#}"), true),
            };
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "content": [{ "type": "text", "text": text }], "isError": is_error }
            })
        }
        method => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32601, "message": format!("Method not found: {method}") }
        }),
    }
}

pub async fn call(wallet: &AgentWallet, name: &str, args: &Value) -> anyhow::Result<Value> {
    let text = |field: &str| args[field].as_str().map(str::to_string);
    let required = |field: &str| {
        text(field).ok_or_else(|| anyhow::anyhow!("missing required argument '{field}'"))
    };
    let request_id = text("request_id");
    let dry_run = args["dry_run"].as_bool().unwrap_or(false);
    match name {
        "wallet_status" => wallet.status().await,
        "balances" => wallet.balances().await,
        "send" => {
            let request = Request::Send {
                to: required("to")?,
                amount: amount(args)?,
                token: text("token").unwrap_or_else(|| "SOL".into()),
                memo: text("memo"),
            };
            wallet.execute(request, request_id, dry_run).await
        }
        "swap" => {
            let request = Request::Swap {
                from: required("from")?,
                to: required("to")?,
                amount: amount(args)?,
                slippage_bps: args["slippage_bps"].as_u64().map(|v| v.min(10_000) as u16),
            };
            wallet.execute(request, request_id, dry_run).await
        }
        "stake" => {
            let request = Request::Stake {
                amount: amount(args)?,
            };
            wallet.execute(request, request_id, dry_run).await
        }
        "unstake" => {
            let request = Request::Unstake {
                amount: amount(args)?,
            };
            wallet.execute(request, request_id, dry_run).await
        }
        "history" => wallet.history(args["limit"].as_i64().unwrap_or(20)).await,
        "operation" => wallet.operation(&required("id")?).await,
        "create_invoice" => {
            wallet
                .create_invoice(
                    text("token").as_deref().unwrap_or("SOL"),
                    &amount(args)?,
                    &required("description")?,
                    args["expires_in_hours"].as_u64().unwrap_or(168),
                )
                .await
        }
        "pay_invoice" => {
            let document = invoice_document(args)?;
            wallet.pay_invoice(&document, dry_run).await
        }
        "invoice_status" => wallet.invoice_status(&required("invoice_id")?).await,
        "change_budget" => {
            let proposed = apply_budget_changes(wallet, args).await?;
            let reason = text("reason").unwrap_or_else(|| "agent requested a budget change".into());
            wallet.change_budget(proposed, &reason).await
        }
        other => Err(anyhow::anyhow!("unknown tool '{other}'")),
    }
}

/// `invoice` may be the object `create_invoice` returned, or that object as a JSON string.
fn invoice_document(args: &Value) -> anyhow::Result<Value> {
    match &args["invoice"] {
        Value::Object(_) => Ok(args["invoice"].clone()),
        Value::String(text) => serde_json::from_str(text)
            .map_err(|error| anyhow::anyhow!("invoice must be the signed JSON object: {error}")),
        _ => Err(anyhow::anyhow!("missing required argument 'invoice'")),
    }
}

/// Accept amounts as strings or JSON numbers, normalising to a decimal string.
fn amount(args: &Value) -> anyhow::Result<String> {
    match &args["amount"] {
        Value::String(text) => Ok(text.clone()),
        Value::Number(number) => Ok(number.to_string()),
        _ => Err(anyhow::anyhow!("missing required argument 'amount'")),
    }
}

async fn apply_budget_changes(wallet: &AgentWallet, args: &Value) -> anyhow::Result<Budget> {
    let mut budget = wallet.budget().await?;
    let limits = |value: &Value, decimals: u8, current: Limits| -> anyhow::Result<Limits> {
        let field = |name: &str, fallback: u64| -> anyhow::Result<u64> {
            match &value[name] {
                Value::String(text) => parse_units(text, decimals),
                Value::Number(number) => parse_units(&number.to_string(), decimals),
                Value::Null => Ok(fallback),
                _ => Err(anyhow::anyhow!("{name} must be a decimal amount")),
            }
        };
        Ok(Limits {
            per_op: field("per_op", current.per_op)?,
            daily: field("daily", current.daily)?,
        })
    };
    if args["send_sol"].is_object() {
        budget.send_sol = limits(&args["send_sol"], 9, budget.send_sol)?;
    }
    if args["convert_sol"].is_object() {
        budget.convert_sol = limits(&args["convert_sol"], 9, budget.convert_sol)?;
    }
    if let Some(tokens) = args["token_limits"].as_array() {
        for entry in tokens {
            let token = entry["token"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("token_limits entries need 'token'"))?;
            let asset = tx::resolve_asset(&wallet.rpc, token, &wallet.cluster).await?;
            let mint = asset
                .mint
                .ok_or_else(|| anyhow::anyhow!("SOL limits live in send_sol"))?
                .to_string();
            let current = budget.send_tokens.get(&mint).copied().unwrap_or(Limits {
                per_op: 0,
                daily: 0,
            });
            budget
                .send_tokens
                .insert(mint, limits(entry, asset.decimals, current)?);
        }
    }
    if let Some(add) = args["trusted_recipients_add"].as_array() {
        for recipient in add.iter().filter_map(Value::as_str) {
            solana_sdk::pubkey::Pubkey::try_from(recipient)
                .map_err(|_| anyhow::anyhow!("'{recipient}' is not a Solana address"))?;
            if !budget.trusted_recipients.iter().any(|r| r == recipient) {
                budget.trusted_recipients.push(recipient.to_string());
            }
        }
    }
    if let Some(remove) = args["trusted_recipients_remove"].as_array() {
        let remove: Vec<&str> = remove.iter().filter_map(Value::as_str).collect();
        budget
            .trusted_recipients
            .retain(|r| !remove.contains(&r.as_str()));
    }
    if let Some(flag) = args["require_trusted_recipients"].as_bool() {
        budget.require_trusted_recipients = flag;
    }
    if let Some(bps) = args["max_slippage_bps"].as_u64() {
        budget.max_slippage_bps = bps.min(10_000) as u16;
    }
    Ok(budget)
}

fn tools() -> Value {
    let request_id = json!({
        "type": "string",
        "description": "Stable idempotency key for this payment (4-128 chars). Reusing it never pays twice."
    });
    let dry_run = json!({
        "type": "boolean",
        "description": "Preview effects and the budget decision without sending anything."
    });
    let amount = json!({
        "type": "string",
        "description": "Decimal amount in whole tokens, e.g. \"0.25\"."
    });
    let limits = json!({
        "type": "object",
        "properties": {
            "per_op": { "type": "string" },
            "daily": { "type": "string" }
        }
    });
    json!([
        {
            "name": "wallet_status",
            "description": "Your wallet address, balances, budget, what you spent in the last 24h, and how approvals work. Call this before spending.",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "balances",
            "description": "SOL and token balances of your wallet.",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "send",
            "description": "Send SOL or an SPL token (USDC, USDT, JitoSOL, mSOL or a mint address) to a Solana address.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "to": { "type": "string", "description": "Recipient Solana address." },
                    "amount": amount,
                    "token": { "type": "string", "description": "SOL (default), USDC, USDT, JitoSOL, mSOL, or a mint address." },
                    "memo": { "type": "string", "description": "Optional on-chain memo, up to 200 bytes." },
                    "request_id": request_id,
                    "dry_run": dry_run
                },
                "required": ["to", "amount"]
            },
            "annotations": { "destructiveHint": true }
        },
        {
            "name": "swap",
            "description": "Swap tokens through Jupiter (mainnet).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "from": { "type": "string", "description": "Token to sell: SOL, USDC, a symbol or mint." },
                    "to": { "type": "string", "description": "Token to buy." },
                    "amount": amount,
                    "slippage_bps": { "type": "integer", "description": "Default 50 (0.5%)." },
                    "request_id": request_id,
                    "dry_run": dry_run
                },
                "required": ["from", "to", "amount"]
            },
            "annotations": { "destructiveHint": true }
        },
        {
            "name": "stake",
            "description": "Liquid-stake SOL into JitoSOL, which earns staking yield and can be unstaked any time (mainnet).",
            "inputSchema": {
                "type": "object",
                "properties": { "amount": amount, "request_id": request_id, "dry_run": dry_run },
                "required": ["amount"]
            }
        },
        {
            "name": "unstake",
            "description": "Convert JitoSOL back to SOL instantly through the market. amount is in JitoSOL, or \"all\".",
            "inputSchema": {
                "type": "object",
                "properties": { "amount": amount, "request_id": request_id, "dry_run": dry_run },
                "required": ["amount"]
            }
        },
        {
            "name": "history",
            "description": "Recent operations with status, decision, approval and explorer link.",
            "inputSchema": {
                "type": "object",
                "properties": { "limit": { "type": "integer", "description": "Default 20." } }
            },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "operation",
            "description": "Refresh and return one operation (receipt) by id.",
            "inputSchema": {
                "type": "object",
                "properties": { "id": { "type": "string" } },
                "required": ["id"]
            }
        },
        {
            "name": "create_invoice",
            "description": "Sign a bill from this wallet. Give the returned object to the agent who should pay it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "amount": amount,
                    "token": { "type": "string", "description": "SOL (default), USDC, USDT, JitoSOL, mSOL, or a mint address." },
                    "description": { "type": "string", "description": "What this bill is for, one line, up to 160 characters." },
                    "expires_in_hours": { "type": "integer", "description": "1-720. Default 168 (7 days)." }
                },
                "required": ["amount", "description"]
            }
        },
        {
            "name": "pay_invoice",
            "description": "Pay a signed invoice with an ordinary transfer. The invoice id is the memo and the idempotency key, so this wallet cannot pay it twice. The first payment to a new agent asks the human; approving also trusts that address for later payments inside the budget.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "invoice": { "type": "object", "description": "The object returned by the payee's create_invoice." },
                    "dry_run": dry_run
                },
                "required": ["invoice"]
            },
            "annotations": { "destructiveHint": true }
        },
        {
            "name": "invoice_status",
            "description": "See whether an invoice this wallet issued or accepted has been paid. Paid means a finalized transfer of the exact amount with the invoice id as the memo.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "invoice_id": { "type": "string" }
                },
                "required": ["invoice_id"]
            },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "change_budget",
            "description": "Change your budget. Tightening applies immediately; anything that widens it asks the human to approve with Touch ID. Explain why in 'reason'.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "reason": { "type": "string" },
                    "send_sol": limits,
                    "convert_sol": limits,
                    "token_limits": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "token": { "type": "string" },
                                "per_op": { "type": "string" },
                                "daily": { "type": "string" }
                            },
                            "required": ["token"]
                        }
                    },
                    "trusted_recipients_add": { "type": "array", "items": { "type": "string" } },
                    "trusted_recipients_remove": { "type": "array", "items": { "type": "string" } },
                    "require_trusted_recipients": { "type": "boolean" },
                    "max_slippage_bps": { "type": "integer" }
                },
                "required": ["reason"]
            }
        }
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_surface_is_small_and_well_formed() {
        let tools = tools();
        let tools = tools.as_array().unwrap();
        assert_eq!(tools.len(), 12);
        for tool in tools {
            assert!(tool["name"].is_string());
            assert!(tool["description"].as_str().unwrap().len() > 10);
            assert_eq!(tool["inputSchema"]["type"], "object");
        }
    }

    #[test]
    fn amounts_accept_strings_and_numbers() {
        assert_eq!(amount(&json!({"amount": "0.5"})).unwrap(), "0.5");
        assert_eq!(amount(&json!({"amount": 2})).unwrap(), "2");
        assert!(amount(&json!({})).is_err());
    }
}
