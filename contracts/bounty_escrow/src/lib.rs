#![no_std]

mod errors;
mod events;
mod fees;
mod storage;
mod types;

#[cfg(test)]
extern crate std;
#[cfg(test)]
mod test;

use soroban_sdk::{
    contract, contractimpl, symbol_short, token, Address, BytesN, ContractExecutable, Env, Symbol,
    Vec,
};

pub use errors::Error;
pub use types::{Bounty, Config, DataKey, SplitPayout, Status, TokenLimits, Totals, WinnerShare};
use types::{ClaimRecord, DisputeRecord, PendingUpgrade, RelayerLimit};

/// Top-ups per bounty. Bounds storage and event volume.
const MAX_TOP_UPS: u32 = 10;
const MAX_WINNERS: u32 = 5;

#[contract]
pub struct BountyEscrow;

fn require_admin(env: &Env, admin: &Address) -> Result<Config, Error> {
    let c = storage::get_config(env)?;
    if *admin != c.admin {
        return Err(Error::Unauthorized);
    }
    admin.require_auth();
    Ok(c)
}

fn changed(env: &Env, field: Symbol) {
    events::ConfigChanged { field }.publish(env);
}

fn add(a: i128, b: i128) -> Result<i128, Error> {
    a.checked_add(b).ok_or(Error::Overflow)
}

fn sub(a: i128, b: i128) -> Result<i128, Error> {
    a.checked_sub(b).ok_or(Error::Overflow)
}

/// Records a winner credit that could not be paid now. The contract keeps the funds until `claim`.
fn defer(env: &Env, token: &Address, net: i128) -> Result<(), Error> {
    let total = storage::get_claimable_total(env, token);
    storage::set_claimable_total(env, token, add(total, net)?);
    Ok(())
}

/// Removes a credit once it is paid out through `claim`.
fn undefer(env: &Env, token: &Address, net: i128) -> Result<(), Error> {
    let total = storage::get_claimable_total(env, token);
    storage::set_claimable_total(env, token, sub(total, net)?);
    Ok(())
}

fn authorize_release(env: &Env, caller: &Address, b: &Bounty, cfg: &Config) -> Result<bool, Error> {
    if *caller == b.maintainer {
        return Ok(false);
    }
    if *caller != cfg.relayer {
        return Err(Error::Unauthorized);
    }
    if !b.relayer_allowed {
        return Err(Error::RelayerNotAllowed);
    }
    Ok(true)
}

fn charge_relayer(env: &Env, token: &Address, amount: i128) -> Result<(), Error> {
    let Some(limit) = storage::get_relayer_limit(env, token) else {
        return Ok(());
    };
    let now = env.ledger().timestamp();
    let mut usage = storage::get_relayer_usage(env, token);
    if now >= usage.window_start.saturating_add(limit.window_secs) {
        usage.window_start = now;
        usage.released = 0;
    }
    usage.released = add(usage.released, amount)?;
    if usage.released > limit.cap {
        return Err(Error::RelayerLimitExceeded);
    }
    storage::set_relayer_usage(env, token, &usage);
    Ok(())
}

#[contractimpl]
impl BountyEscrow {
    /// Initializes contract configuration with roles, fee basis points, grace period, and duration limits.
    ///
    /// # Caller
    /// Requires authorization from `admin`.
    ///
    /// # Checks
    /// - Contract must not already be initialized.
    /// - `fee_bps` must not exceed `MAX_FEE_BPS` (1,000 bps = 10%).
    /// - `min_duration_secs` must not exceed `max_duration_secs`.
    ///
    /// # Errors
    /// - `Error::AlreadyInitialized`: Contract was already initialized.
    /// - `Error::InvalidFee`: `fee_bps` exceeds maximum permitted ceiling (1,000 bps).
    /// - `Error::InvalidDeadline`: `min_duration_secs` is greater than `max_duration_secs`.
    #[allow(clippy::too_many_arguments)]
    pub fn init(
        env: Env,
        admin: Address,
        relayer: Address,
        arbiter: Address,
        treasury: Address,
        fee_bps: u32,
        grace_secs: u64,
        min_duration_secs: u64,
        max_duration_secs: u64,
    ) -> Result<(), Error> {
        admin.require_auth();
        if storage::has_config(&env) {
            return Err(Error::AlreadyInitialized);
        }
        if fee_bps > fees::MAX_FEE_BPS {
            return Err(Error::InvalidFee);
        }
        if min_duration_secs > max_duration_secs {
            return Err(Error::InvalidDeadline);
        }
        storage::set_config(
            &env,
            &Config {
                admin,
                relayer,
                arbiter,
                treasury,
                fee_bps,
                grace_secs,
                min_duration_secs,
                max_duration_secs,
                paused: false,
            },
        );
        Ok(())
    }

    /// Escrows tokens for a GitHub issue bounty and transitions status to `Open`.
    ///
    /// # Caller
    /// Requires authorization from `maintainer`.
    ///
    /// # Checks
    /// - Contract must not be paused.
    /// - `token` must be allowlisted with configured limits.
    /// - `amount` must be between `limits.min_amount` and `limits.max_amount` inclusive.
    /// - `deadline` must fall within `[now + min_duration_secs, now + max_duration_secs]`.
    /// - No active bounty may already exist for the `(repo_id, issue)` pair.
    /// - Snapshots `fee_bps` and `grace_secs` into the bounty record at creation time.
    ///
    /// # Errors
    /// - `Error::NotInitialized`: Contract has not been initialized.
    /// - `Error::Paused`: Funding is paused by admin.
    /// - `Error::TokenNotAllowed`: Token is not on the allowlist.
    /// - `Error::AmountTooSmall`: Amount is below minimum token threshold.
    /// - `Error::AmountTooLarge`: Amount exceeds maximum token threshold.
    /// - `Error::InvalidDeadline`: Deadline is outside allowed min/max duration window.
    /// - `Error::IssueAlreadyFunded`: An active bounty already exists for this repo and issue.
    /// - `Error::Overflow`: Arithmetic overflow calculating timestamp windows or token totals.
    #[allow(clippy::too_many_arguments)]
    pub fn fund_bounty(
        env: Env,
        maintainer: Address,
        repo_id: u64,
        issue: u64,
        token: Address,
        amount: i128,
        deadline: u64,
        relayer_allowed: bool,
    ) -> Result<u64, Error> {
        maintainer.require_auth();
        let cfg = storage::get_config(&env)?;
        if cfg.paused {
            return Err(Error::Paused);
        }
        let limits = storage::get_token(&env, &token).ok_or(Error::TokenNotAllowed)?;
        if amount < limits.min_amount {
            return Err(Error::AmountTooSmall);
        }
        if amount > limits.max_amount {
            return Err(Error::AmountTooLarge);
        }
        let now = env.ledger().timestamp();
        let earliest = now
            .checked_add(cfg.min_duration_secs)
            .ok_or(Error::Overflow)?;
        let latest = now
            .checked_add(cfg.max_duration_secs)
            .ok_or(Error::Overflow)?;
        if deadline < earliest || deadline > latest {
            return Err(Error::InvalidDeadline);
        }
        if storage::active_bounty(&env, repo_id, issue).is_some() {
            return Err(Error::IssueAlreadyFunded);
        }

        let id = storage::next_id(&env);
        storage::set_bounty(
            &env,
            &Bounty {
                id,
                maintainer: maintainer.clone(),
                repo_id,
                issue,
                token: token.clone(),
                amount,
                fee_bps,
                fee_bps: policy.fee_bps,
                grace_secs: policy.grace_secs,
                fee_bps: cfg.fee_bps,
                deadline,
                relayer_allowed,
                status: Status::Open,
                winner: None,
                created_at: now,
            },
        );
        storage::set_active(&env, repo_id, issue, id);
        let mut t = storage::get_totals(&env, &token);
        t.funded = add(t.funded, amount)?;
        t.held = add(t.held, amount)?;
        storage::set_totals(&env, &token, &t);

        token::Client::new(&env, &token).transfer(
            &maintainer,
            env.current_contract_address(),
            &amount,
        );
        events::Funded {
            id,
            maintainer,
            repo_id,
            issue,
            token,
            amount,
            fee_bps: cfg.fee_bps,
            deadline,
            relayer_allowed,
        }
        .publish(&env);
        Ok(id)
    }

