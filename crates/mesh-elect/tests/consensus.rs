//! Consensus property tests.
//!
//! These exist because the properties that matter here — convergence,
//! permutation invariance, tie-breaking, hysteresis, partition merge — are
//! precisely the ones you CANNOT see on glass. A watch shows you one node's
//! opinion at one moment; it cannot show you that two nodes given the same
//! observations agree, or that a third node adopting late does not restart the
//! election. Every claim the design makes is asserted here.

use mesh_elect::*;

const N: usize = N_CHANNELS;

/// Scan result helper: `(channel, rssi)` pairs → the `observe_self` array.
fn scan(rows: &[(u8, i8)]) -> [Option<i8>; N] {
    let mut out = [None; N];
    for (ch, rssi) in rows {
        out[ch_index(*ch).expect("test channel in range")] = Some(*rssi);
    }
    out
}

fn node(id: u8, rows: &[(u8, i8)]) -> Elector {
    let mut e = Elector::new(id);
    e.observe_self(0, &scan(rows));
    e
}

/// Weight vector as a node would put it on the wire.
fn weights(rows: &[(u8, i8)]) -> [u8; N] {
    let mut w = [0u8; N];
    for (ch, rssi) in rows {
        w[ch_index(*ch).unwrap()] = weight(*rssi) as u8;
    }
    w
}

// ===========================================================================
// The core claim: same inputs → same winner, regardless of order
// ===========================================================================

/// The design rests on the election being a pure function of the observation
/// SET, so that every node computing it lands on the same answer with no
/// agreement rounds. If the result depended on the order observations arrived —
/// which on a lossy broadcast medium is arbitrary and different for every node —
/// the whole "no voting protocol needed" argument collapses.
#[test]
fn permutation_invariance() {
    let obs: [(u8, [u8; N]); 4] = [
        (10, weights(&[(1, -70), (6, -55)])),
        (20, weights(&[(6, -60), (11, -80)])),
        (30, weights(&[(1, -75), (6, -58), (11, -70)])),
        (40, weights(&[(6, -66), (11, -62)])),
    ];

    // Every rotation of arrival order must yield the same winner. (A rotation
    // per starting index is enough to catch order dependence in the tally and
    // in the tie-break, which is where it would hide.)
    let mut winners = vec![];
    for start in 0..obs.len() {
        let mut e = Elector::new(99);
        e.observe_self(0, &scan(&[(6, -65)]));
        for k in 0..obs.len() {
            let (id, w) = &obs[(start + k) % obs.len()];
            assert_eq!(
                e.observe_peer(0, *id, 0, 6, 0, w),
                Ingest::Recorded,
                "ingest of id{id} should be recorded"
            );
        }
        winners.push(e.winner(0).unwrap());
    }
    assert!(
        winners.windows(2).all(|p| p[0] == p[1]),
        "winner must not depend on arrival order, got {winners:?}"
    );
    // ch6 is the only channel all five see → it must win on voters alone.
    assert_eq!(winners[0], 6);
}

/// Two nodes that have never heard each other, scanning the same APs, must
/// still agree. This is where "fast, always" actually comes from: the common
/// cold-boot case needs no rendezvous at all, because the physical APs ARE the
/// shared input.
#[test]
fn independent_nodes_agree_from_scan_alone() {
    let aps = &[(1, -70), (6, -52), (11, -66)];
    let a = node(10, aps);
    let b = node(77, aps);
    assert_eq!(a.winner(0), b.winner(0));
    assert_eq!(a.winner(0), Some(6), "strongest usable channel wins a 1-1-1 tie");
}

// ===========================================================================
// Scoring: the fleet outvotes the close node
// ===========================================================================

