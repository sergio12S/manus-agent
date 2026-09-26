# Security

Manus agent wallet holds a Solana key on the user's machine and lets an AI agent spend
inside a budget. We take reports about bypassing the budget, the approval gate, or the
structural transaction checks seriously.

## Reporting

Please open a **private security advisory** on this repository
(Security → Advisories → Report a vulnerability). Do not file a public issue.

Include the version (`manus --version`), platform, and the smallest reproduction you can.
We aim to acknowledge reports within 72 hours.

## In scope

- An agent moving value outside its budget without the human's LocalAuthentication approval
- An agent widening its own budget or switching networks without approval
- A transaction whose real effects differ from what the budget decision measured
- Disclosure of the wallet password or recovery phrase to the agent or the network
- Double payment for one `request_id`

## Out of scope

- An attacker who already controls the user's unlocked macOS account or the binary on disk
- Loss caused by approving a clearly described Touch ID prompt
- Third-party programs (Jupiter, token issuers) behaving as designed

The full threat model is at <https://manuspay.xyz/security>.