    /// Releases an open bounty to a single recipient (`winner`), deducting the snapshotted platform fee.
    ///
    /// # Caller
    /// Requires authorization from `caller`. Must be either the `maintainer`, or the configured
    /// `relayer` if `relayer_allowed` was enabled at funding.
    ///
    /// # Checks
    /// - Bounty status must be `Status::Open`.
    /// - If called by relayer: bounty must have `relayer_allowed == true` and transfer must not exceed rolling relayer rate cap.
    /// - `winner` cannot be the maintainer or the contract address (`SelfPayout`).
    /// - If the token transfer to `winner` fails (e.g. no trustline), payout is deferred to `claim` instead of reverting.
    ///
    /// # Errors
    /// - `Error::NotInitialized`: Contract has not been initialized.
    /// - `Error::BountyNotFound`: Bounty with `bounty_id` does not exist.
    /// - `Error::InvalidStatus`: Bounty status is not `Status::Open`.
    /// - `Error::Unauthorized`: Caller is not maintainer or authorized relayer.
    /// - `Error::RelayerNotAllowed`: Called by relayer, but maintainer opted out of relayer release.
    /// - `Error::RelayerLimitExceeded`: Relayer release amount exceeds rolling per-token quota.
    /// - `Error::SelfPayout`: Winner is the maintainer or the contract itself.
    /// - `Error::Overflow`: Arithmetic overflow calculating fee, net, or balance totals.
    pub fn release(
        env: Env,
        caller: Address,
        bounty_id: u64,
        winner: Address,
    ) -> Result<(), Error> {
        caller.require_auth();
        let cfg = storage::get_config(&env)?;
        let mut b = storage::get_bounty(&env, bounty_id)?;
        if b.status != Status::Open {
            return Err(Error::InvalidStatus);
        }
        if authorize_release(&env, &caller, &b, &cfg)? {
            charge_relayer(&env, &b.token, b.amount)?;
        }
        if winner == b.maintainer || winner == env.current_contract_address() {
            return Err(Error::SelfPayout);
        }
        let fee = fees::fee(b.amount, b.fee_bps)?;
        let net = fees::net(b.amount, b.fee_bps)?;

        b.status = Status::Released;
        b.winner = Some(winner.clone());
        storage::set_bounty(&env, &b);
        storage::clear_active(&env, b.repo_id, b.issue);

        let tok = token::Client::new(&env, &b.token);
        let me = env.current_contract_address();
        // A winner with no trustline must not revert the release. The net stays in the contract as a
        // claim the winner can collect once the trustline exists.
        let paid = matches!(tok.try_transfer(&me, &winner, &net), Ok(Ok(())));
        if !paid {
            storage::set_claim(
                &env,
                bounty_id,
                &ClaimRecord {
                    winner: winner.clone(),
                    net,
                    claimed: false,
                },
            );
            defer(&env, &b.token, net)?;
        }

        let mut t = storage::get_totals(&env, &b.token);
        if paid {
            t.released_net = add(t.released_net, net)?;
        }
        t.fees = add(t.fees, fee)?;
        t.held = sub(t.held, b.amount)?;
        storage::set_totals(&env, &b.token, &t);

        if fee > 0 {
            tok.transfer(&me, &cfg.treasury, &fee);
        }
        events::Released {
            id: bounty_id,
            winner,
            net,
            fee,
            caller,
            deferred: !paid,
        }
        .publish(&env);
        Ok(())
    }

