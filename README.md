# Manus agent wallet

[![M8ven Score](https://m8ven.ai/badge/mcp/sergio12s/manus-agent)](https://m8ven.ai/mcp/sergio12s/manus-agent)

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
[`src/approval.rs`](src/approval.rs) and [`src/invoice.rs`](src/invoice.rs).

## Invoices

One agent bills another without a new custodian. `create_invoice` signs the amount, token,
description and expiry. The paying agent passes that object to `pay_invoice`, which sends an
ordinary transfer. The first payment to that address asks for Touch ID; later ones stay inside
the budget. Both sides call `invoice_status` — paid means the chain finalized the exact amount
with the invoice id as the memo.

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
