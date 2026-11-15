//! A small MCP server over stdio: the agent wallet's whole surface is twenty tools.

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
for payments so a retry uses the same receipt. If a receipt says submission_unknown, check its \
signature before making a new payment with a different id. Amounts are decimal strings in whole tokens (\"0.25\" SOL). \
Transfers are free; swap, stake and unstake include a 0.1% Manus fee that dry_run shows in the summary. \
To bill another agent, create_invoice and give them the returned object. To pay a bill, pay_invoice. \
The first payment to a new agent asks the human and then trusts that address inside the budget. \
invoice_status reads the chain. Invoice payments are ordinary transfers and carry no Manus fee. \
For HTTP 402 (x402 v2) paid endpoints, call pay_x402 with the target 'url' (plus optional method/headers/body) \
to fetch, pay inside your budget, and automatically retry the request, or pass an existing 'challenge' or \
'payment_required_header'. Use verify to validate any signed invoice, manus.receipt.v1, or operation receipt on-chain. \
For crypto trading: call market_signals (AIPricePatterns regime, Kelly fraction, historical pattern analogs, and wallet_execution_plan), \
quote (live Jupiter quote, price impact, mint/freeze authority and Token-2022 safety inspection, and budget decision), \
and portfolio (holdings, average entry price, and realized/unrealized PnL in SOL) before executing swap. \
Use create_trigger, list_triggers, and cancel_trigger to attach autonomous Take-Profit / Stop-Loss / OCO exit rules.";

pub async fn run_stdio(wallet: Arc<AgentWallet>) -> anyhow::Result<()> {
    use tokio::io::{stdin, AsyncBufReadExt, AsyncWriteExt, BufReader};

    // Background watcher: automatically checks and executes active Take-Profit / Stop-Loss
    // triggers every 15 seconds while the MCP server is running.
    let watcher_wallet = wallet.clone();
    let watcher = tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(15));
        loop {
            interval.tick().await;
            let _ = watcher_wallet.evaluate_triggers().await;
        }
    });

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
    watcher.abort();
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
            wallet
                .swap_with_bracket(
                    &required("from")?,
                    &required("to")?,
                    &amount(args)?,
                    args["slippage_bps"].as_u64().map(|v| v.min(10_000) as u16),
                    args["take_profit_pct"].as_f64(),
                    args["stop_loss_pct"].as_f64(),
                    args["trailing_stop_bps"]
                        .as_u64()
                        .map(|v| v.min(9_999) as u16),
                    text("tag"),
                    request_id,
                    dry_run,
                )
                .await
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
        "pay_x402" => {
            let challenge = args.get("challenge").filter(|v| !v.is_null());
            let header = text("payment_required_header");
            let http = text("url").map(|url| crate::x402::HttpCall {
                url,
                method: text("method"),
                headers: args.get("headers").filter(|v| !v.is_null()).cloned(),
                body: args.get("body").filter(|v| !v.is_null()).cloned(),
            });
            wallet
                .pay_x402(challenge, header.as_deref(), http, request_id, dry_run)
                .await
        }
        "verify" => {
            let target = args
                .get("document")
                .or_else(|| args.get("target"))
                .filter(|v| !v.is_null())
                .ok_or_else(|| anyhow::anyhow!("missing required argument 'document'"))?;
            let offline = args["offline"].as_bool().unwrap_or(false);
            wallet.verify(target, offline).await
        }
        "quote" => {
            wallet
                .quote(
                    &required("from")?,
                    &required("to")?,
                    &amount(args)?,
                    args["slippage_bps"].as_u64().map(|v| v.min(10_000) as u16),
                    args["kelly_fraction"].as_f64(),
                )
                .await
        }
        "portfolio" => wallet.portfolio().await,
        "create_trigger" => {
            wallet
                .create_trigger(
                    &required("from")?,
                    &required("to")?,
                    &amount(args)?,
                    args["slippage_bps"].as_u64().map(|v| v.min(10_000) as u16),
                    args["take_profit_rate"].as_f64(),
                    args["stop_loss_rate"].as_f64(),
                    args["take_profit_pct"].as_f64(),
                    args["stop_loss_pct"].as_f64(),
                    args["trailing_stop_bps"]
                        .as_u64()
                        .map(|v| v.min(9_999) as u16),
                    text("tag"),
                    dry_run,
                )
                .await
        }
        "list_triggers" => {
            let active_only = args["active_only"].as_bool().unwrap_or(true);
            wallet.list_triggers(active_only).await
        }
        "cancel_trigger" => wallet.cancel_trigger(&required("id")?).await,
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
    if let Some(add) = args["allowed_tokens_add"].as_array() {
        for token in add.iter().filter_map(Value::as_str) {
            let trimmed = token.trim();
            if !trimmed.is_empty() && !budget.allowed_tokens.iter().any(|t| t == trimmed) {
                budget.allowed_tokens.push(trimmed.to_string());
            }
        }
    }
    if let Some(remove) = args["allowed_tokens_remove"].as_array() {
        let remove: Vec<&str> = remove.iter().filter_map(Value::as_str).collect();
        budget
            .allowed_tokens
            .retain(|t| !remove.contains(&t.as_str()));
    }
    if let Some(flag) = args["require_allowed_tokens"].as_bool() {
        budget.require_allowed_tokens = flag;
    }
    if let Some(secs) = args["swap_cooldown_secs"].as_u64() {
        budget.swap_cooldown_secs = secs;
    }
    match &args["max_daily_loss_sol"] {
        Value::String(text) if text.trim() == "0" => budget.max_daily_loss_sol = 0,
        Value::String(text) => budget.max_daily_loss_sol = parse_units(text, 9)?,
        Value::Number(num) if num.as_f64() == Some(0.0) => budget.max_daily_loss_sol = 0,
        Value::Number(num) => budget.max_daily_loss_sol = parse_units(&num.to_string(), 9)?,
        _ => {}
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
            "annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true }
        },
        {
            "name": "balances",
            "description": "SOL and token balances of your wallet.",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true }
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
            "annotations": { "readOnlyHint": false, "destructiveHint": true, "idempotentHint": false, "openWorldHint": true }
        },
        {
            "name": "swap",
            "description": "Swap tokens through Jupiter (mainnet). Optionally attach a one-call bracket exit trigger (take_profit_pct, stop_loss_pct, trailing_stop_bps) and strategy tag.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "from": { "type": "string", "description": "Token to sell: SOL, USDC, a symbol or mint." },
                    "to": { "type": "string", "description": "Token to buy." },
                    "amount": amount,
                    "slippage_bps": { "type": "integer", "description": "Default 50 (0.5%)." },
                    "take_profit_pct": { "type": "number", "description": "Optional auto-exit Take-Profit percentage above entry (e.g. 8.0 for +8%)." },
                    "stop_loss_pct": { "type": "number", "description": "Optional auto-exit Stop-Loss percentage below entry (e.g. 3.0 for -3%)." },
                    "trailing_stop_bps": { "type": "integer", "description": "Optional auto-exit Trailing Stop in bps below peak rate (e.g. 200 for 2%)." },
                    "tag": { "type": "string", "description": "Optional strategy or signal tag (e.g. \"BULL_TREND_15m\") for portfolio PnL and win-rate attribution." },
                    "request_id": request_id,
                    "dry_run": dry_run
                },
                "required": ["from", "to", "amount"]
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": true, "idempotentHint": false, "openWorldHint": true }
        },
        {
            "name": "stake",
            "description": "Liquid-stake SOL into JitoSOL, which earns staking yield and can be unstaked any time (mainnet).",
            "inputSchema": {
                "type": "object",
                "properties": { "amount": amount, "request_id": request_id, "dry_run": dry_run },
                "required": ["amount"]
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": true, "idempotentHint": false, "openWorldHint": true }
        },
        {
            "name": "unstake",
            "description": "Convert JitoSOL back to SOL instantly through the market. amount is in JitoSOL, or \"all\".",
            "inputSchema": {
                "type": "object",
                "properties": { "amount": amount, "request_id": request_id, "dry_run": dry_run },
                "required": ["amount"]
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": true, "idempotentHint": false, "openWorldHint": true }
        },
        {
            "name": "history",
            "description": "Recent operations with status, decision, approval and explorer link.",
            "inputSchema": {
                "type": "object",
                "properties": { "limit": { "type": "integer", "description": "Default 20." } }
            },
            "annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true }
        },
        {
            "name": "operation",
            "description": "Refresh and return one operation (receipt) by id.",
            "inputSchema": {
                "type": "object",
                "properties": { "id": { "type": "string" } },
                "required": ["id"]
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false, "openWorldHint": true }
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
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false, "openWorldHint": false }
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
            "annotations": { "readOnlyHint": false, "destructiveHint": true, "idempotentHint": true, "openWorldHint": true }
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
            "annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true }
        },
        {
            "name": "pay_x402",
            "description": "Call or pay an HTTP 402 (x402 v2) endpoint on your own inside your budget. Pass 'url' (and optional 'method', 'headers', 'body') so Manus fetches the resource, settles the 402 challenge, and retries automatically with PAYMENT-SIGNATURE / X-PAYMENT, or pass an already received 'challenge' / 'payment_required_header'. Returns a portable manus.receipt.v1, payment_header, and http_response.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "HTTP/HTTPS endpoint URL to request and automatically retry with the signed x402 payment header." },
                    "method": { "type": "string", "description": "HTTP method when 'url' is provided (default GET, or POST when 'body' is set)." },
                    "headers": { "type": "object", "description": "Optional HTTP request headers when 'url' is provided." },
                    "body": { "description": "Optional JSON or string request body when 'url' is provided." },
                    "challenge": { "type": "object", "description": "Decoded x402 v2 PaymentRequired JSON object (if already fetched)." },
                    "payment_required_header": { "type": "string", "description": "Base64-encoded PAYMENT-REQUIRED HTTP header (if already fetched)." },
                    "request_id": request_id,
                    "dry_run": dry_run
                }
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": true, "idempotentHint": false, "openWorldHint": true }
        },
        {
            "name": "verify",
            "description": "Cryptographically and on-chain verify a signed invoice, manus.receipt.v1, operation receipt, or local op_/inv_ ID.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "document": { "description": "Signed invoice object, manus.receipt.v1 object, operation receipt, JSON string, or op_/inv_ ID." },
                    "offline": { "type": "boolean", "description": "Only verify signatures and structure without querying Solana RPC." }
                },
                "required": ["document"]
            },
            "annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true }
        },
        {
            "name": "quote",
            "description": "Get a live Jupiter swap quote (mainnet), effective price, price impact, route, on-chain token safety inspection (mint/freeze authority, Token-2022 transfer fees and extensions), budget decision, and optional Kelly position sizing (pass kelly_fraction, or amount=\"kelly\").",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "from": { "type": "string", "description": "Token to sell: SOL, USDC, USDT, JitoSOL, mSOL, or a mint address." },
                    "to": { "type": "string", "description": "Token to buy: SOL, USDC, USDT, JitoSOL, mSOL, or a mint address." },
                    "amount": { "type": "string", "description": "Decimal amount in whole tokens, \"all\" (for tokens), or \"kelly\" (when kelly_fraction is provided for SOL)." },
                    "slippage_bps": { "type": "integer", "description": "Slippage tolerance in bps (default 50 = 0.5%)." },
                    "kelly_fraction": { "type": "number", "description": "Optional Kelly fraction (0.0..1.0, e.g. from AIPricePatterns MCP) to compute recommended SOL trade size within wallet balance and budget." }
                },
                "required": ["from", "to", "amount"]
            },
            "annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true }
        },
        {
            "name": "portfolio",
            "description": "Trading portfolio and PnL tracker: SOL and token holdings, live SOL valuation, per-token cost basis, average entry price, realized/unrealized PnL, active triggers, and closed-trade win rate & profit factor broken down by strategy tag.",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true }
        },
        {
            "name": "create_trigger",
            "description": "Create an autonomous Take-Profit, Stop-Loss, OCO (One-Cancels-the-Other), or Trailing-Stop exit trigger. While Manus MCP or `manus agent trigger watch` is running, Manus monitors the pair's Jupiter rate every 15s, updates peak_rate, and automatically executes the swap when a threshold is hit.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "from": { "type": "string", "description": "Token to sell when triggered (e.g. JUP, BONK, or a token mint)." },
                    "to": { "type": "string", "description": "Token to receive when triggered (e.g. SOL or USDC)." },
                    "amount": { "type": "string", "description": "Decimal amount in whole tokens, or \"all\" for the entire token balance." },
                    "slippage_bps": { "type": "integer", "description": "Slippage tolerance in bps (default 50)." },
                    "take_profit_rate": { "type": "number", "description": "Target output-per-input exchange rate at or above which to take profit." },
                    "stop_loss_rate": { "type": "number", "description": "Stop output-per-input exchange rate at or below which to cut losses." },
                    "take_profit_pct": { "type": "number", "description": "Optional percentage gain above current live Jupiter rate (e.g. 5.0 for +5%)." },
                    "stop_loss_pct": { "type": "number", "description": "Optional percentage drop below current live Jupiter rate (e.g. 2.5 for -2.5%)." },
                    "trailing_stop_bps": { "type": "integer", "description": "Optional trailing stop distance in bps below the highest observed rate (e.g. 200 for 2.0%)." },
                    "tag": { "type": "string", "description": "Optional strategy tag for PnL attribution when triggered." },
                    "dry_run": dry_run
                },
                "required": ["from", "to", "amount"]
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false, "openWorldHint": true }
        },
        {
            "name": "list_triggers",
            "description": "List active (or all recent) Take-Profit / Stop-Loss triggers.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "active_only": { "type": "boolean", "description": "Default true." }
                }
            },
            "annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true }
        },
        {
            "name": "cancel_trigger",
            "description": "Cancel an active Take-Profit / Stop-Loss trigger by id.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Trigger id (trg_...)." }
                },
                "required": ["id"]
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false, "openWorldHint": false }
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
                    "max_slippage_bps": { "type": "integer" },
                    "allowed_tokens_add": { "type": "array", "items": { "type": "string" }, "description": "Add allowed swap output tokens (symbol or mint)." },
                    "allowed_tokens_remove": { "type": "array", "items": { "type": "string" }, "description": "Remove allowed swap output tokens." },
                    "require_allowed_tokens": { "type": "boolean", "description": "When true, swapping into any token outside allowed_tokens requires Touch ID." },
                    "swap_cooldown_secs": { "type": "integer", "description": "Minimum seconds between automatic swaps without Touch ID." },
                    "max_daily_loss_sol": { "type": "string", "description": "Daily realized-loss circuit breaker in SOL (e.g. \"0.25\", or \"0\" to disable)." }
                },
                "required": ["reason"]
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false, "openWorldHint": false }
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
        assert_eq!(tools.len(), 19);
        for tool in tools {
            assert!(tool["name"].is_string());
            assert!(tool["description"].as_str().unwrap().len() > 10);
            assert_eq!(tool["inputSchema"]["type"], "object");
            let ann = tool["annotations"]
                .as_object()
                .unwrap_or_else(|| panic!("missing annotations on {}", tool["name"]));
            assert!(
                ann.get("readOnlyHint").and_then(|v| v.as_bool()).is_some(),
                "missing readOnlyHint on {}",
                tool["name"]
            );
            assert!(
                ann.get("destructiveHint").and_then(|v| v.as_bool()).is_some(),
                "missing destructiveHint on {}",
                tool["name"]
            );
        }

        let by_name = |n: &str| {
            tools
                .iter()
                .find(|t| t["name"] == n)
                .unwrap_or_else(|| panic!("missing tool {n}"))
        };
        for ro in [
            "wallet_status",
            "balances",
            "history",
            "invoice_status",
            "verify",
            "quote",
            "portfolio",
            "list_triggers",
        ] {
            let ann = &by_name(ro)["annotations"];
            assert_eq!(ann["readOnlyHint"], true, "{ro}");
            assert_eq!(ann["destructiveHint"], false, "{ro}");
            assert_eq!(ann["idempotentHint"], true, "{ro}");
        }
        for fm in ["send", "swap", "stake", "unstake", "pay_invoice", "pay_x402"] {
            let ann = &by_name(fm)["annotations"];
            assert_eq!(ann["readOnlyHint"], false, "{fm}");
            assert_eq!(ann["destructiveHint"], true, "{fm}");
            assert_eq!(ann["openWorldHint"], true, "{fm}");
        }
        for sm in [
            "operation",
            "create_invoice",
            "create_trigger",
            "cancel_trigger",
            "change_budget",
        ] {
            let ann = &by_name(sm)["annotations"];
            assert_eq!(ann["readOnlyHint"], false, "{sm}");
            assert_eq!(ann["destructiveHint"], false, "{sm}");
        }
        assert!(by_name("create_trigger")["inputSchema"]["properties"]["dry_run"].is_object());
    }

    #[test]
    fn amounts_accept_strings_and_numbers() {
        assert_eq!(amount(&json!({"amount": "0.5"})).unwrap(), "0.5");
        assert_eq!(amount(&json!({"amount": 2})).unwrap(), "2");
        assert!(amount(&json!({})).is_err());
    }
}