    /// Releases an open bounty divided among multiple winners according to basis-point shares.
    ///
    /// # Caller
    /// Requires authorization from `caller`. Must be either the `maintainer`, or the configured
    /// `relayer` if `relayer_allowed` was enabled at funding.
    ///
    /// # Checks
    /// - Bounty status must be `Status::Open`.
    /// - `winners` must contain between 1 and 5 unique winner addresses.
    /// - No winner address may be the maintainer or the contract address (`SelfPayout`).
    /// - Winner shares must sum to exactly 10,000 bps (100%).
    /// - If called by relayer: checks relayer authorization and rolling rate cap.
    /// - Integer rounding dust from division is allocated to the first winner.
    ///
    /// # Errors
    /// - `Error::NotInitialized`: Contract has not been initialized.
    /// - `Error::BountyNotFound`: Bounty does not exist.
    /// - `Error::InvalidStatus`: Bounty is not `Status::Open`.
    /// - `Error::InvalidWinners`: `winners` list is empty, exceeds 5 entries, or has duplicates.
    /// - `Error::Unauthorized`: Caller is neither the maintainer nor authorized relayer.
    /// - `Error::RelayerNotAllowed`: Called by relayer on a bounty without relayer permissions.
    /// - `Error::RelayerLimitExceeded`: Relayer release volume exceeds rolling cap.
    /// - `Error::SelfPayout`: Any winner is the maintainer or the contract address.
    /// - `Error::InvalidSplit`: Total basis points do not equal 10,000.
    /// - `Error::Overflow`: Arithmetic overflow calculating payouts or totals.
    pub fn release_split(
        env: Env,
        caller: Address,
        bounty_id: u64,
        winners: Vec<WinnerShare>,
    ) -> Result<(), Error> {
        caller.require_auth();
        let cfg = storage::get_config(&env)?;
        let mut b = storage::get_bounty(&env, bounty_id)?;
        if b.status != Status::Open {
            return Err(Error::InvalidStatus);
        }
        if winners.is_empty() || winners.len() > MAX_WINNERS {
            return Err(Error::InvalidWinners);
        }
        if authorize_release(&env, &caller, &b, &cfg)? {
            charge_relayer(&env, &b.token, b.amount)?;
        }

        let mut total_bps = 0u32;
        for i in 0..winners.len() {
            let entry = winners.get(i).unwrap();
            if entry.winner == b.maintainer || entry.winner == env.current_contract_address() {
                return Err(Error::SelfPayout);
            }
            total_bps = total_bps
                .checked_add(entry.share_bps)
                .ok_or(Error::Overflow)?;
            for j in 0..i {
                if winners.get(j).unwrap().winner == entry.winner {
                    return Err(Error::InvalidWinners);
                }
            }
        }
        if total_bps != 10_000 {
            return Err(Error::InvalidSplit);
        }

        let fee = fees::fee(b.amount, b.fee_bps)?;
        let net = sub(b.amount, fee)?;
        let mut payouts = Vec::new(&env);
        let mut paid = 0i128;
        for i in 0..winners.len() {
            let entry = winners.get(i).unwrap();
            let amount = if i == 0 {
                0
            } else {
                fees::share(net, entry.share_bps)?
            };
            paid = add(paid, amount)?;
            payouts.push_back(SplitPayout {
                winner: entry.winner,
                amount,
            });
        }
        let first = winners.get(0).unwrap();
        payouts.set(
            0,
            SplitPayout {
                winner: first.winner,
                amount: sub(net, paid)?,
            },
        );

        b.status = Status::Released;
        b.winner = None;
        storage::set_bounty(&env, &b);
        storage::clear_active(&env, b.repo_id, b.issue);
        let tok = token::Client::new(&env, &b.token);
        let me = env.current_contract_address();
        for payout in payouts.iter() {
            tok.transfer(&me, &payout.winner, &payout.amount);
        }
        if fee > 0 {
            tok.transfer(&me, &cfg.treasury, &fee);
        }
        let mut totals = storage::get_totals(&env, &b.token);
        totals.released_net = add(totals.released_net, net)?;
        totals.fees = add(totals.fees, fee)?;
        totals.held = sub(totals.held, b.amount)?;
        storage::set_totals(&env, &b.token, &totals);
        events::ReleasedSplit {
            id: bounty_id,
            payouts,
            fee,
            caller,
        }
        .publish(&env);
        Ok(())
    }

    /// Refunds the entire bounty amount back to the maintainer without fees after the deadline and grace period have elapsed.
    ///
    /// # Caller
    /// Requires authorization from `maintainer`.
    ///
    /// # Checks
    /// - Caller must be the original bounty maintainer.
    /// - Bounty status must be `Status::Open`.
    /// - Current ledger timestamp must be `>= deadline + grace_secs` (using the grace period snapshotted at funding).
    /// - Operates even when the contract is paused.
    ///
    /// # Errors
    /// - `Error::BountyNotFound`: Bounty does not exist.
    /// - `Error::Unauthorized`: Caller is not the bounty maintainer.
    /// - `Error::InvalidStatus`: Bounty status is not `Status::Open`.
    /// - `Error::DeadlineNotReached`: Current time is before `deadline + grace_secs`.
    /// - `Error::Overflow`: Arithmetic overflow on timestamp addition or accounting totals.
    pub fn refund(env: Env, maintainer: Address, bounty_id: u64) -> Result<(), Error> {
        maintainer.require_auth();
        let cfg = storage::get_config(&env)?;
        let mut b = storage::get_bounty(&env, bounty_id)?;
        if maintainer != b.maintainer {
            return Err(Error::Unauthorized);
        }
        if b.status != Status::Open {
            return Err(Error::InvalidStatus);
        }
        // #163: use the grace snapshotted at funding time, not the live config value.
        let opens = b.deadline.checked_add(b.grace_secs).ok_or(Error::Overflow)?;
        let opens = b
            .deadline
            .checked_add(cfg.grace_secs)
            .ok_or(Error::Overflow)?;
        if env.ledger().timestamp() < opens {
            return Err(Error::DeadlineNotReached);
        }
        b.status = Status::Refunded;
        storage::set_bounty(&env, &b);
        storage::clear_active(&env, b.repo_id, b.issue);
        let mut t = storage::get_totals(&env, &b.token);
        t.refunded = add(t.refunded, b.amount)?;
        t.held = sub(t.held, b.amount)?;
        storage::set_totals(&env, &b.token, &t);

        token::Client::new(&env, &b.token).transfer(
            &env.current_contract_address(),
            &b.maintainer,
            &b.amount,
        );
        events::Refunded {
            id: bounty_id,
            amount: b.amount,
        }
        .publish(&env);
        Ok(RefundSettlement { amount: b.amount })
    }

    /// Returns the exact ledger timestamp at which a bounty becomes eligible for `refund`.
    ///
    /// # Caller
    /// Read-only query; requires no authentication.
    ///
    /// # Checks
    /// - Bounty must exist in storage.
    /// - Uses the `grace_secs` snapshotted into the bounty at funding time.
    ///
    /// # Errors
    /// - `Error::BountyNotFound`: Bounty does not exist.
    /// - `Error::Overflow`: Arithmetic overflow calculating `deadline + grace_secs`.
    pub fn refundable_at(env: Env, bounty_id: u64) -> Result<u64, Error> {
        let bounty = storage::get_bounty(&env, bounty_id)?;
        // #163: use the grace snapshotted at funding time.
        bounty.deadline.checked_add(bounty.grace_secs).ok_or(Error::Overflow)
    }

