# Radio Scan (Zigbee/Thread 802.15.4 monitor) — Design Spec

Date: 2026-07-20
Status: draft for JP review
Feasibility basis: `scratch/zigbee-thread/nebula.md` (verified against esp-radio 0.18 source)
Depends on: Slint UI migration merged to `main` (plan tasks T9 cutover, T10 on-demand framebuffer)

## Goal

Add a **passive 802.15.4 monitor** to the watch — a "Radio Scan" mode that
sniffs nearby Zigbee/Thread networks and shows PAN IDs, channels, RSSI, and
device/beacon counts. Entered deliberately from the launcher, gated behind a
confirm screen, because it **cannot coexist** with the WiFi/BLE/ESP-NOW mesh.

## Context (verified)

- The ESP32-C6 has a native 802.15.4 radio. The driver already ships in our
  pinned `esp-radio 0.18.0` as the `ieee802154` module (feature-gated; needs
  `unstable`, which we already enable). Chip-gated to c6/c61/h2 — **C6 works**.
  Enabling it is a one-line Cargo feature add. No new crate.
- **Coexistence is impossible** at our versions. esp-radio's own module docs
  say verbatim: *"Coexistence with Wi-Fi or Bluetooth is currently not
  possible. If you do it anyway, things will break."* The `coex` feature we
  already ship is WiFi↔BLE only; it does not extend to 802.15.4. All three
  radios share the single 2.4 GHz PHY. → monitoring must be a **dedicated
  mode** that tears the mesh down first.
- The watch already runs a **time-share radio design**: WiFi bursts (NTP/MQTT/
  weather) then drops its association; ESP-NOW mesh pins to a fixed channel;
  BLE advertises. So "the mesh" to pause = ESP-NOW + WiFi + BLE.
- Passive limits (physics, not tooling): 802.15.4 MAC headers (PAN ID, source
  addr, channel, RSSI) are cleartext; Zigbee NWK and Thread MLE **payloads are
  encrypted**. We can map *who transmits where and how loud*, never *what*.
  Zigbee-vs-Thread is a **best-effort guess**, surfaced with a "?" in the UI.
- API surface (esp-radio 0.18 `ieee802154`): `Ieee802154::new(peripherals
  .IEEE802154)`, `set_config(Config { promiscuous: true, channel, rx_queue_size,
  .. })`, `start_receive()`, `received() -> Option<Result<ReceivedFrame>>`
  (decoded MAC header + `channel` + `rssi: i8` + `lqi: u8`) and
  `raw_received() -> Option<RawReceived>`. No standalone energy-detect *scan*
  API — per-channel "energy" is derived from received-frame RSSI + counts.

## Decision: Slint overlay, not an embedded-graphics AppState app

**Radio Scan is a Slint modal overlay reachable from the launcher — NOT an
`App`-trait eg app (like the games), and NOT a 6th carousel page.**

Rationale (grounded in the current shell, `src/ui/slint_shell.rs` +
`ui/slint/shell.slint`):

1. **It's live-data UI, and the shell is Slint.** The Mesh page already does
   exactly this shape: a `VecModel<PeerRow>` swapped in place every 1 s
   (`ShellUi::set_mesh_rows`). Radio Scan's PAN list is the same pattern with
   a different row struct. eg apps, by contrast, are full-frame real-time
   *animations* that own the panel and bypass Slint's scene (the reason
   `request_redraw()` exists). A data readout does not want that.
2. **The overlay primitive already exists.** Launcher and AOD are both
   `if root.<flag>: Overlay {}` modals layered over the page carousel
   (shell.slint:250, :256). Radio Scan is a third such overlay
   (`if root.scan-open`). This directly models JP's "dedicated mode, explicit
   enter/exit" — no new UI architecture.
3. **The confirm + results screens are declarative** — trivial in Slint,
   tedious in eg. Reusing `theme.slint` keeps it visually part of the shell.
4. **RAM.** eg apps allocate the ~202 KB framebuffer on entry (T10 on-demand
   model). A Slint overlay renders through the existing 1.6 KB line-flusher and
   needs **no framebuffer**. Because scan mode also tears WiFi down (tens of KB),
   it is **net-negative on heap** — a rare feature that frees memory while active.

Trade-off acknowledged: it does *not* reuse the `App` trait, so launcher→app
dispatch needs one special case (below). That cost is small and worth it.

## Architecture

### Entry point — launcher item, overlay target

- Add `AppState::RadioScan` to `src/apps/mod.rs`.
- Add it to `LAUNCHER_APPS` in `slint_shell.rs` **and** the `for` list in
  `ui/slint/launcher.slint` — these two are kept in lock-step by an explicit
  comment contract; both must change together, in the same order.
