# Mesh consensus: no pinning, fleet-wide AP + gateway election

**Goal (JP, 2026-07-29):** "the mesh should vote and come to consensus on which ap
and node to use as a gateway. all nodes that see any other node should join the
same mesh… a working mesh fast all the time." No channel pinning, no BSSID pinning.

Spans TWO repos: `esp32c6-watch` (C6 watches) and `smol` (C3 nodes). The wire
format must be implemented identically in both or the fleet partitions.

## The constraint that makes this simple

**The ESP32 has ONE radio. ESP-NOW transmits and receives on whatever channel the
WiFi STA is currently on.** Therefore:

> **Choosing the AP *is* choosing the ESP-NOW channel.**

Two nodes associated to the same BSSID are automatically on the same channel and
can always hear each other. Two nodes on different APs on different channels
*cannot*, no matter what the mesh code does. So the entire mesh-partition problem
reduces to: **make the whole fleet agree on one AP.**

This is why JP's instinct is right and the current design is wrong. Today each node
independently picks its own "best BSSID" by local RSSI, so a fleet spread across a
roaming SSID lands on different channels and the mesh silently partitions. Observed:

    watch A: best BSSID a4:2b:b0:b7:93:2e ch6 rssi-44 (pinning)
             pinned BSSID failed 2x - falling back to driver select
             best BSSID 9c:5c:8e:cb:db:90 ch1 rssi-63 (pinning)   <-- now on ch1

...while the mesh wanted ch6. The two watches never heard each other; every peer
that acked them was a smol node or the gateway.

## Design

### 1. Deterministic election, not a voting protocol

Do NOT implement multi-round consensus (Raft/Paxos-style). On a lossy broadcast
medium with no reliable delivery, use a **pure function of shared observations** —
same inputs produce the same winner on every node, so convergence needs no
agreement rounds, only information spread.

Each node periodically broadcasts its observations. The election key is computed
locally by everyone:

    score(AP) = SUM over nodes n that can see AP of weight(rssi(n, AP))
    winner    = argmax score, tie-broken by LOWEST BSSID (total order, no ties)

Summing per-node RSSI *is* the vote: a node "votes" for the APs it can actually
reach, weighted by how well. An AP that only one node can see loses to one the
whole fleet sees, which is exactly the property we want and precisely what
per-node local choice fails to deliver.

`weight()` should be monotone but saturating (e.g. clamp to a floor around -85 dBm
and cap the top) so one very close node cannot outvote the rest of the fleet.

### 2. Epoch + hysteresis: converge, then STOP

Flapping is worse than a suboptimal choice — every switch costs an association and
tears the mesh down.

- Every decision carries a monotonically increasing **epoch**.
- **Higher epoch wins.** A late joiner adopts the established decision rather than
  re-running the election, which is what "all nodes that see any other node join
  the same mesh" requires.
- On partition merge, higher epoch wins; equal epoch breaks by larger member count,
  then lowest BSSID.
- Only bump the epoch when the winner would change **and** the new candidate beats
  the incumbent by a margin (suggest >= 6 dB aggregate) **and** has held that lead
  for a settle window (suggest >= 30 s).

### 3. Rendezvous: the bootstrap problem

You cannot vote before you can hear each other, and you cannot hear each other
before you share a channel. Resolve in this order:

1. **Fast path — persisted last-known-good.** Store `{bssid, channel, epoch}` in
   config. On boot, adopt it IMMEDIATELY and start meshing (target: talking within
   ~1 s of radio up). This is what makes it "fast all the time"; a fleet that was
   converged yesterday is converged instantly today.
2. **Listen sweep.** If nobody answers within a short window, sweep channels
   listening for SMOLv1 traffic. On hearing any peer, adopt its epoch/decision.
3. **Deterministic fallback channel.** With no AP usable at all, derive a channel
   from a hash of the SSID (or keep 6) so an infrastructure-less fleet still meshes.
   This is a RENDEZVOUS default, not a pin: it is used only when there is nothing
   to elect, and is abandoned the moment an elected AP exists.