/// The spec's stated goal: "an AP that only one node can see loses to one the
/// whole fleet sees". A plain saturating SUM does not deliver this — one node at
/// -35 dBm scores 48, beating three nodes at -70 dBm (16 each = 48... and
/// winning outright at -34). Count-dominant lexicographic scoring does deliver
/// it, which is why this crate departs from the spec's plain sum.
#[test]
fn fleet_majority_beats_one_very_close_node() {
    let mut e = Elector::new(1);
    // We are the close node, right on top of a ch1 AP and nothing else.
    e.observe_self(0, &scan(&[(1, -30)]));
    // Three peers can only use ch11, and only moderately.
    for id in [2u8, 3, 4] {
        e.observe_peer(0, id, 0, 11, 0, &weights(&[(11, -70)]));
    }
    assert_eq!(
        e.winner(0),
        Some(11),
        "3 voters must beat 1 voter no matter how strong the 1 is"
    );

    let t = e.tally(0);
    assert_eq!(t[ch_index(1).unwrap()].voters, 1);
    assert_eq!(t[ch_index(11).unwrap()].voters, 3);
}

/// Saturation: proximity must not buy unbounded influence.
#[test]
fn weight_saturates_and_floors() {
    assert_eq!(weight(-30), weight(WEIGHT_CEIL_DBM), "clamped at the top");
    assert_eq!(weight(-35), weight(-30));
    assert_eq!(weight(-83), 0, "below the usable floor is not a vote");
    assert_eq!(weight(USABLE_MIN_DBM), 1, "bare usable visibility still counts");
    assert!(weight(-50) > weight(-70), "monotone in between");
}

/// A channel the fleet can only barely hear must not win on headcount, or the
/// election would happily march everyone onto a channel nobody can associate on.
#[test]
fn unusable_channel_never_wins_on_headcount() {
    let mut e = Elector::new(1);
    e.observe_self(0, &scan(&[(6, -55), (11, -90)]));
    for id in [2u8, 3, 4, 5] {
        // Every peer "sees" ch11, but below the usable floor → weight 0.
        e.observe_peer(0, id, 0, 6, 0, &weights(&[(6, -60), (11, -95)]));
    }
    assert_eq!(e.winner(0), Some(6));
    assert_eq!(e.tally(0)[ch_index(11).unwrap()].voters, 0);
}

/// Ties must resolve identically everywhere, with no coin flip. Lowest channel
/// number is the total order (BSSIDs move between scans; channel numbers do not).
#[test]
fn ties_break_to_lowest_channel() {
    let e = node(5, &[(1, -60), (6, -60), (11, -60)]);
    assert_eq!(e.winner(0), Some(1));
}

// ===========================================================================
// Epoch: adopt, don't re-elect
// ===========================================================================

/// "All nodes that see any other node should join the same mesh" requires that a
/// late joiner ADOPTS rather than re-running the election — otherwise every new
/// node arriving is a fresh chance to partition.
#[test]
fn late_joiner_adopts_established_epoch() {
    let mut joiner = Elector::new(50);
    // Its own scan would prefer ch1.
    joiner.observe_self(0, &scan(&[(1, -40), (6, -70)]));
    assert_eq!(joiner.winner(0), Some(1));

    // But the fleet is already converged on ch6 at epoch 7.
    let got = joiner.observe_peer(0, 10, 7, 6, 3, &weights(&[(6, -65)]));
    assert_eq!(got, Ingest::Adopted(Decision { channel: 6, epoch: 7, gateway: 3 }));
    assert_eq!(joiner.decision().channel, 6, "adopted, not re-elected");
    assert!(joiner.on_probation(), "an adopted decision starts unproven");
}

/// Higher epoch wins; a stale lower-epoch frame must not drag us back.
#[test]
fn lower_epoch_does_not_override() {
    let mut e = Elector::new(1);
    e.observe_self(0, &scan(&[(1, -60), (6, -60)]));
    e.observe_peer(0, 2, 9, 6, 0, &weights(&[(6, -60)]));
    assert_eq!(e.decision().epoch, 9);
    e.observe_peer(0, 3, 4, 1, 0, &weights(&[(1, -60)]));
    assert_eq!(e.decision().channel, 6, "epoch 4 must not beat epoch 9");
    assert_eq!(e.decision().epoch, 9);
}

