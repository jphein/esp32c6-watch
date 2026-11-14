//! #108: the roamer must not sit forever on an AP that gives no lease, or a lease that reaches nothing
//! (#89's L2-isolated BSSID), and must not flap when the probe target itself is down.
use roam_guard::*;

const A: Bssid = [0x9a, 0x5c, 0x8e, 0xcb, 0xdb, 0x90]; // #89's isolating AP
const B: Bssid = [0x9a, 0x5c, 0x8e, 0xcb, 0xdb, 0x91];
const C: Bssid = [0x9a, 0x5c, 0x8e, 0xcb, 0xdb, 0x92];
const MIN: u64 = 60_000;

#[test]
fn a_lease_is_overdue_only_after_the_timeout() {
    assert!(!RoamGuard::lease_overdue(1_000, 1_000 + LEASE_TIMEOUT_MS - 1));
    assert!(RoamGuard::lease_overdue(1_000, 1_000 + LEASE_TIMEOUT_MS));
    assert!(!RoamGuard::lease_overdue(5_000, 1_000)); // a clock that reads earlier is not overdue
}

#[test]
fn a_lease_timeout_quarantines_that_bssid_for_a_while() {
    let mut g = RoamGuard::new();
    g.on_lease_timeout(&A, 6, 0);
    assert!(g.is_bad(&A, 0) && g.is_bad(&A, QUARANTINE_MS - 1));
    assert!(!g.is_bad(&A, QUARANTINE_MS)); // expires: never a permanent ban
    assert!(!g.is_bad(&B, 0));
}

#[test]
fn an_unreachable_probe_quarantines_and_roams() {
    let mut g = RoamGuard::new();
    assert!(g.should_probe(0));
    assert_eq!(g.on_probe(&A, 6, Probe::Unreachable, 0), Verdict::Roam);
    assert!(g.is_bad(&A, 1));
}

#[test]
fn a_reachable_or_inconclusive_probe_keeps_the_ap() {
    let mut g = RoamGuard::new();
    assert_eq!(g.on_probe(&A, 6, Probe::Reachable, 0), Verdict::Keep);
    assert_eq!(g.on_probe(&A, 6, Probe::Inconclusive, 0), Verdict::Keep);
    assert!(!g.is_bad(&A, 1));
}

#[test]
fn two_different_aps_failing_in_a_row_means_the_target_is_down_not_the_ap() {
    // A broker that is down must not make the watch flap across every AP.
    let mut g = RoamGuard::new();
    assert_eq!(g.on_probe(&A, 6, Probe::Unreachable, 0), Verdict::Roam);
    assert_eq!(g.on_probe(&B, 11, Probe::Unreachable, MIN), Verdict::TargetDown);
    assert!(!g.is_bad(&A, MIN + 1) && !g.is_bad(&B, MIN + 1)); // neither AP is blamed
    assert!(!g.should_probe(MIN + 1) && g.should_probe(MIN + SUSPEND_MS)); // probing rests, then resumes
}

#[test]
fn a_reachable_probe_in_between_resets_the_consensus() {
    let mut g = RoamGuard::new();
    g.on_probe(&A, 6, Probe::Unreachable, 0);
    g.on_probe(&B, 11, Probe::Reachable, MIN);
    assert_eq!(g.on_probe(&C, 1, Probe::Unreachable, 2 * MIN), Verdict::Roam);
}

#[test]
fn the_same_ap_failing_again_is_still_that_ap() {
    let mut g = RoamGuard::new();
    g.on_probe(&A, 6, Probe::Unreachable, 0);
    assert_eq!(g.on_probe(&A, 6, Probe::Unreachable, QUARANTINE_MS + 1), Verdict::Roam);
    assert!(g.is_bad(&A, QUARANTINE_MS + 2));
}

#[test]
fn landing_on_a_quarantined_bssid_is_rejected_then_accepted_so_the_watch_always_gets_online() {
    let mut g = RoamGuard::new();
    g.on_lease_timeout(&A, 6, 0);
    assert_eq!(g.on_associated(&A, 1), Landing::Reject);
    assert_eq!(g.on_associated(&A, 2), Landing::Reject);
    assert_eq!(g.on_associated(&A, 3), Landing::AcceptExhausted); // the only AP there is: take it
    assert!(!g.should_probe(4)); // and don't probe it straight back into a roam loop
    assert_eq!(g.on_associated(&B, 5), Landing::Accept);
}

#[test]
fn a_good_landing_resets_the_rejection_count() {
    let mut g = RoamGuard::new();
    g.on_lease_timeout(&A, 6, 0);
    assert_eq!(g.on_associated(&A, 1), Landing::Reject);
    assert_eq!(g.on_associated(&B, 2), Landing::Accept);
    assert_eq!(g.on_associated(&A, 3), Landing::Reject);
}

#[test]
fn quarantine_is_bounded_and_the_oldest_entry_gives_way() {
    let mut g = RoamGuard::new();
    for i in 0..10u8 {
        g.on_lease_timeout(&[0, 0, 0, 0, 0, i], 1 + (i % 13), u64::from(i));
    }
    assert!(g.is_bad(&[0, 0, 0, 0, 0, 9], 10)); // the newest is held
    assert!(!g.is_bad(&[0, 0, 0, 0, 0, 0], 10)); // the oldest was evicted, never a panic
}

#[test]
fn avoid_mask_marks_the_channels_of_live_quarantines() {
    let mut g = RoamGuard::new();
    g.on_lease_timeout(&A, 6, 0);
    g.on_lease_timeout(&B, 11, 0);
    assert_eq!(g.avoid_mask(1), (1 << 6) | (1 << 11));
    assert_eq!(g.avoid_mask(QUARANTINE_MS), 0);
}

#[test]
fn pick_channel_steers_around_avoided_channels_but_never_strands_the_watch() {
    let mut obs = [None; N_CHANNELS];
    obs[5] = Some(-50); // ch6, strongest, quarantined
    obs[10] = Some(-70); // ch11
    assert_eq!(pick_channel(&obs, 0), Some((6, -50)));
    assert_eq!(pick_channel(&obs, 1 << 6), Some((11, -70)));
    assert_eq!(pick_channel(&obs, (1 << 6) | (1 << 11)), Some((6, -50))); // all avoided: strongest anyway
    assert_eq!(pick_channel(&[None; N_CHANNELS], 0), None);
}