### 4. Never let WiFi starve the mesh

Today's `mesh_pin_ok` yields the radio to ANY WiFi intent, so a node that retries
association forever (our current bug) never meshes at all. New rule: the mesh
follows the STA channel continuously and is never gated on association *success*.
Association attempts must not silence ESP-NOW.

### 5. Gateway election — same machinery

Score by uplink quality, not by who booted first:
`associated` AND `has working uplink (MQTT/broker reachable)` AND RSSI, tie-broken
by lowest node id, carried under the same epoch/hysteresis rules. Exactly one
gateway per epoch; others are leaves. A gateway that loses its uplink must
relinquish (bump epoch) rather than black-hole traffic.

### 6. Remove BSSID pinning — and note it may fix the WiFi bug too

`net_task`'s "best BSSID (pinning)" + `PIN_MAX_FAILS` fallback fights the driver's
own roaming logic on an 802.11r SSID with multiple BSSIDs. Association is failing
at **-44 dBm**, which is not a signal problem. Removing the pin in favour of
associating by SSID (letting the driver pick, then *reporting* which BSSID/channel
it landed on into the election) is both the de-pinning JP asked for and a strong
candidate fix for the association failure.

Note the ordering: nodes report the channel they ACTUALLY got, and the election
runs over reality rather than over intentions.

## Wire format (must match in both repos)

Extend SMOLv1 with one frame. Keep it ASCII-prefixed like the rest of the protocol
so old nodes ignore it harmlessly, and keep it under the 250 B ESP-NOW payload cap:

    SMOLv1 ELECT <node_id> <epoch> <chosen_bssid> <chosen_ch> <gw_id> <n_cands> [<bssid> <ch> <rssi>]*

Cap the candidate list to what fits (~6-8 entries) and send the strongest first.

## Explicit non-goals

- Not a general consensus algorithm; no leader lease, no log replication.
- Not a mesh routing protocol — this decides CHANNEL and GATEWAY only.
- No cryptographic authentication in v1 (ESP-NOW here is unencrypted,
  `lmk: None`). NOTE: an unauthenticated election frame lets a hostile broadcaster
  drag the fleet to a channel of its choosing. Accept for a home fleet, but record
  it, and bound any state the frames can allocate (see the `peers` DoS finding).

## Acceptance

1. Two watches + smol nodes, cold boot: all on the SAME channel and mutually
   visible within seconds, with no manual channel config.
2. Ping between the two watches works from boot, not only after the 180 s WiFi
   burst gives up.
3. Kill the elected AP: fleet re-converges on another and the mesh recovers.
4. No flapping: with two comparable APs, the elected choice is stable for minutes.
5. Association at -44 dBm succeeds (de-pinning validated).

---

# Addendum: revisions from implementation (Morpheus, 2026-07-29)

Six places the design above needed correcting. Each is implemented and covered by
a host test in `crates/mesh-elect/tests/`.

## R1. Elect a CHANNEL, not a BSSID (§1, §6)

Only the channel is load-bearing — any AP on the elected channel puts us on it,
which is all the mesh needs — and enforcing a BSSID costs the driver its roaming
freedom, i.e. it *is* the pinning we set out to remove.

Worse, per-BSSID scoring picks wrong on precisely our network. With one roaming
SSID over ~12 APs, node A's strongest ch6 AP is usually a *different* BSSID from
node B's strongest ch6 AP, so a summed per-BSSID score cannot see that they agree
on the channel: a weak AP both happen to share can outscore a channel both see
strongly.

Enforcement is therefore `with_channel()` with `bssid_set = false`, which in
ESP-IDF is a scan *hint*, not a target. That also fails safe — a hint cannot
wedge the connect, a pin can.

## R2. Score count-first, not by plain sum (§1)