/// Partition merge: the higher epoch wins; equal epochs break by member count,
/// then by channel. Must be a total order or a merge can oscillate forever.
#[test]
fn partition_merge_is_total_order() {
    let a = Decision { channel: 6, epoch: 5, gateway: 0 };
    let b = Decision { channel: 1, epoch: 4, gateway: 0 };
    assert!(a.supersedes(2, &b, 9), "higher epoch beats bigger partition");

    let c = Decision { channel: 11, epoch: 5, gateway: 0 };
    assert!(c.supersedes(6, &a, 3), "equal epoch → more members wins");
    assert!(!a.supersedes(3, &c, 6));

    let d = Decision { channel: 1, epoch: 5, gateway: 0 };
    assert!(d.supersedes(3, &a, 3), "equal epoch+members → lower channel wins");
    assert!(!a.supersedes(3, &d, 3), "and the order is antisymmetric");
}

// ===========================================================================
// Hysteresis: converge, then STOP
// ===========================================================================

/// Flapping costs an association and tears the mesh down each time, so it is
/// worse than a suboptimal-but-stable channel. A marginally better challenger
/// must never move us.
#[test]
fn marginal_challenger_never_flaps() {
    let mut e = Elector::new(1);
    e.restore(Decision { channel: 6, epoch: 1, gateway: 0 });

    // Same voter count, a hair stronger — must not clear the margin, ever, no
    // matter how long we wait. Observations are refreshed every round, as the
    // firmware tick does; without that the table would go stale and the test
    // would pass vacuously on an empty tally.
    for t in (0..600_000).step_by(2_000) {
        e.observe_self(t, &scan(&[(6, -60), (11, -58)]));
        assert_eq!(e.step(t), None, "must not switch at t={t}");
    }
    assert_eq!(e.decision().channel, 6);
    assert_eq!(e.decision().epoch, 1, "no epoch spent");
}

/// A challenger that IS clearly better still has to hold the lead for the settle
/// window before we spend an epoch on it.
#[test]
fn decisive_challenger_waits_for_settle_window() {
    let mut e = Elector::new(1);
    e.restore(Decision { channel: 6, epoch: 1, gateway: 0 });
    // ch11 has two voters to ch6's one → decisive (a connectivity win).
    e.observe_self(0, &scan(&[(6, -75), (11, -50)]));
    e.observe_peer(0, 2, 0, 11, 0, &weights(&[(11, -55)]));

    assert_eq!(e.step(0), None, "first sighting only arms the challenger");
    assert_eq!(e.step(SETTLE_MS - 1), None, "still inside the settle window");
    let d = e.step(SETTLE_MS + 1).expect("settled → commit");
    assert_eq!(d.channel, 11);
    assert_eq!(d.epoch, 2, "exactly one epoch spent");
    // And it must then STOP.
    assert_eq!(e.step(SETTLE_MS + 2), None);
    assert_eq!(e.step(10 * SETTLE_MS), None, "converged means quiet");
}

/// A challenger that leads briefly and then falls back must leave no trace —
/// the settle timer has to reset, not accumulate across interruptions.
#[test]
fn interrupted_lead_resets_the_settle_timer() {
    let mut e = Elector::new(1);
    e.restore(Decision { channel: 6, epoch: 1, gateway: 0 });
    e.observe_self(0, &scan(&[(6, -75), (11, -50)]));
    e.observe_peer(0, 2, 0, 11, 0, &weights(&[(11, -55)]));
    assert_eq!(e.step(0), None);
    assert_eq!(e.step(SETTLE_MS / 2), None);

    // ch11 collapses (peer left, our own view of it dies), ch6 recovers.
    e.observe_self(SETTLE_MS / 2, &scan(&[(6, -50)]));
    e.observe_peer(SETTLE_MS / 2, 2, 0, 6, 0, &weights(&[(6, -55)]));
    assert_eq!(e.step(SETTLE_MS / 2 + 1), None);
    assert_eq!(e.decision().channel, 6);

    // ch11 comes back: it must serve a FULL settle window from now, not inherit
    // credit for the earlier half.
    e.observe_self(SETTLE_MS, &scan(&[(6, -75), (11, -50)]));
    e.observe_peer(SETTLE_MS, 2, 0, 11, 0, &weights(&[(11, -55)]));
    assert_eq!(e.step(SETTLE_MS), None, "re-arms here");
    assert_eq!(e.step(SETTLE_MS + SETTLE_MS - 1), None, "not yet");
    assert!(e.step(2 * SETTLE_MS + 1).is_some(), "now it has held long enough");
}

