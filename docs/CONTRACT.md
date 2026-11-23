# bounty_escrow contract

Soroban escrow for GitHub issue bounties. soroban-sdk 29.0.1, target `wasm32v1-none`.

## Lifecycle
`Open -> Released | Refunded | Disputed -> Resolved`, plus `Disputed -> Open` when a dispute expires. `Open` bounties can be extended (`extend_deadline`) and topped up (`top_up`). One active bounty per `(repo_id, issue)`.

## Lifecycle sequence diagrams

These render on GitHub. `Escrow` is this contract, `Token SAC` is the token contract, and every token movement is a call from the escrow to the token contract. `try_transfer` failures defer a payout instead of reverting the transaction.

### Fund

```mermaid
sequenceDiagram
    autonumber
    actor M as Maintainer
    participant E as Escrow contract
    participant T as Token SAC
    M->>E: fund_bounty(M, repo, issue, token, amount, deadline, relayer_allowed)
    Note over E: M signs, not paused, token allowlisted<br/>min ≤ amount ≤ max, deadline in window<br/>no active bounty on this issue
    Note over E: snapshot fee_bps and grace_secs, state = Open<br/>funded += amount, held += amount
    E->>T: transfer(from = Maintainer, to = Escrow, amount)
    T-->>E: transfer succeeds
    E-->>M: bounty id and Funded event