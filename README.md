# Manus agent wallet

[![M8ven Score](https://m8ven.ai/badge/mcp/sergio12s-manus-agent-11my2k)](https://m8ven.ai/mcp/sergio12s-manus-agent-11my2k?s=readme)

A Solana wallet your AI agent operates on its own, inside a budget you set. Anything above
the budget waits for your Touch ID. Every operation is simulated first and leaves a receipt.

Works with Claude Code, Codex, Gemini and any MCP client. Website and docs:
<https://manuspay.xyz>.

```bash
curl -fsSL https://manuspay.xyz/install.sh | bash
manus agent setup              # wallet in ~/.manus, password in the macOS Keychain, devnet by default
manus agent connect claude     # or: codex, gemini
```

## Why this repository exists

This is the **exact agent-wallet code** compiled into the official `manus` binary: the part
that holds your key, measures what a transaction does, enforces the budget and asks for your
approval. It is public so you can audit it. The official binary pins this repository by tag;
see [Releases](https://github.com/sergio12S/manus-agent/releases) for the matching version.

## How it stays safe

- **Budget, not keys.** Per-operation and rolling 24-hour limits for sends, conversions
  (swap/stake) and each token. Inside them the agent acts immediately.
- **Judged by effects.** Each transaction is signed, structurally inspected (single signer,
  allowlisted programs, no delegations, authority changes or account reassignment),
  simulated byte-for-byte, and measured by the wallet's real balance changes.
- **A gate the agent cannot press.** Above the budget, macOS LocalAuthentication asks for
  Touch ID or the login password. Widening the budget and moving to mainnet use the same gate.
- **Safe retries.** A `request_id` identifies one operation. The wallet records its signature
  before broadcast, reuses the existing receipt on retry, and never starts a second payment for
  that id. An interrupted submission may need reconciliation before its outcome is known.

Start with [`src/engine.rs`](src/engine.rs) (lifecycle), [`src/tx.rs`](src/tx.rs)
(inspection, simulation, Jupiter), [`src/budget.rs`](src/budget.rs),
[`src/approval.rs`](src/approval.rs), [`src/invoice.rs`](src/invoice.rs),
[`src/x402.rs`](src/x402.rs), [`src/verify.rs`](src/verify.rs) and
[`src/trading.rs`](src/trading.rs).

## Crypto trading execution, bracket triggers & portfolio PnL

Agents can pair external market-intelligence MCP servers (such as [AIPricePatterns](https://aipricepatterns.com)) with Manus as their dedicated **execution, risk-firewall, and portfolio layer** on Solana mainnet:

- **`quote` (`manus agent quote <from> <to> <amount> [--kelly-fraction <0..1>]`)**: fetches a live Jupiter quote, effective rate, price impact, inspects the on-chain SPL Token / Token-2022 mint for `freeze_authority`, `mint_authority`, `PermanentDelegate`, `TransferHook`, `NonTransferable`, and transfer fees (`risk_level`: `safe`, `caution`, `high_risk`), and sizes positions via `kelly_fraction` (or `amount="kelly"`) against your live SOL balance and remaining daily `convert_sol` budget.
- **One-call bracket `swap` (`take_profit_pct`, `stop_loss_pct`, `trailing_stop_bps`, `tag`)**: executes a Jupiter swap and atomically attaches an active exit trigger on the exact received token balance (`to -> from`), tagging the position (e.g. `"BULL_TREND_15m"`) for PnL attribution.
- **Autonomous Take-Profit / Stop-Loss / OCO / Trailing-Stop triggers (`create_trigger`, `list_triggers`, `cancel_trigger` & `manus agent trigger`)**: the background watcher inside `manus agent mcp` (or `manus agent trigger watch`) polls live Jupiter quotes every 15s, updates `peak_rate` for trailing stops, and executes the exit swap automatically inside your budget.
- **Automatic trading guards & daily loss circuit breaker**: swapping into an unknown token with an active `freeze_authority`, Token-2022 permanent delegate, or transfer fee automatically requires Touch ID approval even inside the budget. Operators can also set `allowed_tokens`, `require_allowed_tokens`, `swap_cooldown_secs`, and `max_daily_loss_sol` (blocks new non-exit entries when 24h realized loss reaches the threshold, while still permitting defensive exits into `SOL`, `USDC`, or `USDT`).
- **`portfolio` (`manus agent portfolio`)**: tracks on-chain balances, weighted-average entry price in SOL, cost basis, realized/unrealized PnL, active triggers, and closed-trade **Win Rate / Profit Factor** broken down by strategy `tag`.

## Invoices, x402 APIs and receipt verification

One agent bills another without a new custodian. `create_invoice` signs the amount, token,
description and expiry. The paying agent passes that object to `pay_invoice`, which sends an
ordinary transfer. The first payment to that address asks for Touch ID; later ones stay inside
the budget. Both sides call `invoice_status` — paid means the chain finalized the exact amount
with the invoice id as the memo.

When an HTTP service responds with `402 Payment Required` (x402 v2), the agent passes the
challenge or `PAYMENT-REQUIRED` header to `pay_x402` (or `manus agent x402 pay`). Manus verifies
the network, asset and atomic amount against the wallet's budget, settles the transfer with an
on-chain memo, and returns both a portable `manus.receipt.v1` and a base64 `payment_header`
proof to retry the HTTP request.

Any signed invoice, `manus.receipt.v1` document or operation receipt can be verified offline or
against Solana RPC using the `verify` MCP tool or `manus agent verify <target>`.

## If a payment outcome is unknown

If a send was interrupted and RPC cannot establish whether its signature landed, the receipt
shows `submission_unknown` (or `unknown` for an invoice). The amount remains counted against
the rolling budget, and retrying the same `request_id` returns the original receipt rather than
sending again. Check the recorded signature in a reliable transaction-history RPC or explorer
before deciding whether to create a new payment. A missing RPC result alone does not prove the
payment failed. Invoice lookup also reports an incomplete search when it cannot verify the
relevant recipient history.

## Fees

Sending SOL and tokens is free. Swaps, stake and unstake carry a **0.1% Manus fee**, collected
by Jupiter in the received token inside the same transaction, shown in every preview and
receipt, and paid to [`FEE_WALLET`](src/lib.rs). A quote charging more than 0.1% is rejected.

## Build from source

```bash
cargo build --release          # produces target/release/manus-agent
cargo test
# End-to-end against a local validator (macOS needs COPYFILE_DISABLE=1 for the validator):
solana-test-validator --reset &
cargo test --test localnet -- --ignored --test-threads=1
```

`manus-agent` exposes the same commands as `manus agent …` in the official binary.

## License

[Functional Source License 1.1, ALv2 Future License](LICENSE). You may read, run, modify and
share this code for any purpose except offering a competing product or service. Each
version becomes available under Apache 2.0 two years after its release.