/// A lone node must never move the fleet's channel off a committed decision. It
/// knows only what its own antenna sees. Two things depend on this: a watch
/// carried into another building must not abandon the fleet it left, and this
/// change must be safe to deploy on the watch BEFORE smol speaks ELECT — with no
/// peers heard, the watch stays put instead of walking off the rendezvous
/// channel and partitioning the mesh it was meant to fix.
#[test]
fn a_lone_node_is_not_a_quorum() {
    let mut e = Elector::new(1);
    e.restore(Decision { channel: 6, epoch: 2, gateway: 0 });
    // Our own scan strongly prefers ch1, and ch6 is not even usable here.
    for t in (0..300_000).step_by(2_000) {
        e.observe_self(t, &scan(&[(1, -40)]));
        assert_eq!(e.step(t), None, "alone: must not move the fleet at t={t}");
    }
    assert_eq!(e.decision().channel, 6);
    assert_eq!(e.decision().epoch, 2, "no epoch spent while alone");

    // One peer agreeing that ch1 is better is quorum enough to proceed.
    let base = 300_000;
    e.observe_self(base, &scan(&[(1, -40)]));
    e.observe_peer(base, 2, 0, 6, 0, &weights(&[(1, -45)]));
    assert_eq!(e.step(base), None, "arms the challenger");
    let d = e
        .step(base + SETTLE_MS + 1)
        .expect("with a peer, the election may proceed");
    assert_eq!(d.channel, 1);
}

/// The quorum gate must not block ADOPTION — a lone node still follows the fleet
/// when the fleet tells it where to go, otherwise a single watch could never
/// rejoin.
#[test]
fn quorum_gate_does_not_block_adoption() {
    let mut e = Elector::new(1);
    e.restore(Decision { channel: 6, epoch: 2, gateway: 0 });
    e.observe_self(0, &scan(&[(1, -50), (11, -50)]));
    assert!(matches!(
        e.observe_peer(0, 2, 8, 11, 0, &weights(&[(11, -55)])),
        Ingest::Adopted(_)
    ));
    assert_eq!(e.decision().channel, 11, "adoption is not an election");
}

// ===========================================================================
// Anti-wedge: a monotonic epoch must not be a permanent hostage
// ===========================================================================

/// The failure mode an unconditional "higher epoch wins" invites, and which the
/// spec does not address: one node holding a high epoch for a channel that no
/// longer exists pins the entire fleet onto a dead channel forever. Probation
/// bounds it — an adopted decision that yields nothing gets abandoned.
#[test]
fn adopted_but_dead_epoch_is_abandoned() {
    let mut e = Elector::new(1);
    e.observe_self(0, &scan(&[(1, -55), (6, -60)]));
    // A peer drags us to ch6 at a high epoch.
    assert!(matches!(
        e.observe_peer(0, 2, 9_000, 6, 0, &weights(&[(6, -60)])),
        Ingest::Adopted(_)
    ));
    assert_eq!(e.decision().channel, 6);

    // Nothing ever answers on ch6. Before the probation window, we hold.
    assert_eq!(e.note_barren(PROBATION_MS - 1), None, "do not give up early");
    // After it, we re-elect ONTO A HIGHER EPOCH so the fleet can follow us out.
    let d = e.note_barren(PROBATION_MS + 1).expect("escape the dead channel");
    assert_eq!(d.channel, 1, "fall back to a channel we can actually use");
    assert!(d.epoch > 9_000, "must move the epoch forward, not backward");
    assert!(!e.on_probation());
}

