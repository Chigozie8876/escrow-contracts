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
```

### Release — maintainer and relayer callers

```mermaid
sequenceDiagram
    autonumber
    actor M as Maintainer
    actor R as Relayer
    participant E as Escrow contract
    participant T as Token SAC
    actor W as Winner
    participant TR as Treasury
    alt Maintainer calls release
        M->>E: release(caller = M, id, winner)
    else Relayer calls release, only if the bounty set relayer_allowed
        R->>E: release(caller = R, id, winner)
        Note over E: caller must equal config relayer<br/>charge_relayer(token, amount) against the rolling per-token cap
    end
    Note over E: caller signs, status must be Open<br/>winner is neither the maintainer nor the contract<br/>fee = floor(amount × fee_bps ÷ 10_000), net = amount − fee<br/>state = Released
    E->>T: try_transfer(from = Escrow, to = Winner, net)
    alt transfer succeeds
        T-->>W: credit net, released_net += net
    else transfer fails, for example no trustline
        T--xE: failure
        Note over E,W: store claim and defer net<br/>winner collects later with claim(W, id)
    end
    opt fee > 0
        E->>T: transfer(from = Escrow, to = Treasury, fee)
        T-->>TR: credit fee
    end
    Note over E: emit Released with the deferred flag
```

### Refund

```mermaid
sequenceDiagram
    autonumber
    actor M as Maintainer
    participant E as Escrow contract
    participant T as Token SAC
    M->>E: refund(M, id)
    Note over E: M signs and must be the bounty maintainer<br/>status must be Open<br/>now ≥ deadline + grace, with grace snapshotted at funding<br/>allowed while paused, state = Refunded
    E->>T: transfer(from = Escrow, to = Maintainer, full amount)
    T-->>M: credit full amount, no fee
    Note over E: refunded += amount, held -= amount
```

### Dispute and resolve

```mermaid
sequenceDiagram
    autonumber
    actor M as Maintainer
    actor A as Arbiter
    participant E as Escrow contract
    participant T as Token SAC
    actor W as Winner
    participant TR as Treasury
    M->>E: open_dispute(caller, id), or A opens it
    Note over E: caller is maintainer or arbiter, status Open → Disputed<br/>once per bounty, release and refund are frozen
    A->>E: resolve(A, id, winner, winner_bps)
    Note over E: A signs and must equal config arbiter, status Disputed<br/>winner_bps ≤ 10_000, winner required when bps > 0<br/>winner is not maintainer, contract, arbiter or treasury<br/>portion = floor(amount × winner_bps ÷ 10_000)<br/>fee = floor(portion × fee_bps ÷ 10_000)<br/>maintainer gets amount − portion, state = Resolved
    opt winner_bps > 0 and winner_net > 0
        E->>T: try_transfer(from = Escrow, to = Winner, winner_net)
        alt transfer succeeds
            T-->>W: credit winner_net
        else transfer fails
            Note over E,W: defer winner_net to claim
        end
    end
    opt fee > 0
        E->>T: transfer(from = Escrow, to = Treasury, fee)
        T-->>TR: credit fee
    end
    opt maintainer amount > 0
        E->>T: transfer(from = Escrow, to = Maintainer, amount − portion)
        T-->>M: credit remainder
    end
    Note over E,A: resolve_with_evidence also emits the evidence event