    /// Freezes release and refund operations on an open bounty, transitioning status to `Disputed`.
    ///
    /// # Caller
    /// Requires authorization from `caller`. Must be either the `maintainer` or the configured `arbiter`.
    ///
    /// # Checks
    /// - Bounty status must be `Status::Open`.
    /// - Bounty must not have been previously disputed (one dispute permitted per bounty lifecycle).
    ///
    /// # Errors
    /// - `Error::NotInitialized`: Contract has not been initialized.
    /// - `Error::BountyNotFound`: Bounty does not exist.
    /// - `Error::Unauthorized`: Caller is neither the maintainer nor the configured arbiter.
    /// - `Error::InvalidStatus`: Bounty status is not `Status::Open`.
    /// - `Error::DisputeAlreadyUsed`: Bounty was already disputed previously.
    pub fn open_dispute(env: Env, caller: Address, bounty_id: u64) -> Result<(), Error> {
        caller.require_auth();
        let cfg = storage::get_config(&env)?;
        let mut b = storage::get_bounty(&env, bounty_id)?;
        if caller != b.maintainer && caller != cfg.arbiter {
            return Err(Error::Unauthorized);
        }
        if b.status != Status::Open {
            return Err(Error::InvalidStatus);
        }
        // One dispute per bounty. An expired dispute returns to Open, so this stops it being reopened in a loop.
        if storage::get_dispute(&env, bounty_id).is_some() {
            return Err(Error::DisputeAlreadyUsed);
        }
        storage::set_dispute(
            &env,
            bounty_id,
            &DisputeRecord {
                opened_at: env.ledger().timestamp(),
            },
        );
        b.status = Status::Disputed;
        storage::set_bounty(&env, &b);
        events::Disputed {
            id: bounty_id,
            caller,
        }
        .publish(&env);
        Ok(())
    }

    /// Resolves an active dispute by allocating funds between a winner and the maintainer.
    ///
    /// # Caller
    /// Requires authorization from `arbiter`.
    ///
    /// # Checks
    /// - Caller must match the configured arbiter address.
    /// - Bounty status must be `Status::Disputed`.
    /// - `winner_bps` must be `<= 10_000` (100%).
    /// - If `winner_bps > 0`, a `winner` address is required and cannot be maintainer, contract, arbiter, or treasury.
    /// - Platform fee is deducted from the winner's portion; remainder reverts to maintainer.
    /// - If winner token transfer fails, net portion is deferred to `claim`.
    /// - Operates even when the contract is paused.
    ///
    /// # Errors
    /// - `Error::NotInitialized`: Contract has not been initialized.
    /// - `Error::BountyNotFound`: Bounty does not exist.
    /// - `Error::Unauthorized`: Caller is not the configured arbiter.
    /// - `Error::InvalidStatus`: Bounty status is not `Status::Disputed`.
    /// - `Error::InvalidSplit`: `winner_bps > 10_000` or `winner_bps > 0` with `None` winner.
    /// - `Error::SelfPayout`: Winner is the maintainer or contract address.
    /// - `Error::ConflictOfInterest`: Arbiter designated itself or the treasury as winner.
    /// - `Error::Overflow`: Arithmetic overflow calculating portions, fees, or totals.
    pub fn resolve(
        env: Env,
        arbiter: Address,
        bounty_id: u64,
        winner: Option<Address>,
        winner_bps: u32,
    ) -> Result<(), Error> {
        settle(&env, arbiter, bounty_id, winner, winner_bps, None)
    }

    /// Resolves an active dispute identical to `resolve`, while anchoring a 32-byte evidence bundle hash on-chain.
    ///
    /// # Caller
    /// Requires authorization from `arbiter`.
    ///
    /// # Checks
    /// - Same authorization, status, split, and conflict-of-interest checks as `resolve`.
    /// - Emits an `evidence` event carrying `evidence_hash`.
    ///
    /// # Errors
    /// - `Error::NotInitialized`: Contract has not been initialized.
    /// - `Error::BountyNotFound`: Bounty does not exist.
    /// - `Error::Unauthorized`: Caller is not the configured arbiter.
    /// - `Error::InvalidStatus`: Bounty status is not `Status::Disputed`.
    /// - `Error::InvalidSplit`: `winner_bps > 10_000` or `winner_bps > 0` with `None` winner.
    /// - `Error::SelfPayout`: Winner is the maintainer or contract address.
    /// - `Error::ConflictOfInterest`: Arbiter designated itself or the treasury as winner.
    /// - `Error::Overflow`: Arithmetic overflow calculating portions, fees, or totals.
    pub fn resolve_with_evidence(
        env: Env,
        arbiter: Address,
        bounty_id: u64,
        winner: Option<Address>,
        winner_bps: u32,
        evidence_hash: BytesN<32>,
    ) -> Result<(), Error> {
        settle(
            &env,
            arbiter,
            bounty_id,
            winner,
            winner_bps,
            Some(evidence_hash),
        )
    }

    /// Extends the deadline of an open bounty to a later timestamp.
    ///
    /// # Caller
    /// Requires authorization from `maintainer`.
    ///
    /// # Checks
    /// - Caller must be the bounty maintainer.
    /// - Bounty status must be `Status::Open`.
    /// - `new_deadline` must be strictly greater than current `deadline` (deadlines can only be extended).
    /// - `new_deadline` must not exceed `now + max_duration_secs`.
    ///
    /// # Errors
    /// - `Error::NotInitialized`: Contract has not been initialized.
    /// - `Error::BountyNotFound`: Bounty does not exist.
    /// - `Error::Unauthorized`: Caller is not the maintainer.
    /// - `Error::InvalidStatus`: Bounty status is not `Status::Open`.
    /// - `Error::InvalidDeadline`: `new_deadline <= deadline` or exceeds maximum duration from current time.
    /// - `Error::Overflow`: Arithmetic overflow adding max duration to current timestamp.
    pub fn extend_deadline(
        env: Env,
        maintainer: Address,
        bounty_id: u64,
        new_deadline: u64,
    ) -> Result<(), Error> {
        maintainer.require_auth();
        let cfg = storage::get_config(&env)?;
        let mut b = storage::get_bounty(&env, bounty_id)?;
        if maintainer != b.maintainer {
            return Err(Error::Unauthorized);
        }
        if b.status != Status::Open {
            return Err(Error::InvalidStatus);
        }
        if new_deadline <= b.deadline {
            return Err(Error::InvalidDeadline);
        }
        let latest = env
            .ledger()
            .timestamp()
            .checked_add(cfg.max_duration_secs)
            .ok_or(Error::Overflow)?;
        if new_deadline > latest {
            return Err(Error::InvalidDeadline);
        }
        let old_deadline = b.deadline;
        b.deadline = new_deadline;
        storage::set_bounty(&env, &b);
        events::DeadlineExtended {
            id: bounty_id,
            old_deadline,
            new_deadline,
        }
        .publish(&env);
        Ok(())
    }