/// Probation must clear when the decision proves out, so a healthy fleet never
/// abandons a good channel just because it was adopted rather than self-elected.
#[test]
fn probation_clears_once_the_channel_validates() {
    let mut e = Elector::new(1);
    e.observe_self(0, &scan(&[(6, -55)]));
    assert!(matches!(
        e.observe_peer(0, 2, 500, 6, 0, &weights(&[(6, -55)])),
        Ingest::Adopted(_)
    ));
    assert!(e.on_probation());
    // Our own observations agree ch6 is the winner → validated.
    assert_eq!(e.step(1_000), None);
    assert!(!e.on_probation());
    assert_eq!(e.note_barren(10 * PROBATION_MS), None, "nothing to escape");
}

// ===========================================================================
// Bounds and hostile input
// ===========================================================================

/// The sibling `SmolMesh.peers` is an append-only Vec fed from unauthenticated
/// broadcasts — an OOM waiting to happen on a watch with ~6.5 KB of free heap.
/// This table must refuse a stranger rather than grow or evict a live peer.
#[test]
fn full_table_refuses_strangers_without_evicting_live_peers() {
    let mut e = Elector::new(1);
    e.observe_self(0, &scan(&[(6, -60)]));
    // Fill every remaining slot with live peers.
    for id in 2..=(MAX_NODES as u8) {
        assert_eq!(
            e.observe_peer(0, id, 0, 6, 0, &weights(&[(6, -60)])),
            Ingest::Recorded
        );
    }
    assert_eq!(e.members(0), MAX_NODES as u8);

    // A flood of new ids must all bounce, and the roster must not budge.
    for id in 100..200u8 {
        assert_eq!(
            e.observe_peer(0, id, 0, 6, 0, &weights(&[(6, -60)])),
            Ingest::Full,
            "id{id} must be refused, not admitted"
        );
    }
    assert_eq!(e.members(0), MAX_NODES as u8, "roster is bounded");
    // Known peers still refresh fine — the bound must not break the live fleet.
    assert_eq!(
        e.observe_peer(1_000, 2, 0, 6, 0, &weights(&[(6, -55)])),
        Ingest::Recorded
    );
}

/// A stale slot is reclaimable — the bound must not permanently lock out a node
/// that joins after another has left.
#[test]
fn stale_slots_are_reclaimed() {
    let mut e = Elector::new(1);
    e.observe_self(0, &scan(&[(6, -60)]));
    for id in 2..=(MAX_NODES as u8) {
        e.observe_peer(0, id, 0, 6, 0, &weights(&[(6, -60)]));
    }
    let later = OBS_STALE_MS + 1_000;
    // Everyone has gone quiet → a newcomer takes a reclaimed slot.
    assert_eq!(
        e.observe_peer(later, 123, 0, 6, 0, &weights(&[(6, -60)])),
        Ingest::Recorded
    );
}

/// Stale observations must stop voting — a node that walked away must not keep
/// electing the channel it used to see.
#[test]
fn stale_observations_stop_voting() {
    let mut e = Elector::new(1);
    e.observe_self(0, &scan(&[(6, -70)]));
    for id in [2u8, 3, 4] {
        e.observe_peer(0, id, 0, 11, 0, &weights(&[(11, -60)]));
    }
    assert_eq!(e.winner(0), Some(11), "the majority is live");
    // Refresh only ourselves, well past the stale window.
    let later = OBS_STALE_MS + 1;
    e.observe_self(later, &scan(&[(6, -70)]));
    assert_eq!(e.winner(later), Some(6), "departed nodes stop counting");
    assert_eq!(e.members(later), 1);
}