The proposed `score = SUM of saturating per-node weights` does not deliver its own
stated goal. On a 0..48 weight scale one node at -35 dBm scores 48 and beats three
nodes at -70 dBm (16 each). Saturation alone cannot fix this; the comparison has
to be lexicographic:

    score(ch) = ( number of nodes that can USE ch , sum of their weights )
    winner    = max score, tie-broken by LOWEST channel number

Count dominates, so the winner is always the channel the most nodes can join —
literally what "all nodes that see any other node join the same mesh" asks for.
"Can use" is gated at -82 dBm (smol's proven `AP_USABLE_MIN`) so a barely-audible
channel cannot win on headcount. Tie-break is the channel *number*, not a BSSID:
a total order that is also stable across scans, since BSSIDs move and channel
numbers do not.

## R3. Margin must scale with fleet size (§2)

A flat ">= 6 dB aggregate" gets *easier* to trip as the fleet grows — with five
reporters it is barely 1 dB each. Implemented as
`max(MARGIN_FLOOR, incumbent_sum / 8)`. Also: a challenger with strictly MORE
voters is a connectivity win rather than a signal preference, so it needs only the
settle window, not the dB margin.

## R4. A monotonic epoch needs an escape hatch (§2) — NOT IN THE ORIGINAL

"Higher epoch wins" unconditionally means one node holding a high persisted epoch
for a channel that no longer exists can pin the whole fleet onto a dead channel
**forever**. Added probation: an *adopted* decision that produces no peers for
`PROBATION_MS` (45 s) is abandoned and we re-elect at a higher epoch. Adopted
decisions only — our own persisted choice is not on probation.

Related constraint discovered by a test that hung: staleness must outlive both
decision windows (`OBS_STALE_MS > SETTLE_MS`, `> PROBATION_MS`), or a challenger's
supporting observations expire before its settle window closes and the machine can
never finish deciding. Asserted at compile time.

## R5. One node is not a quorum — NOT IN THE ORIGINAL

A node that has heard nobody knows only what its own antenna sees. That is enough
to AGREE with a decision, never enough to move the fleet off one. Without this, a
watch carried into another building elects its new local channel and abandons the
fleet it left.

This is also what makes the change **safe to deploy one repo at a time**: until
smol also speaks ELECT, the watch hears no ELECT peers, so it stays on the
rendezvous channel (ch6) instead of unilaterally walking off it and *causing* the
partition this work exists to fix. The feature arms itself when the fleet is
ready. Adoption is deliberately exempt, so a lone node can still rejoin.

## R6. The wire frame is fixed-size, so there is nothing to cap

Because we elect a channel, the candidate set is the 13 channels of the 2.4 GHz
band — a constant. The frame is therefore exactly 61 bytes with no length field
and no repetition, and the spec's "cap the candidate list to 6-8 entries" concern
disappears by construction rather than by discipline:

    "SMOLv1 ELECT " <id:3> ' ' <epoch:10> ' ' <ch:2> ' ' <gw:3> ' ' <w:26>

Tag byte 7 is `'E'`, unused by every existing SMOLv1 frame in both repos, so old
firmware ignores it harmlessly. Pinned against golden bytes in `tests/wire.rs` —
a round-trip test alone would pass happily while both repos agreed on the wrong
thing.

## Security posture (unchanged, now explicit)

ELECT frames are unauthenticated and that is accepted for a home fleet, but three
mitigations are built in at zero cost: per-node dedupe by id, claimed weights
clamped to the honest ceiling, and **refusal to adopt a channel our own scan found
no usable AP on**. That last one defeats the most damaging attack (drag the fleet
to a channel with no infrastructure) for one array read. A determined attacker
forging multiple node ids can still influence the outcome; that is not fixable
without a shared key.

## Deliberate cost

De-pinning trades away #57's *targeted* BSSID roam — without `bssid_set` we cannot
force a specific AP, only a channel. The driver's own sort-by-signal plus a
one-channel scan replaces it. If room-to-room roaming regresses on glass, the fix
is a narrowly-scoped exception, not restoring the default pin.