    /// Adds additional tokens to an existing open bounty while preserving its initial snapshotted fee.
    ///
    /// # Caller
    /// Requires authorization from `maintainer`.
    ///
    /// # Checks
    /// - Contract must not be paused.
    /// - Caller must be the bounty maintainer.
    /// - Bounty status must be `Status::Open`.
    /// - `extra` must be strictly greater than 0.
    /// - Token must still be on the allowlist.
    /// - Top-up count must be `< MAX_TOP_UPS` (maximum 10 per bounty).
    /// - New total amount (`amount + extra`) must not exceed `limits.max_amount`.
    ///
    /// # Errors
    /// - `Error::NotInitialized`: Contract has not been initialized.
    /// - `Error::Paused`: Funding and top-up operations are paused.
    /// - `Error::BountyNotFound`: Bounty does not exist.
    /// - `Error::Unauthorized`: Caller is not the bounty maintainer.
    /// - `Error::InvalidStatus`: Bounty status is not `Status::Open`.
    /// - `Error::AmountTooSmall`: `extra <= 0`.
    /// - `Error::TokenNotAllowed`: Token has been removed from the allowlist.
    /// - `Error::TopUpLimitReached`: Bounty has reached maximum of 10 top-ups.
    /// - `Error::AmountTooLarge`: Cumulative amount exceeds token's maximum deposit limit.
    /// - `Error::Overflow`: Arithmetic overflow adding funds or updating totals.
    pub fn top_up(env: Env, maintainer: Address, bounty_id: u64, extra: i128) -> Result<(), Error> {
        maintainer.require_auth();
        let cfg = storage::get_config(&env)?;
        if cfg.paused {
            return Err(Error::Paused);
        }
        let mut b = storage::get_bounty(&env, bounty_id)?;
        if maintainer != b.maintainer {
            return Err(Error::Unauthorized);
        }
        if b.status != Status::Open {
            return Err(Error::InvalidStatus);
        }
        if extra <= 0 {
            return Err(Error::AmountTooSmall);
        }
        // A token removed from the allowlist can no longer be topped up. Release and refund still work.
        let limits = storage::get_token(&env, &b.token).ok_or(Error::TokenNotAllowed)?;
        let uses = storage::get_top_ups(&env, bounty_id);
        if uses >= MAX_TOP_UPS {
            return Err(Error::TopUpLimitReached);
        }
        let new_amount = add(b.amount, extra)?;
        // The per-token cap applies to the total, so a top-up cannot exceed it.
        if new_amount > limits.max_amount {
            return Err(Error::AmountTooLarge);
        }

        b.amount = new_amount;
        storage::set_bounty(&env, &b);
        storage::set_top_ups(&env, bounty_id, uses + 1);
        let mut t = storage::get_totals(&env, &b.token);
        t.funded = add(t.funded, extra)?;
        t.held = add(t.held, extra)?;
        storage::set_totals(&env, &b.token, &t);

        token::Client::new(&env, &b.token).transfer(
            &maintainer,
            env.current_contract_address(),
            &extra,
        );
        events::ToppedUp {
            id: bounty_id,
            extra,
            new_amount,
        }
        .publish(&env);
        Ok(())
    }

    /// Returns a timed-out disputed bounty back to `Status::Open` without deciding a winner.
    ///
    /// # Caller
    /// Callable by anyone; requires caller authorization (`caller.require_auth()`).
    ///
    /// # Checks
    /// - Bounty status must be `Status::Disputed`.
    /// - Dispute record must exist.
    /// - Current ledger timestamp must be `>= opened_at + dispute_timeout`.
    ///
    /// # Errors
    /// - `Error::BountyNotFound`: Bounty does not exist.
    /// - `Error::InvalidStatus`: Bounty status is not `Status::Disputed` or dispute record missing.
    /// - `Error::DisputeNotExpired`: Current timestamp is before dispute timeout expiration.
    /// - `Error::Overflow`: Arithmetic overflow calculating timeout deadline.
    pub fn expire_dispute(env: Env, caller: Address, bounty_id: u64) -> Result<(), Error> {
        caller.require_auth();
        let mut b = storage::get_bounty(&env, bounty_id)?;
        if b.status != Status::Disputed {
            return Err(Error::InvalidStatus);
        }
        let record = storage::get_dispute(&env, bounty_id).ok_or(Error::InvalidStatus)?;
        let ready = record
            .opened_at
            .checked_add(storage::dispute_timeout(&env))
            .ok_or(Error::Overflow)?;
        if env.ledger().timestamp() < ready {
            return Err(Error::DisputeNotExpired);
        }
        b.status = Status::Open;
        storage::set_bounty(&env, &b);
        events::DisputeExpired { id: bounty_id }.publish(&env);
        Ok(())
    }

    /// Pays out a deferred net amount to a winner whose transfer previously failed (e.g. missing trustline).
    ///
    /// # Caller
    /// Requires authorization from `winner`.
    ///
    /// # Checks
    /// - A deferred claim record must exist for `bounty_id`.
    /// - Claim must not have been previously collected (`claimed == false`).
    /// - Caller must match the recorded `winner` address.
    ///
    /// # Errors
    /// - `Error::BountyNotFound`: Bounty does not exist.
    /// - `Error::NothingToClaim`: No deferred claim record found for this bounty.
    /// - `Error::AlreadyClaimed`: Claim was already collected.
    /// - `Error::Unauthorized`: Caller does not match the recorded claim recipient.
    /// - `Error::Overflow`: Arithmetic overflow updating totals.
    pub fn claim(env: Env, winner: Address, bounty_id: u64) -> Result<(), Error> {
        winner.require_auth();
        let mut record = storage::get_claim(&env, bounty_id).ok_or(Error::NothingToClaim)?;
        if record.claimed {
            return Err(Error::AlreadyClaimed);
        }
        if record.winner != winner {
            return Err(Error::Unauthorized);
        }
        let b = storage::get_bounty(&env, bounty_id)?;
        record.claimed = true;
        storage::set_claim(&env, bounty_id, &record);
        undefer(&env, &b.token, record.net)?;
        let mut t = storage::get_totals(&env, &b.token);
        t.released_net = add(t.released_net, record.net)?;
        storage::set_totals(&env, &b.token, &t);

        token::Client::new(&env, &b.token).transfer(
            &env.current_contract_address(),
            &winner,
            &record.net,
        );
        events::Claimed {
            id: bounty_id,
            winner,
            net: record.net,
        }
        .publish(&env);
        Ok(())
    }

    /// Retrieves full details for a bounty by its identifier.
    ///
    /// # Caller
    /// Read-only query; requires no authentication.
    ///
    /// # Checks
    /// None.
    ///
    /// # Errors
    /// - `Error::BountyNotFound`: Bounty does not exist in storage.
    pub fn get_bounty(env: Env, id: u64) -> Result<Bounty, Error> {
        storage::get_bounty(&env, id)
    }

    /// Queries the active bounty identifier for a GitHub repository and issue pair, if one is open.
    ///
    /// # Caller
    /// Read-only query; requires no authentication.
    ///
    /// # Checks
    /// None.
    ///
    /// # Errors
    /// Returns `None` if no active bounty is open for the specified issue; never reverts.
    pub fn active_bounty(env: Env, repo_id: u64, issue: u64) -> Option<u64> {
        storage::active_bounty(&env, repo_id, issue)
    }

