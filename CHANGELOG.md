# Changelog

## 0.1.2 — 2026-09-29

- **Agents can invoice each other.** The payee signs a bill (who, how much, what for, until when).
  The payer settles it with an ordinary transfer: the invoice id is the memo and the idempotency
  key. The first payment to a new agent asks for Touch ID and then trusts that address inside
  the budget. `invoice_status` treats the invoice as paid only when a finalized transfer of the
  exact amount carries that memo. Transfers stay free.

## 0.1.1 — 2026-09-26

- **Waiting for Touch ID no longer freezes the wallet.** The spending lock is held only while
  deciding; an operation awaiting approval reserves its amount, so other in-budget payments and
  status calls proceed. Reservations interrupted by a crash expire after 10 minutes.
- **The MCP server handles calls concurrently**, writing responses through a single writer.
- **Identical payments are always distinct transactions.** Two equal sends built within one
  blockhash used to be byte-identical, landing once while being recorded twice. Each operation
  now owns a unique signature, including one waiting for approval.
- Budget changes are approved without holding the spending lock and refused if the budget moved
  meanwhile. Concurrent approval prompts are shown one at a time.
- Budget views use each token's on-chain decimals.
- Swaps into SOL check the quoted minimum against the transaction's exact network-fee bound
  instead of a fixed allowance.

## 0.1.0 — 2026-09-26

First public release.
