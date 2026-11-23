use crate::Error;

/// #98: `Error` codes are part of the on-chain ABI once deployed — clients and
/// indexers persist them by number. This pins every variant's `u32` value so an
/// accidental reorder fails CI instead of silently shipping. If a variant is
/// added or renumbered, update this test and the `docs/CONTRACT.md` error
/// table in the same change.
#[test]
fn error_codes_are_pinned() {
    assert_eq!(Error::AlreadyInitialized as u32, 1);
    assert_eq!(Error::NotInitialized as u32, 2);
    assert_eq!(Error::Unauthorized as u32, 3);
    assert_eq!(Error::Paused as u32, 4);
    assert_eq!(Error::TokenNotAllowed as u32, 5);
    assert_eq!(Error::AmountTooSmall as u32, 6);
    assert_eq!(Error::AmountTooLarge as u32, 7);
    assert_eq!(Error::InvalidDeadline as u32, 8);
    assert_eq!(Error::IssueAlreadyFunded as u32, 9);
    assert_eq!(Error::BountyNotFound as u32, 10);
    assert_eq!(Error::InvalidStatus as u32, 11);
    assert_eq!(Error::DeadlineNotReached as u32, 12);
    assert_eq!(Error::SelfPayout as u32, 13);
    assert_eq!(Error::InvalidSplit as u32, 14);
    assert_eq!(Error::InvalidFee as u32, 15);
    assert_eq!(Error::Overflow as u32, 16);
    assert_eq!(Error::RelayerNotAllowed as u32, 17);
    assert_eq!(Error::TopUpLimitReached as u32, 18);
    assert_eq!(Error::DisputeNotExpired as u32, 19);
    assert_eq!(Error::DisputeAlreadyUsed as u32, 20);
    assert_eq!(Error::NothingToClaim as u32, 21);
    assert_eq!(Error::AlreadyClaimed as u32, 22);
    assert_eq!(Error::ConflictOfInterest as u32, 23);
    assert_eq!(Error::RelayerLimitExceeded as u32, 24);
    assert_eq!(Error::InvalidWinners as u32, 25);
    assert_eq!(Error::UpgradeNotReady as u32, 26);
    assert_eq!(Error::NoPendingUpgrade as u32, 27);
    assert_eq!(Error::UpgradeDelayCannotDecrease as u32, 28);
    assert_eq!(Error::InvalidKey as u32, 29);
    assert_eq!(Error::ContractAddressRejected as u32, 30);
    assert_eq!(Error::InvalidLimits as u32, 31);
}