    /// Returns the global contract configuration parameters and role addresses.
    ///
    /// # Caller
    /// Read-only query; requires no authentication.
    ///
    /// # Checks
    /// None.
    ///
    /// # Errors
    /// - `Error::NotInitialized`: Contract has not yet been initialized.
    pub fn get_config(env: Env) -> Result<Config, Error> {
        storage::get_config(&env)
    }

    /// Returns the cumulative token accounting metrics (funded, released_net, fees, refunded, held).
    ///
    /// # Caller
    /// Read-only query; requires no authentication.
    ///
    /// # Checks
    /// None.
    ///
    /// # Errors
    /// Returns zeroed totals if no operations have occurred for the token; never reverts.
    pub fn get_totals(env: Env, token: Address) -> Totals {
        storage::get_totals(&env, &token)
    }

    /// Returns allowlist limits (minimum and maximum deposit bounds) for a token address.
    ///
    /// # Caller
    /// Read-only query; requires no authentication.
    ///
    /// # Checks
    /// None.
    ///
    /// # Errors
    /// Returns `None` if the token is not allowlisted; never reverts.
    pub fn get_token(env: Env, token: Address) -> Option<TokenLimits> {
        storage::get_token(&env, &token)
    }

    // ---- admin ----

    /// Adds or updates deposit limits for an accepted token contract address.
    ///
    /// # Caller
    /// Requires authorization from `admin`.
    ///
    /// # Checks
    /// - Caller must be the configured contract admin.
    /// - `min` must be strictly positive (`min > 0`).
    /// - `max` must be greater than or equal to `min`.
    ///
    /// # Errors
    /// - `Error::NotInitialized`: Contract has not been initialized.
    /// - `Error::Unauthorized`: Caller is not the admin.
    /// - `Error::AmountTooSmall`: `min <= 0`.
    /// - `Error::AmountTooLarge`: `max < min`.
    pub fn set_token(
        env: Env,
        admin: Address,
        token: Address,
        min: i128,
        max: i128,
    ) -> Result<(), Error> {
        require_admin(&env, &admin)?;
        if min <= 0 {
            return Err(Error::AmountTooSmall);
        }
        if max < min {
            return Err(Error::AmountTooLarge);
        }
        storage::set_token(
            &env,
            &token,
            &TokenLimits {
                min_amount: min,
                max_amount: max,
            },
        );
        changed(&env, symbol_short!("token"));
        Ok(())
    }

    /// Removes a token contract address from the allowlist, preventing new funding and top-ups.
    ///
    /// # Caller
    /// Requires authorization from `admin`.
    ///
    /// # Checks
    /// - Caller must be the configured contract admin.
    /// - `token` must currently exist in the allowlist.
    ///
    /// # Errors
    /// - `Error::NotInitialized`: Contract has not been initialized.
    /// - `Error::Unauthorized`: Caller is not the admin.
    /// - `Error::TokenNotAllowed`: Token is not currently on the allowlist.
    pub fn remove_token(env: Env, admin: Address, token: Address) -> Result<(), Error> {
        require_admin(&env, &admin)?;
        if storage::get_token(&env, &token).is_none() {
            return Err(Error::TokenNotAllowed);
        }
        storage::remove_token(&env, &token);
        changed(&env, symbol_short!("token"));
        Ok(())
    }

    /// Updates the global platform fee in basis points applied to future bounties.
    ///
    /// # Caller
    /// Requires authorization from `admin`.
    ///
    /// # Checks
    /// - Caller must be the configured contract admin.
    /// - `fee_bps` must not exceed `MAX_FEE_BPS` (1,000 bps = 10%).
    ///
    /// # Errors
    /// - `Error::NotInitialized`: Contract has not been initialized.
    /// - `Error::Unauthorized`: Caller is not the admin.
    /// - `Error::InvalidFee`: `fee_bps > 1000`.
    pub fn set_fee(env: Env, admin: Address, fee_bps: u32) -> Result<(), Error> {
        let mut c = require_admin(&env, &admin)?;
        if fee_bps > fees::MAX_FEE_BPS {
            return Err(Error::InvalidFee);
        }
        c.fee_bps = fee_bps;
        storage::set_config(&env, &c);
        changed(&env, symbol_short!("fee"));
        Ok(())
    }

    /// Updates the authorized relayer address for automated bounty releases.
    ///
    /// # Caller
    /// Requires authorization from `admin`.
    ///
    /// # Checks
    /// - Caller must be the configured contract admin.
    ///
    /// # Errors
    /// - `Error::NotInitialized`: Contract has not been initialized.
    /// - `Error::Unauthorized`: Caller is not the admin.
    pub fn set_relayer(env: Env, admin: Address, relayer: Address) -> Result<(), Error> {
        let mut c = require_admin(&env, &admin)?;
        c.relayer = relayer;
        storage::set_config(&env, &c);
        changed(&env, symbol_short!("relayer"));
        Ok(())
    }

    /// Updates the authorized arbiter address for opening and resolving disputes.
    ///
    /// # Caller
    /// Requires authorization from `admin`.
    ///
    /// # Checks
    /// - Caller must be the configured contract admin.
    ///
    /// # Errors
    /// - `Error::NotInitialized`: Contract has not been initialized.
    /// - `Error::Unauthorized`: Caller is not the admin.
    pub fn set_arbiter(env: Env, admin: Address, arbiter: Address) -> Result<(), Error> {
        let mut c = require_admin(&env, &admin)?;
        c.arbiter = arbiter;
        storage::set_config(&env, &c);
        changed(&env, symbol_short!("arbiter"));
        Ok(())
    }

    /// Updates the platform treasury address receiving deducted release fees.
    ///
    /// # Caller
    /// Requires authorization from `admin`.
    ///
    /// # Checks
    /// - Caller must be the configured contract admin.
    ///
    /// # Errors
    /// - `Error::NotInitialized`: Contract has not been initialized.
    /// - `Error::Unauthorized`: Caller is not the admin.
    pub fn set_treasury(env: Env, admin: Address, treasury: Address) -> Result<(), Error> {
        let mut c = require_admin(&env, &admin)?;
        c.treasury = treasury;
        storage::set_config(&env, &c);
        changed(&env, symbol_short!("treasury"));
        Ok(())
    }