- In `main.rs`'s launch dispatch (post-T9), `AppState::RadioScan` is
  special-cased: instead of constructing an eg `App` + framebuffer, it sets
  `ShellUi` into scan mode (`scan_open = true`, `scan_phase = Warn`). The
  overlay lives in the shell's render path; the loop stays in "shell mode".

### State machine (`scan_phase`, an `in` enum-ish int property on the shell)

```
        launcher tap "Radio Scan"
                  │
                  ▼
            ┌──────────┐   Back / swipe-right      ┌────────┐
            │  Warn    │ ─────────────────────────▶│ closed │ (radios untouched)
            │ (confirm)│                            └────────┘
            └────┬─────┘
                 │ Confirm  → req.scan_confirm
                 ▼
          ┌─────────────┐  teardown fails (retries exhausted)
          │  Restoring? │◀───────────────┐
          └──────┬──────┘                 │
                 │ mesh down + 15.4 up     │
                 ▼                         │
           ┌───────────┐   Exit / swipe-right → req.scan_exit
           │ Scanning  │ ────────────────────────────────┐
           │ (live)    │                                  │
           └───────────┘                                  ▼
                                                   ┌──────────────┐
                                                   │  Restoring   │ tear down 15.4,
                                                   │              │ bring mesh back
                                                   └──────┬───────┘
                                                          │ ok → closed
                                                          │ fail → Error screen
                                                          ▼        (offer Reboot)
```

Phases: `Warn`, `Scanning`, `Restoring`, `Error`. (`Restoring` shows a brief
"Switching radio…" spinner in both directions; it exists so the UI never looks
frozen during the ~hundreds-of-ms radio swap.)