/// Malformed and hostile fields must be rejected at the boundary, never indexed
/// with or trusted into election state.
#[test]
fn hostile_fields_are_rejected() {
    let mut e = Elector::new(7);
    // Out-of-range channels (would panic an unchecked index).
    for ch in [0u8, 14, 99, 255] {
        assert_eq!(
            e.observe_peer(0, 2, 1, ch, 0, &weights(&[(6, -60)])),
            Ingest::Rejected,
            "channel {ch} must be rejected"
        );
    }
    // Someone claiming to be us, and the reserved id 0.
    assert_eq!(e.observe_peer(0, 7, 1, 6, 0, &[0; N]), Ingest::Rejected);
    assert_eq!(e.observe_peer(0, 0, 1, 6, 0, &[0; N]), Ingest::Rejected);

    // Forged giant weights must clamp to the honest ceiling, so one liar cannot
    // outvote the fleet even before saturation applies.
    e.observe_self(0, &scan(&[(6, -60)]));
    let mut liar = [255u8; N];
    liar[ch_index(11).unwrap()] = 255;
    e.observe_peer(0, 2, 0, 6, 0, &liar);
    let t = e.tally(0);
    let ceil = weight(WEIGHT_CEIL_DBM);
    assert!(
        t[ch_index(11).unwrap()].sum <= ceil,
        "a forged 255 must clamp to {ceil}, got {}",
        t[ch_index(11).unwrap()].sum
    );
}

/// The cheap defence against the attack the design explicitly accepts elsewhere:
/// a hostile high-epoch frame naming a channel with no infrastructure must not
/// move us there. Costs one array read.
#[test]
fn refuses_adoption_onto_a_channel_nobody_can_use() {
    let mut e = Elector::new(1);
    // We have scanned: only ch6 has a usable AP.
    e.observe_self(0, &scan(&[(6, -55)]));
    // An attacker claims a huge epoch for ch13, and reports nothing usable.
    let got = e.observe_peer(0, 2, 999_999, 13, 0, &[0; N]);
    assert_eq!(got, Ingest::RefusedUnusable(13));
    assert_eq!(e.decision().channel, 6, "must not follow onto a dead channel");

    // But a peer that demonstrates ch13 IS usable is believed — the check is
    // "no evidence", not "not my idea".
    let mut e2 = Elector::new(1);
    e2.observe_self(0, &scan(&[(6, -55)]));
    assert!(matches!(
        e2.observe_peer(0, 2, 999_999, 13, 0, &weights(&[(13, -60)])),
        Ingest::Adopted(_)
    ));
}

/// A node with no scan data yet cannot afford to be picky, or a cold-booted node
/// would refuse every decision the fleet offers it.
#[test]
fn a_node_with_no_scan_data_still_adopts() {
    let mut e = Elector::new(1);
    assert!(matches!(
        e.observe_peer(0, 2, 3, 11, 0, &weights(&[(11, -60)])),
        Ingest::Adopted(_)
    ));
    assert_eq!(e.decision().channel, 11);
}

// ===========================================================================
// Boot: fast, always
// ===========================================================================

/// The "fast all the time" path: restore, and be converged before hearing anyone.
#[test]
fn restore_is_immediate_and_validates_input() {
    let mut e = Elector::new(1);
    e.restore(Decision { channel: 11, epoch: 42, gateway: 3 });
    assert_eq!(e.decision(), Decision { channel: 11, epoch: 42, gateway: 3 });
    assert!(!e.on_probation(), "our own persisted choice is not on probation");

    // A corrupt persisted record must not be honoured.
    let mut e2 = Elector::new(1);
    e2.restore(Decision { channel: 0, epoch: 5, gateway: 0 });
    assert_eq!(e2.decision(), Decision::bootstrap());
    let mut e3 = Elector::new(1);
    e3.restore(Decision { channel: 200, epoch: 5, gateway: 0 });
    assert_eq!(e3.decision(), Decision::bootstrap());
}