    /// Updates the default grace period (in seconds) applied to newly funded bounties before refund.
    ///
    /// # Caller
    /// Requires authorization from `admin`.
    ///
    /// # Checks
    /// - Caller must be the configured contract admin.
    ///
    /// # Errors
    /// - `Error::NotInitialized`: Contract has not been initialized.
    /// - `Error::Unauthorized`: Caller is not the admin.
    pub fn set_grace(env: Env, admin: Address, grace_secs: u64) -> Result<(), Error> {
        let mut c = require_admin(&env, &admin)?;
        c.grace_secs = grace_secs;
        storage::set_config(&env, &c);
        changed(&env, symbol_short!("grace"));
        Ok(())
    }

    /// Updates the minimum and maximum allowed bounty duration bounds (in seconds).
    ///
    /// # Caller
    /// Requires authorization from `admin`.
    ///
    /// # Checks
    /// - Caller must be the configured contract admin.
    /// - `min` must be less than or equal to `max`.
    ///
    /// # Errors
    /// - `Error::NotInitialized`: Contract has not been initialized.
    /// - `Error::Unauthorized`: Caller is not the admin.
    /// - `Error::InvalidDeadline`: `min > max`.
    pub fn set_durations(env: Env, admin: Address, min: u64, max: u64) -> Result<(), Error> {
        let mut c = require_admin(&env, &admin)?;
        if min > max {
            return Err(Error::InvalidDeadline);
        }
        c.min_duration_secs = min;
        c.max_duration_secs = max;
        storage::set_config(&env, &c);
        changed(&env, symbol_short!("duration"));
        Ok(())
    }

    /// Transfers admin authority using two-party authentication to prevent lockout from typos.
    ///
    /// # Caller
    /// Requires dual authorization: both the current `admin` and incoming `new_admin` must sign.
    ///
    /// # Checks
    /// - Current admin authorization must match the stored admin.
    /// - Incoming admin authorization is required via `new_admin.require_auth()`.
    ///
    /// # Errors
    /// - `Error::NotInitialized`: Contract has not been initialized.
    /// - `Error::Unauthorized`: Calling address is not current admin.
    pub fn set_admin(env: Env, admin: Address, new_admin: Address) -> Result<(), Error> {
        let mut c = require_admin(&env, &admin)?;
        new_admin.require_auth();
        c.admin = new_admin;
        storage::set_config(&env, &c);
        changed(&env, symbol_short!("admin"));
        Ok(())
    }

    /// Pauses new bounty funding and top-ups; releases, refunds, and dispute resolutions remain active.
    ///
    /// # Caller
    /// Requires authorization from `admin`.
    ///
    /// # Checks
    /// - Caller must be the configured contract admin.
    ///
    /// # Errors
    /// - `Error::NotInitialized`: Contract has not been initialized.
    /// - `Error::Unauthorized`: Caller is not the admin.
    pub fn pause(env: Env, admin: Address) -> Result<(), Error> {
        let mut c = require_admin(&env, &admin)?;
        c.paused = true;
        storage::set_config(&env, &c);
        events::Paused {}.publish(&env);
        Ok(())
    }

    /// Resumes normal funding and top-up operations after being paused.
    ///
    /// # Caller
    /// Requires authorization from `admin`.
    ///
    /// # Checks
    /// - Caller must be the configured contract admin.
    ///
    /// # Errors
    /// - `Error::NotInitialized`: Contract has not been initialized.
    /// - `Error::Unauthorized`: Caller is not the admin.
    pub fn unpause(env: Env, admin: Address) -> Result<(), Error> {
        let mut c = require_admin(&env, &admin)?;
        c.paused = false;
        storage::set_config(&env, &c);
        events::Unpaused {}.publish(&env);
        Ok(())
    }

    /// Configures the timeout period (in seconds) after which an unresolved dispute may be expired.
    ///
    /// # Caller
    /// Requires authorization from `admin`.
    ///
    /// # Checks
    /// - Caller must be the configured contract admin.
    /// - `secs` must be strictly positive (`secs > 0`).
    ///
    /// # Errors
    /// - `Error::NotInitialized`: Contract has not been initialized.
    /// - `Error::Unauthorized`: Caller is not the admin.
    /// - `Error::InvalidDeadline`: `secs == 0`.
    pub fn set_dispute_timeout(env: Env, admin: Address, secs: u64) -> Result<(), Error> {
        require_admin(&env, &admin)?;
        if secs == 0 {
            return Err(Error::InvalidDeadline);
        }
        storage::set_dispute_timeout(&env, secs);
        changed(&env, symbol_short!("dispute"));
        Ok(())
    }

    /// Returns the dispute timeout window in seconds (defaults to 14 days if unset).
    ///
    /// # Caller
    /// Read-only query; requires no authentication.
    ///
    /// # Checks
    /// None.
    ///
    /// # Errors
    /// None; never reverts.
    pub fn get_dispute_timeout(env: Env) -> u64 {
        storage::dispute_timeout(&env)
    }

    /// Configures a rolling rate-limit window and maximum release cap for automated relayer payouts of a token.
    ///
    /// # Caller
    /// Requires authorization from `admin`.
    ///
    /// # Checks
    /// - Caller must be the configured contract admin.
    /// - `window_secs` must be strictly positive (`window_secs > 0`).
    /// - `cap` must be strictly positive (`cap > 0`).
    ///
    /// # Errors
    /// - `Error::NotInitialized`: Contract has not been initialized.
    /// - `Error::Unauthorized`: Caller is not the admin.
    /// - `Error::InvalidDeadline`: `window_secs == 0` or `cap <= 0`.
    pub fn set_relayer_limit(
        env: Env,
        admin: Address,
        token: Address,
        window_secs: u64,
        cap: i128,
    ) -> Result<(), Error> {
        require_admin(&env, &admin)?;
        if window_secs == 0 || cap <= 0 {
            return Err(Error::InvalidDeadline);
        }
        storage::set_relayer_limit(&env, &token, &RelayerLimit { window_secs, cap });
        changed(&env, symbol_short!("rlimit"));
        Ok(())
    }

    /// Updates the timelock delay required before proposed contract bytecode upgrades can execute.
    ///
    /// # Caller
    /// Requires authorization from `admin`.
    ///
    /// # Checks
    /// - Caller must be the configured contract admin.
    /// - `delay_secs` must be `>= current_delay` (timelock delay can only increase, never decrease).
    ///
    /// # Errors
    /// - `Error::NotInitialized`: Contract has not been initialized.
    /// - `Error::Unauthorized`: Caller is not the admin.
    /// - `Error::UpgradeDelayCannotDecrease`: `delay_secs` is smaller than the current delay.
    pub fn set_upgrade_delay(env: Env, admin: Address, delay_secs: u64) -> Result<(), Error> {
        let cfg = require_admin(&env, &admin)?;
        let current = storage::upgrade_delay(&env, cfg.max_duration_secs + cfg.grace_secs);
        if delay_secs < current {
            return Err(Error::UpgradeDelayCannotDecrease);
        }
        storage::set_upgrade_delay(&env, delay_secs);
        changed(&env, symbol_short!("upgrade"));
        Ok(())
    }