**Warn screen (REQUIRED by JP):** full-screen, text ≈ *"Radio Scan — WiFi & BLE
will turn OFF while scanning. The mesh pauses until you exit."* Two actions:
**Scan** (primary) → `req.scan_confirm`; **Back** → close overlay, radios never
touched. Back is also swipe-right (matches launcher's close gesture).

### Radio lifecycle (the crux — owned by `main.rs`, not the UI)

`main.rs` owns all peripherals and the radio state; the UI only emits requests.
New module suggestion: `src/net/radio_mode.rs` encapsulating the switch.

Boot ownership (verified, `main.rs`):
- `peripherals.WIFI` → consumed by `esp_radio::wifi::new(..)` (main.rs:436).
- `peripherals.BT` → consumed by `BleConnector::new(..)` (main.rs:438).
- `peripherals.IEEE802154` → **currently unused**; reserve it at boot into a
  slot so scan mode can take it without a `steal()`.

**Enter scan (on `scan_confirm`):**
1. Stop the ESP-NOW mesh task / stop broadcasting; set mesh "paused".
2. `wifi_controller.disconnect_async().await` then **drop** `wifi_controller`
   + `wifi_interfaces` + the embassy-net runner (frees the WiFi side of the PHY).
   This is the teardown the main.rs comment (lines ~856-859) describes but the
   firmware has never actually exercised at runtime — see Risks.
3. Stop BLE advertising / drop the BLE host connector.
4. `Ieee802154::new(ieee_radio)`; `set_config(Config { promiscuous: true,
   rx_queue_size: 16, channel: 11, ..default })`; `start_receive()`.
5. `scan_phase = Scanning`; launch the scan engine (below).

**Exit scan (on `scan_exit`):**
1. **Drop** the `Ieee802154` (frees the PHY; `Drop` clears callbacks).
2. Re-create WiFi: `esp_radio::wifi::new(unsafe { peripherals::WIFI::steal() }, ..)`
   (esp-hal 1.x peripheral singletons expose `steal()`); re-create embassy-net
   stack/runner; re-arm BLE advertising; resume the ESP-NOW mesh on its fixed
   channel.
3. `scan_phase = Restoring` during step 2; on success → close overlay
   (`scan_open = false`), toast "Mesh resumed".

### Failure handling — what if mesh restore fails?

Restore is the fragile path (re-init after drop is unproven — see Risks).
Ladder:
1. **Retry** the WiFi/mesh re-init up to N=3 times with a short backoff.
2. If still failing → `scan_phase = Error`, screen: *"Mesh restore failed"* with
   a single **Reboot** button. Boot is the known-good, fully-deterministic radio
   init path (all three stacks come up clean from reset), and the RTC is
   battery-backed so time survives; the mesh re-syncs in seconds. Reuse the
   existing reboot flow (`reboot_tap` → `esp_hal::reset` /
   software reset already wired for the power page).
3. Safety net: an auto-timeout in `Scanning` (e.g. 90 s of no interaction)
   auto-triggers `scan_exit`, so the watch can never silently sit off-mesh
   forever if the user walks away.

Open question for JP: **live-restore-with-reboot-fallback (recommended)** vs
**always soft-reboot on scan exit** (dead simple + deterministic, but loses
uptime and blinks the screen). Recommend the former; flagging because the
latter is a legitimately safer v1 if the hardware spike shows re-init is flaky.

### Scan engine (channel hop) — on the existing esp-rtos/embassy executor

No new scheduler: reuse `embassy_time` on the executor `esp_rtos::start` already
launched at boot (main.rs:265).

- Sweep channels **11–26** (802.15.4 2.4 GHz band).
- **Dwell 300 ms/channel** (JP's 250–400 ms range); full sweep ≈ 4.8 s.
  Configurable const. Optionally a "park on channel N" mode (tap a channel in
  the strip) for sustained watch of one PAN.
- Per dwell: apply channel (re-`set_config` with the new `channel`; the public
  API has no lighter per-field setter — acceptable at ~3 Hz), then poll
  `received()` in a tight loop until the dwell timer fires, folding each frame
  into the data model.
- Runs as its own `embassy` task (or inline in the shell loop branch); pushes
  the model into Slint via `ShellUi` setters at ≤2 Hz to bound alloc churn
  (same discipline as the mesh page's on-page gating).

### Data model (Rust, capped, no_std)

```rust
struct PanEntry {
    pan_id: u16,
    channel: u8,
    last_rssi: i8,
    rssi_ewma: i8,           // smoothed
    frames: u32,
    beacons: u16,
    devices: heapless::FnvIndexSet<u16, 8>, // distinct source short addrs (cap 8)
    guess: PanKind,          // Unknown | ZigbeeLikely | ThreadLikely
}
struct ChannelStat { frames: u32, peak_rssi: i8 } // [ChannelStat; 16], idx = ch-11

// top-level scan state
pans: heapless::Vec<PanEntry, 16>,   // cap 16 PANs; LRU-evict weakest on overflow
channels: [ChannelStat; 16],
total_frames: u32,
```

Dedup key = `pan_id` (+ channel). New source addr → insert into `devices`
(ignore on full). Everything is fixed-capacity `heapless` — no dynamic growth.

**Zigbee vs Thread heuristic (best-effort, labeled "?"):**
- `ZigbeeLikely` if we see a **beacon** frame whose beacon payload carries the
  Zigbee stack profile / protocol-ID signature.
- `ThreadLikely` if we see 2015-frame-version secured **data** frames matching
  Thread patterns (6LoWPAN dispatch, MLE-style), absent Zigbee beacons.
- else `Unknown` → UI shows plain "802.15.4". Never assert a hard label.

### UI surface (`ui/slint/scan.slint`, imported by shell.slint)

- **Warn phase:** title, the WiFi/BLE-off warning body, `Scan` + `Back`.
- **Scanning phase:**
  - top: 16-cell channel strip (ch 11–26), cell height/brightness ∝ frame count
    (energy proxy), busiest highlighted in `Theme.accent`.
  - middle: scrollable PAN list (Flickable, mesh-page row pattern): each row
    `PANID 0x1A2B · ch15 · -62 dBm · 3 dev · Zigbee?`.
  - footer: `Mesh paused · {total} frames · sweep {n}s` + Exit.
  - empty state (mirrors mesh page): "NO 802.15.4 TRAFFIC / sweeping 11–26".
  - a persistent small caption: *"passive · headers only · payloads encrypted"*
    so the honesty limit is on-screen, per JP.
- **Restoring/Error phases:** spinner + text; Error adds the Reboot button.
- New shared row struct in `theme.slint` (e.g. `ScanRow { pan, meta, kind }`),
  same reasoning as `PeerRow` living there.

### ShellUi API additions (`src/ui/slint_shell.rs`)

- `ShellRequests`: `scan_confirm: Cell<bool>`, `scan_exit: Cell<bool>`,
  `scan_reboot: Cell<bool>` (or reuse `reboot`), optional `scan_park: Cell<Option<u8>>`.
- Methods: `set_scan_open(bool)`, `set_scan_phase(ScanPhase)`,
  `set_scan_channels(&[ChannelStat;16])`, `set_scan_pans(&[PanEntry])`
  (gated on `scan_open`, mirrors the mesh-rows gating), `set_scan_footer(..)`.
- Slint callbacks: `scan-confirm()`, `scan-exit()`, `scan-reboot()`,
  wired into the request cells exactly like `wifi-tap`/`launch-app`.

## Memory strategy

- Scan working set: `pans` 16×~34 B ≈ 550 B; `channels` 16×5 B = 80 B;
  rx queue 16×~131 B ≈ 2.1 KB; Slint models small → **~3–4 KB live**.
- **No framebuffer** (Slint path). WiFi stack (tens of KB) is down during scan.
  → scan mode's heap footprint is **below** normal shell+WiFi idle. Verify with
  the system page's live heap readout at each transition.

## Testing

- **Host unit tests (pure, no HW):** MAC-header parse → `PanEntry` folding;
  the Zigbee/Thread classifier over canned frame byte-arrays; channel-hop
  index math; list capping/dedup/LRU-evict. These are pure functions — a small
  `#[cfg(test)]` module is worth it even though the repo has no suite today
  (propose, don't impose a framework: `std`-side `cargo test` on the parser
  module only). Flag for JP.
- **Hardware is the real gate** (matches the migration spec's stance):
  1. Flash to C6; open launcher → Radio Scan → confirm the **Warn** screen
     appears and **Back leaves radios untouched** (WiFi dot state unchanged).
  2. Confirm → verify via boot log that WiFi/BLE tore down and 15.4 came up.
  3. Near a **known** Zigbee coordinator (JP's HA/ZHA/deCONZ) and/or a Thread
     border router: verify captured **PAN ID + channel match the coordinator's
     actual config** — this is the definitive correctness check, not vibes.
  4. Verify RSSI values are sane (stronger closer), device counts plausible.
  5. Exit → verify mesh restores: WiFi reconnects, peers reappear, NTP resyncs.
  6. Measure heap (system page) before/during/after; measure flash delta
     (`cargo size` / espflash) vs current binary — **measure, don't trust the
     ~20–50 KB estimate**.
  7. Force a restore failure (e.g. exit rapidly) to exercise the Error→Reboot
     path.

## Risks

1. **Re-init after drop is unproven in this firmware (highest risk).** The
   drop-and-recreate teardown is documented but never exercised at runtime;
   WiFi's controller currently lives for the whole loop. Whether
   `esp_radio::wifi::new(WIFI::steal())` cleanly re-initializes after a mid-run
   drop MUST be de-risked by a standalone hardware spike **before** any UI work.
   If it's flaky, fall back to "always soft-reboot on scan exit".
2. **802.15.4 Rust stack is young** — `unstable`-gated, `#![allow(missing_docs)]`,
   `TODO`s in-tree, no coex. Expect churn across esp-radio bumps; pin carefully.
3. **Zigbee/Thread classification is heuristic** — will mislabel; UI must never
   present it as authoritative (hence the "?" + caption).
4. **Flash growth** could bump against partition sizing (see partitions.csv) —
   measure; the ota/app partition must still fit.

## Migration touchpoints (this feature depends on the Slint migration)

- **Must land after** T9 (main.rs shell cutover) — the launch dispatch + shell
  render loop it hooks into only exists post-cutover. Ideally after T10
  (on-demand framebuffer) since that finalizes the shell/app seam, and after
  T13 (dead eg deletion) to avoid wiring into soon-deleted code.
- Files touched: `src/apps/mod.rs` (+`RadioScan`), `src/ui/slint_shell.rs`
  (`LAUNCHER_APPS`, `ShellRequests`, scan setters), `ui/slint/launcher.slint`
  (+item, lock-step), `ui/slint/shell.slint` (+`scan-open`/`scan-phase` +
  overlay), new `ui/slint/scan.slint`, `ui/slint/theme.slint` (+`ScanRow`),
  `src/main.rs` (radio lifecycle + dispatch), new `src/net/radio_mode.rs`,
  `Cargo.toml` (+`ieee802154` feature on esp-radio).
- ShellUi API is additive — no changes to existing page methods.

## Open questions for JP

1. **Restore strategy:** live-restore-with-reboot-fallback (recommended) vs
   always-reboot-on-exit (simpler/safer if re-init is flaky)? Decide after the
   spike, but your lean helps scope.
2. **Scope of the guess:** ship the Zigbee/Thread heuristic in v1, or v1 shows
   only "802.15.4 PAN" (channel/RSSI/PAN/devices) and classification is a v2?
   (Recommend: ship channel/PAN/RSSI/counts solid; classification behind the
   "?" as a bonus, clearly best-effort.)
3. **Channel park / TX:** passive-only in v1 (no beacon-request TX), confirmed?
   Active beacon-request would surface non-beaconing PANs faster but is TX, not
   pure monitoring. (Recommend: passive-only v1.)
4. **Auto-exit timeout** value (proposed 90 s) acceptable?
```