```

`release_split` follows the release flow but transfers one share to each of up to five winners in a single transaction, with all rounding dust assigned to the first winner (see "Fee math").

## Entry points
| Function | Auth | Notes / errors |
|---|---|---|
| `init(admin, relayer, arbiter, treasury, fee_bps, grace_secs, min_dur, max_dur)` | admin | once (`AlreadyInitialized`); `fee_bps <= 1000` (`InvalidFee`); `min <= max` (`InvalidDeadline`) |
| `fund_bounty(maintainer, repo_id, issue, token, amount, deadline, relayer_allowed) -> id` | maintainer | `Paused`, `TokenNotAllowed`, `AmountTooSmall/Large`, `InvalidDeadline`, `IssueAlreadyFunded`, `InvalidKey` (repo_id or issue must not be 0). Snapshots per-token fee override if set, else global `fee_bps`. Snapshots `grace_secs` from the policy at funding time (see #163). |
| `release(caller, id, winner)` | maintainer, or relayer if `relayer_allowed` | `InvalidStatus`, `Unauthorized`, `RelayerNotAllowed`, `SelfPayout` (winner is the maintainer or the contract itself). Allowed after deadline until refunded. Pays `amount - fee` to winner, `fee` to treasury. If the winner's transfer fails (for example, no trustline), the release still succeeds and the net becomes a claim (`deferred = true`). |
| `release_split(caller, id, winners)` | same as `release` | 1–5 unique winners whose shares total 10,000 bps. The fee is charged once; integer dust goes to the first winner. |
| `refund(maintainer, id)` | maintainer | only at/after `deadline + grace` (`DeadlineNotReached`); uses the `grace_secs` snapshotted into the bounty at funding time, so a later `set_grace` call cannot extend the refund window on existing bounties; full amount, no fee; works while paused |
| `refundable_at(id) -> u64` | none | returns unix timestamp at which bounty becomes refundable (`deadline + grace_secs`) |
| `open_dispute(caller, id)` | maintainer or arbiter | Open only; freezes release and refund. One dispute per bounty (`DisputeAlreadyUsed`). |
| `expire_dispute(caller, id)` | anyone | Disputed only; `now >= opened_at + dispute_timeout` (`DisputeNotExpired`). Returns the bounty to `Open` with no judgement. Default timeout 14 days. |
| `extend_deadline(maintainer, id, new_deadline)` | maintainer | Open only; `new_deadline > deadline` and `<= now + max_duration` (`InvalidDeadline`). Emits `deadline_extended`. |
| `top_up(maintainer, id, extra)` | maintainer | Open only; `Paused`, `TokenNotAllowed` if the token was removed, `AmountTooSmall` if `extra <= 0`, `AmountTooLarge` if the new total exceeds the token's `max_amount`, `TopUpLimitReached` after 10 top-ups. The fee snapshot is unchanged. Emits `topped_up`. |
| `claim(winner, id)` | the recorded winner | A deferred payout must exist (`NothingToClaim`), be unclaimed (`AlreadyClaimed`), and belong to the caller (`Unauthorized`). Pays the net. Emits `claimed`. |
| `resolve(arbiter, id, winner: Option, winner_bps)` | arbiter | Disputed only. `portion = floor(amount*bps/10000)`, fee taken from portion, remainder to maintainer. `InvalidSplit`, `SelfPayout`, `ConflictOfInterest` (winner is the arbiter or the treasury). Works while paused. A winner transfer that fails is deferred to `claim` instead of reverting. |
| `resolve_with_evidence(arbiter, id, winner, winner_bps, evidence_hash)` | arbiter | Same rules and errors as `resolve`. Also emits `evidence` with the 32-byte `evidence_hash`. See "Resolution evidence". |
| `get_bounty`, `active_bounty`, `get_config`, `get_totals`, `get_token` | none | read only |
| `set_token`, `remove_token`, `set_fee`, `set_relayer`, `set_arbiter`, `set_treasury`, `set_grace`, `set_durations` | admin | emit `config` or `token_set`/`token_removed` events. Token limits are persistent; reads fall back to legacy instance entries until an admin rewrites or removes them. |
| `set_admin(admin, new_admin)` | admin and new_admin | two-party auth |
| `pause`, `unpause` | admin | blocks `fund_bounty` only |
| `set_dispute_timeout(admin, secs)`, `get_dispute_timeout()` | admin / none | configure and inspect dispute expiry timeout window |
| `set_relayer_limit(admin, token, window_secs, cap)` | admin | configure per-token rolling rate limits on relayer releases |
| `set_upgrade_delay(admin, delay)` | admin | configure timelock delay for contract bytecode upgrades |
| `propose_upgrade`, `execute_upgrade`, `cancel_upgrade` | admin | proposal waits at least `max_duration + grace`; delay can only increase. Emits `upgrade_proposed`, `upgraded`, or `upgrade_cancelled`. |

## Errors
AlreadyInitialized=1 NotInitialized=2 Unauthorized=3 Paused=4 TokenNotAllowed=5 AmountTooSmall=6 AmountTooLarge=7 InvalidDeadline=8 IssueAlreadyFunded=9 BountyNotFound=10 InvalidStatus=11 DeadlineNotReached=12 SelfPayout=13 InvalidSplit=14 InvalidFee=15 Overflow=16 RelayerNotAllowed=17 TopUpLimitReached=18 DisputeNotExpired=19 DisputeAlreadyUsed=20 NothingToClaim=21 AlreadyClaimed=22 ConflictOfInterest=23 RelayerLimitExceeded=24 InvalidWinners=25 UpgradeNotReady=26 NoPendingUpgrade=27 UpgradeDelayCannotDecrease=28 InvalidKey=29 ContractAddressRejected=30 InvalidLimits=31

## Events
Topics `("bounty", <name>, id)`: `funded`, `released` (`deferred`), `released_split`, `refunded`, `disputed`, `resolved` (`winner_deferred`), `evidence` (`evidence_hash`), `deadline_extended`, `topped_up`, `dispute_expired`, `claimed`, `config`, `token_set`, `token_removed`, `paused`, `unpaused`, `upgrade_proposed`, `upgrade_cancelled`, `upgraded`, `initialized`. Data carries every field the indexer needs. See [docs/EVENTS.md](EVENTS.md) for full topic layouts, field types, and example JSON payloads for each event.

## Fee math
`fee = floor(amount*bps/10000)` (dust goes to the receiver); `portion = floor(amount*winner_bps/10000)` (dust stays with the maintainer). Checked arithmetic, `Overflow` on failure.

### Worked examples

Amounts are in the token's smallest unit (stroops for the native asset). The fee basis points are the snapshot taken at funding. Every number in these tables is asserted by `fees::tests::worked_examples` in `contracts/bounty_escrow/src/fees.rs`, which is the single source of truth shared with the TypeScript client.

**`release` — one fee on the full amount; `net = amount - fee` goes to the winner.**

| amount | fee_bps | fee | net to winner |
|---:|---:|---:|---:|
| 1_000_000 | 0 | 0 | 1_000_000 |
| 1_000_000 | 300 | 30_000 | 970_000 |
| 1 | 300 | 0 | 1 |
| 33 | 300 | 0 | 33 |
| 34 | 300 | 1 | 33 |

The last three rows are the tiny-amount case: `33 * 300 = 9_900`, which floors to `0`, so the winner receives all 33; one more stroop crosses the threshold (`34 * 300 = 10_200` floors to `1`). Dust always goes to the receiver, never to the contract.

**`resolve` — the fee is taken from the winner's portion only; the maintainer keeps `amount - portion`.**

`portion = floor(amount*winner_bps/10000)`; `fee = floor(portion*bps/10000)`; `winner_net = portion - fee`.

| amount | winner_bps | fee_bps | portion | fee | winner net | maintainer |
|---:|---:|---:|---:|---:|---:|---:|
| 1_000_000 | 0 | 300 | 0 | 0 | 0 | 1_000_000 |
| 1_000_000 | 5_000 | 300 | 500_000 | 15_000 | 485_000 | 500_000 |
| 1_000_000 | 7_000 | 300 | 700_000 | 21_000 | 679_000 | 300_000 |
| 1_000_000 | 10_000 | 300 | 1_000_000 | 30_000 | 970_000 | 0 |
| 3 | 3_333 | 300 | 0 | 0 | 0 | 3 |

The `0` row is the full-refund-to-maintainer outcome: no winner, no fee. The last row is the tiny-amount case where `3 * 3_333 = 9_999` floors to a zero portion, so the maintainer keeps everything and no fee is charged. A zero `fee_bps` makes the fee column `0` in every row.

**`release_split` — one fee on the full amount; every winner after the first gets `floor(net*share_bps/10000)` and the first winner gets the remaining dust.**

| amount | fee_bps | shares | fee | payouts |
|---:|---:|---|---:|---|
| 1_000_000 | 300 | 5_000 / 5_000 | 30_000 | 485_000 + 485_000 |
| 10_001 | 300 | 3_334 / 3_333 / 3_333 | 300 | 3_235 + 3_233 + 3_233 |
| 1_000_000 | 0 | 2_000 / 3_000 / 5_000 | 0 | 200_000 + 300_000 + 500_000 |

In the middle row the two floored shares are `3_233` each, so the first winner receives `9_701 - 3_233 - 3_233 = 3_235`: the one stroop of dust from the odd amount. The three payouts always sum exactly to `net`.

## Accounting invariant
Per token: `funded == released_net + fees + refunded + held`, and `held + claimable == token.balance(contract)`, where `claimable` is the sum of unclaimed deferred payouts (`DataKey::ClaimableTotal`).

A deferred payout moves its net from `held` to `claimable` and does not touch `released_net`. `claim` moves it from `claimable` to `released_net`. `top_up` raises `funded` and `held` together. The property test has not been updated for `claimable` or the new operations yet; it still checks `held == balance` and will fail on any sequence that defers a payout.

## TTL
Ledgers are ~5 s (17,280/day). Persistent entries and the instance are extended to 120 d whenever under 30 d remain; worst-case bounty life is 90 d + 3 d grace = 93 d (compile-time asserted). Testnet max entry TTL is 3,110,400 ledgers; re-check on mainnet.

## Trust model
- **Admin** can change config and propose code upgrades, but execution waits at least the longest refund window. The timelock creates an exit window; it does not make a malicious upgrade safe. Must be a multisig before mainnet.
- **Relayer** can release only bounties whose maintainer opted in, and only to a non-maintainer address. An optional rolling per-token cap bounds aggregate exposure; `RelayerLimitExceeded` means retry after the window or ask the maintainer to release.
- **Arbiter** can open a dispute on any Open bounty and split it between a winner and the maintainer.
- **Maintainer** can open a dispute on their own bounty, but only once per bounty. An unresolved dispute expires to `Open` after `dispute_timeout`, so a missing arbiter cannot freeze funds. The timeout expiry does not decide who is right: it returns the bounty to the normal rules (refund after `deadline + grace`).
- **Maintainer** can extend the deadline (never shorten it) and top up the amount while the bounty is open, up to the per-token `max_amount` and 10 top-ups.
- Pausing never traps funds: refund and resolve keep working.

## Known gaps
`upgrade` test needs the wasm built first (`./scripts/build.sh` before `cargo test`, as CI does).

## Grace snapshot migration note (#163)
`Bounty` now stores `grace_secs` (snapshotted at funding time). Bounties written to the deployed testnet contract before this change do not have this field; they will deserialise with a default of `0` for the missing value because Soroban's XDR codec fills absent trailing fields with their zero value. This means any pre-existing testnet bounty will read `grace_secs == 0` and become immediately refundable once its deadline passes (zero grace window). **Action required before mainnet:** migrate existing open bounties by re-funding them, or note in the deployment documentation that the testnet state is considered legacy and all open bounties should be settled before the upgrade is applied.

## Resolution evidence
`resolve_with_evidence` records a 32-byte `evidence_hash` in the `evidence` event, so the evidence behind a split is anchored on-chain. Suggested digest: SHA-256 over a canonical JSON bundle of the dispute reason, the AI reviews and the arbiter's note, with keys sorted and no insignificant whitespace. The contract does not check the bundle; it stores the hash.

## Deployment verification
`scripts/reproducible-build.sh` builds twice and compares hashes. `scripts/verify-deployment.sh <network>` compares the deployed Wasm with the `wasm_hash` recorded in `deployments/<network>.json`.