    /// Proposes a new Wasm bytecode hash for contract upgrade subject to the timelock delay.
    ///
    /// # Caller
    /// Requires authorization from `admin`.
    ///
    /// # Checks
    /// - Caller must be the configured contract admin.
    /// - Computes execution ETA as `now + upgrade_delay`.
    ///
    /// # Errors
    /// - `Error::NotInitialized`: Contract has not been initialized.
    /// - `Error::Unauthorized`: Caller is not the admin.
    /// - `Error::Overflow`: Arithmetic overflow calculating timelock ETA.
    pub fn propose_upgrade(env: Env, admin: Address, wasm_hash: BytesN<32>) -> Result<(), Error> {
        let cfg = require_admin(&env, &admin)?;
        let eta = env
            .ledger()
            .timestamp()
            .checked_add(storage::upgrade_delay(
                &env,
                cfg.max_duration_secs + cfg.grace_secs,
            ))
            .ok_or(Error::Overflow)?;
        storage::set_pending_upgrade(
            &env,
            &PendingUpgrade {
                wasm_hash: wasm_hash.clone(),
                eta,
            },
        );
        events::UpgradeProposed { wasm_hash, eta }.publish(&env);
        Ok(())
    }

    /// Applies a pending contract bytecode upgrade after the timelock delay has elapsed.
    ///
    /// # Caller
    /// Requires authorization from `admin`.
    ///
    /// # Checks
    /// - Caller must be the configured contract admin.
    /// - A pending upgrade proposal must exist.
    /// - Current ledger timestamp must be `>= pending.eta`.
    ///
    /// # Errors
    /// - `Error::NotInitialized`: Contract has not been initialized.
    /// - `Error::Unauthorized`: Caller is not the admin.
    /// - `Error::NoPendingUpgrade`: No upgrade is currently proposed.
    /// - `Error::UpgradeNotReady`: Timelock delay has not yet expired.
    pub fn execute_upgrade(env: Env, admin: Address) -> Result<(), Error> {
        require_admin(&env, &admin)?;
        let pending = storage::pending_upgrade(&env).ok_or(Error::NoPendingUpgrade)?;
        if env.ledger().timestamp() < pending.eta {
            return Err(Error::UpgradeNotReady);
        }
        storage::clear_pending_upgrade(&env);
        env.deployer()
            .update_current_contract(ContractExecutable::Wasm(pending.wasm_hash.clone()));
        events::Upgraded {
            wasm_hash: pending.wasm_hash,
        }
        .publish(&env);
        Ok(())
    }

    /// Cancels a pending contract upgrade proposal.
    ///
    /// # Caller
    /// Requires authorization from `admin`.
    ///
    /// # Checks
    /// - Caller must be the configured contract admin.
    /// - A pending upgrade proposal must exist.
    ///
    /// # Errors
    /// - `Error::NotInitialized`: Contract has not been initialized.
    /// - `Error::Unauthorized`: Caller is not the admin.
    /// - `Error::NoPendingUpgrade`: No upgrade is currently proposed.
    pub fn cancel_upgrade(env: Env, admin: Address) -> Result<(), Error> {
        require_admin(&env, &admin)?;
        let pending = storage::pending_upgrade(&env).ok_or(Error::NoPendingUpgrade)?;
        storage::clear_pending_upgrade(&env);
        events::UpgradeCancelled {
            wasm_hash: pending.wasm_hash,
        }
        .publish(&env);
        Ok(())
    }
}

/// Shared body of `resolve` and `resolve_with_evidence`.
fn settle(
    env: &Env,
    arbiter: Address,
    bounty_id: u64,
    winner: Option<Address>,
    winner_bps: u32,
    evidence: Option<BytesN<32>>,
) -> Result<(), Error> {
    arbiter.require_auth();
    let cfg = storage::get_config(env)?;
    if arbiter != cfg.arbiter {
        return Err(Error::Unauthorized);
    }
    let mut b = storage::get_bounty(env, bounty_id)?;
    if b.status != Status::Disputed {
        return Err(Error::InvalidStatus);
    }
    if winner_bps > 10_000 {
        return Err(Error::InvalidSplit);
    }
    let portion = fees::portion(b.amount, winner_bps)?;
    let mut fee = 0;
    let mut winner_net = 0;
    if winner_bps > 0 {
        match &winner {
            None => return Err(Error::InvalidSplit),
            Some(w) if *w == b.maintainer || *w == env.current_contract_address() => {
                return Err(Error::SelfPayout)
            }
            Some(w) if *w == cfg.arbiter || *w == cfg.treasury => {
                return Err(Error::ConflictOfInterest)
            }
            Some(_) => {}
        }
        fee = fees::fee(portion, b.fee_bps)?;
        winner_net = sub(portion, fee)?;
    }
    let maintainer_amount = sub(b.amount, portion)?;

    b.status = Status::Resolved;
    b.winner = if winner_bps > 0 { winner.clone() } else { None };
    storage::set_bounty(env, &b);
    storage::clear_active(env, b.repo_id, b.issue);

    let tok = token::Client::new(env, &b.token);
    let me = env.current_contract_address();
    let mut winner_deferred = false;
    if let (Some(w), true) = (&winner, winner_net > 0) {
        if !matches!(tok.try_transfer(&me, w, &winner_net), Ok(Ok(()))) {
            winner_deferred = true;
            storage::set_claim(
                env,
                bounty_id,
                &ClaimRecord {
                    winner: w.clone(),
                    net: winner_net,
                    claimed: false,
                },
            );
            defer(env, &b.token, winner_net)?;
        }
    }

    let mut t = storage::get_totals(env, &b.token);
    if winner_net > 0 && !winner_deferred {
        t.released_net = add(t.released_net, winner_net)?;
    }
    t.fees = add(t.fees, fee)?;
    t.refunded = add(t.refunded, maintainer_amount)?;
    t.held = sub(t.held, b.amount)?;
    storage::set_totals(env, &b.token, &t);

    if fee > 0 {
        tok.transfer(&me, &cfg.treasury, &fee);
    }
    if maintainer_amount > 0 {
        tok.transfer(&me, &b.maintainer, &maintainer_amount);
    }
    events::Resolved {
        id: bounty_id,
        winner: b.winner,
        winner_net,
        fee,
        maintainer_amount,
        winner_deferred,
    }
    .publish(env);
    if let Some(evidence_hash) = evidence {
        events::EvidenceRecorded {
            id: bounty_id,
            arbiter,
            evidence_hash,
        }
        .publish(env);
    }
    Ok(())
}