/// With no AP visible at all there is nothing to elect, so we must sit on the
/// rendezvous channel rather than churn — an infrastructure-less fleet still
/// meets.
#[test]
fn no_usable_ap_holds_the_rendezvous_channel() {
    let mut e = Elector::new(1);
    e.observe_self(0, &scan(&[(1, -95), (6, -99)]));
    assert_eq!(e.winner(0), None);
    assert_eq!(e.step(0), None);
    assert_eq!(e.decision().channel, RENDEZVOUS_CHANNEL);
    assert_eq!(e.decision().epoch, 0, "nothing to elect spends no epoch");
}

// ===========================================================================
// Gateway
// ===========================================================================

/// Lowest node id wins, so two simultaneous claims resolve with no round trip.
#[test]
fn gateway_claims_break_to_lowest_id() {
    let mut e = Elector::new(1);
    assert!(e.claim_gateway(20).is_some(), "first claim takes an empty role");
    assert_eq!(e.decision().gateway, 20);
    assert!(e.claim_gateway(30).is_none(), "a higher id must not steal it");
    assert_eq!(e.decision().gateway, 20);
    assert!(e.claim_gateway(10).is_some(), "a lower id wins deterministically");
    assert_eq!(e.decision().gateway, 10);
    assert!(e.claim_gateway(0).is_none(), "id 0 is the none-sentinel");
}

/// A gateway that lost its uplink must stand down rather than black-hole traffic.
#[test]
fn gateway_relinquishes_when_it_loses_its_uplink() {
    let mut e = Elector::new(1);
    e.claim_gateway(10);
    assert!(e.relinquish_gateway(99).is_none(), "not the incumbent, no-op");
    assert_eq!(e.decision().gateway, 10);
    assert!(e.relinquish_gateway(10).is_some());
    assert_eq!(e.decision().gateway, 0, "role is vacant, not black-holed");
}

/// A gateway can be learned from a same-epoch peer without spending an epoch —
/// only a *claim* bumps. Otherwise every node learning the gateway would burn an
/// epoch and the fleet would never go quiet.
#[test]
fn gateway_is_learned_without_spending_an_epoch() {
    let mut e = Elector::new(1);
    e.observe_self(0, &scan(&[(6, -60)]));
    e.restore(Decision { channel: 6, epoch: 4, gateway: 0 });
    e.observe_peer(0, 2, 4, 6, 17, &weights(&[(6, -60)]));
    assert_eq!(e.decision().gateway, 17);
    assert_eq!(e.decision().epoch, 4, "learning is free");
}

/// A channel change invalidates reachability, so the gateway must be re-learned
/// on the new channel rather than assumed to have followed us.
#[test]
fn channel_change_clears_the_gateway() {
    let mut e = Elector::new(1);
    e.restore(Decision { channel: 6, epoch: 1, gateway: 17 });
    e.observe_self(0, &scan(&[(6, -75), (11, -50)]));
    e.observe_peer(0, 2, 0, 11, 0, &weights(&[(11, -55)]));
    e.step(0);
    let d = e.step(SETTLE_MS + 1).expect("commits after the settle window");
    assert_eq!(d.channel, 11);
    assert_eq!(d.gateway, 0, "gateway must not be assumed across a channel move");
}

// ===========================================================================
// Whole-fleet convergence
// ===========================================================================

/// The acceptance test the spec asks for, in simulation: a fleet with asymmetric
/// views, gossiping, must all land on the same channel and then stay there.
#[test]
fn fleet_converges_and_then_holds() {
    // Four nodes with overlapping but different views. Only ch6 is seen by all.
    let views: [(u8, &[(u8, i8)]); 4] = [
        (10, &[(1, -45), (6, -70)]),
        (20, &[(6, -60), (11, -55)]),
        (30, &[(6, -65), (11, -75)]),
        (40, &[(1, -80), (6, -58)]),
    ];
    let mut fleet: Vec<Elector> = views.iter().map(|(id, aps)| node(*id, aps)).collect();

    // Gossip: several rounds of everyone broadcasting to everyone (all on the
    // same channel here — the merged case).
    for round in 0..3u64 {
        let t = round * 2_000;
        let frames: Vec<(u8, Decision, [u8; N])> = fleet
            .iter()
            .map(|e| (e.self_id(), e.decision(), e.self_weights()))
            .collect();
        for e in fleet.iter_mut() {
            for (id, d, w) in &frames {
                if *id != e.self_id() {
                    e.observe_peer(t, *id, d.epoch, d.channel, d.gateway, w);
                }
            }
        }
        for e in fleet.iter_mut() {
            e.step(t);
        }
    }

    let winners: Vec<Option<u8>> = fleet.iter().map(|e| e.winner(4_000)).collect();
    assert!(
        winners.iter().all(|w| *w == winners[0]),
        "every node must elect the same channel, got {winners:?}"
    );
    assert_eq!(winners[0], Some(6), "ch6 is the only channel all four can use");

    // Now let it run a long time with no input change: nobody may keep bumping
    // epochs. "Converge, then STOP."
    let epochs_before: Vec<u32> = fleet.iter().map(|e| e.decision().epoch).collect();
    for round in 0..40u64 {
        let t = 10_000 + round * 2_000;
        for e in fleet.iter_mut() {
            e.observe_self(t, &scan(views[0].1)); // refresh so nothing goes stale
        }
        let frames: Vec<(u8, Decision, [u8; N])> = fleet
            .iter()
            .map(|e| (e.self_id(), e.decision(), e.self_weights()))
            .collect();
        for e in fleet.iter_mut() {
            for (id, d, w) in &frames {
                if *id != e.self_id() {
                    e.observe_peer(t, *id, d.epoch, d.channel, d.gateway, w);
                }
            }
            e.step(t);
        }
    }
    let epochs_after: Vec<u32> = fleet.iter().map(|e| e.decision().epoch).collect();
    let bumps: u32 = epochs_before
        .iter()
        .zip(&epochs_after)
        .map(|(a, b)| b - a)
        .sum();
    assert!(
        bumps <= fleet.len() as u32,
        "a converged fleet must go quiet; saw {bumps} epoch bumps across 40 rounds"
    );
    let chans: Vec<u8> = fleet.iter().map(|e| e.decision().channel).collect();
    assert!(
        chans.iter().all(|c| *c == chans[0]),
        "still agreed after settling, got {chans:?}"
    );
}

/// This firmware's stack margin is single-digit KB and its main heap pool has
/// been measured at ~6.5 KB free, so "how big is this" is a design constraint,
/// not trivia. Asserted rather than commented, because a future `MAX_NODES` bump
/// would otherwise spend stack silently.
#[test]
fn state_footprint_is_bounded() {
    let sz = core::mem::size_of::<Elector>();
    assert!(
        sz <= 320,
        "Elector grew to {sz} B — check the stack budget before raising this"
    );
    // u32 epoch + two u8s, so the whole decision is one 8-byte word — cheap to
    // pass by value, cheap to persist.
    assert_eq!(core::mem::size_of::<Decision>(), 8);
    eprintln!("Elector = {sz} B");
}

/// Two partitions that each converged separately must merge to ONE channel when
/// they finally hear each other, rather than fighting.
#[test]
fn two_partitions_merge_to_one_channel() {
    // Partition A settled on ch1 at epoch 3; partition B on ch11 at epoch 5.
    let mut a = node(10, &[(1, -50), (11, -70)]);
    a.restore(Decision { channel: 1, epoch: 3, gateway: 0 });
    let mut b = node(20, &[(1, -70), (11, -50)]);
    b.restore(Decision { channel: 11, epoch: 5, gateway: 0 });

    // A sweep puts them in earshot; each ingests the other.
    a.observe_peer(0, 20, 5, 11, 0, &b.self_weights());
    b.observe_peer(0, 10, 3, 1, 0, &a.self_weights());

    assert_eq!(a.decision().channel, 11, "A adopts the higher epoch");
    assert_eq!(b.decision().channel, 11, "B keeps its own, higher, epoch");
    assert_eq!(a.decision().epoch, b.decision().epoch);
}
