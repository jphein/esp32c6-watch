# Radio Scan — Implementation Plan (executable, post-migration)

Date: 2026-07-20
Status: build-ready draft for JP review (docs-only; do NOT implement yet)
Spec: `docs/superpowers/specs/2026-07-20-radio-scan-design.md` (companion design;
       committed on `main` — will coexist once the migration merges)
Base: `feat/slint-shell` @ 78356f2 (integration HEAD)
Prereq: Slint migration merged; the app-launch fb-fix (task #27, morpheus) landed.

Incorporates oracle's de-risk findings (GAP-A, GAP-B) and JP's adopted defaults:
**live-restore with reboot fallback · Zigbee/Thread classifier deferred to v2 ·
passive-only v1 · 90 s auto-exit.**

Each task is a small, compiling, revertible commit on a `feat/radio-scan` branch
cut from `main` after the migration lands. **RS0 gates everything** — its two
decision gates (WiFi re-init, BLE teardown) set the shape of RS3/RS6.

---

## RS0 — Radio-release spike (DE-RISK FIRST; throwaway) 🚦🚦

**Why:** the feature stands or falls on releasing the 2.4 GHz PHY from WiFi **and
BLE** and getting it back. Neither drop-and-recreate is exercised in the current
firmware. Two independent decision gates:

### Gate A — WiFi + ESP-NOW release/re-init
- Spike: after boot + mesh up, on a trigger — `disconnect_async()`, **drop**
  `wifi_controller` + `wifi_interfaces` (which owns `esp_now` and `station`) +
  stop `net_task` — then `Ieee802154::new(peripherals.IEEE802154)`, capture a
  frame. On a second trigger — drop `Ieee802154`, `esp_radio::wifi::new(unsafe {
  WIFI::steal() })`, re-derive `esp_now`/`station`, reconnect, confirm mesh peers
  return.
- **PASS** → RS3 does live WiFi re-init. **FAIL/flaky** → WiFi path also uses
  reboot-on-exit (RS3 simplifies).

### Gate B — BLE teardown (GAP-A — expected hard) ⚠️
- **Known blocker:** `ble_host_task` is `_spawner.spawn(...)`-ed once at
  main.rs:446 and runs forever, owning the BLE controller; the firmware has **no
  way to stop BLE without a reboot** in the current arch.
- Spike question: can the BLE PHY be released at all without killing that task?
  (Almost certainly no without a refactor.)
- **DECISION GATE (explicit):**
  - **Default / expected:** BLE cannot be torn down live → **scan-exit does a
    software reboot** to restore all radios from the known-good boot path (RTC is
    battery-backed; mesh re-syncs in seconds). This is JP's adopted "reboot
    fallback," and for BLE it is the *primary* path, not just a fallback.
  - **Only if** a separate `ble_host_task` teardown refactor lands first (make the
    task cancellable / drop the controller) does live BLE release become possible;
    until then, **do not** attempt it.
- **Output of RS0:** a one-paragraph verdict recorded in the PR — (a) WiFi live-
  re-init works? (b) BLE requires reboot? — which selects RS3's exit strategy:
  **live-restore for WiFi/ESP-NOW if Gate A passed, reboot for the BLE path.**
- Log heap at every transition.

## RS1 — Cargo feature + build wiring
- `Cargo.toml`: add `ieee802154` to esp-radio features (we already have `unstable`).
- Confirm both bins build + clippy clean; record flash-size delta (RS7 baseline).
- **Accept:** `cargo build --release` green.

## RS2 — Scan data model + MAC parse (host-testable, pure)
- `src/net/scan_model.rs`: `PanEntry { pan_id, channel, last_rssi, rssi_ewma,
  frames, beacons, devices: FnvIndexSet<u16,8> }`, `ChannelStat { frames,
  peak_rssi }`, `ScanState { pans: Vec<_,16>, channels: [_;16], total }`; fold-
  frame + LRU-evict-weakest on overflow. **No classifier** (v2).
- MAC-header parse (PAN id, src addr, frame type) from `ReceivedFrame`; RSSI/LQI
  passthrough.
- `#[cfg(test)]` host tests over canned frame bytes (beacon/data/malformed),
  capping, dedup, evict. Pure → `cargo test` on host, no HW.
- **Accept:** tests pass.

## RS3 — Radio lifecycle: Option-slot refactor + switch (GAP-B) ⚠️
**This is the biggest code change; it is a refactor of `main.rs`'s radio
ownership, gated by RS0's verdict.**
- **GAP-B refactor:** move `wifi_controller`, `wifi_interfaces` (or its unpacked
  `esp_now` + `station`) into **`Option<_>` slots** so they can be dropped and
  replaced. Route **ALL** `esp_now` object use sites through the `Option`
  (early-`continue` when `None` during scan). Complete enumeration verified on
  78356f2 (oracle-corrected — the RX-drain trio was previously missed):
  - `476` — `let mut esp_now = wifi_interfaces.esp_now;` (the binding → Option slot)
  - `916` — `esp_now.set_channel(...)`
  - `941` — `esp_now.add_peer(peer)` (NB: 934 is the `PeerInfo` struct, not the call)
  - `955` — `mesh.tick(&mut esp_now, …)`
  - `979` — `mesh.broadcast_diag(&mut esp_now, …)`
  - `992` — `esp_now.send(…)`
  - `1016` — `mesh.relay_emit(&mut esp_now, …)`
  - `1018` — `mesh.relay_retransmit(&mut esp_now, …)`
  - **`1019` — `esp_now.receive()` (the RX drain loop — easy to miss, critical)**
  - **`1031` — `mesh.handle_rx(&mut esp_now, …)`**
  - **`1112` — `mesh.broadcast_fam(&mut esp_now, …)`**
  (`esp_now_peer_added` at 582/630/933/946/952 is a separate bool flag, not the
  object — reset it on teardown too.) Also `station` → `net_task`.
- `src/net/radio_mode.rs`:
  - `enter_scan()`: pause mesh logic → (Gate A path) drop the WiFi bundle +
    stop net_task → `Ieee802154::new(IEEE802154)` (first time) / `IEEE802154::
    steal()` (re-entry) → `set_config(promiscuous, rx_queue_size:16, ch:11)` →
    `start_receive()`.
  - `exit_scan()`: drop `Ieee802154` → **if BLE was up OR Gate A failed:
    `software_reset()`** (RS0 Gate B default); **else** live path: `wifi::new(
    WIFI::steal())`, re-derive `esp_now`/`station` into the Option slots, restart
    net_task, resume mesh.
- Failure ladder (live path): 3 re-init retries → `scan_phase=Error` → Reboot
  button (reuse the existing `software_reset` path at main.rs ~1090).
- **Accept (HW):** 10× enter/exit cycles per the RS0-selected strategy; heap
  stable across cycles.

## RS4 — Scan engine (channel hop) on the existing executor
- Sweep ch 11–26, **300 ms dwell** (const), poll `received()` into RS2 model;
  push to UI ≤2 Hz. **90 s auto-exit** timer (no interaction → `scan_exit`).
- Optional "park on channel" via a request cell (nice-to-have).
- Reuses the running `esp_rtos`/embassy executor — no new scheduler.
- **Accept (HW):** channel strip animates; PANs accumulate; auto-exit fires.

## RS5 — Slint scan UI (overlay, matches current shell API)
- `ui/slint/scan.slint`: phases **Warn / Scanning / Restoring / Error** driven by
  a `scan-phase` int property; channel strip (16 cells 11–26); PAN Flickable
  list (mesh-row pattern — `PANID · ch · RSSI · N dev`); footer
  (`Mesh paused · {frames} · {sweep}s`); empty state; caption
  `passive · headers only · payloads encrypted`.
- `ui/slint/shell.slint`: add `in property <int> scan-phase`, `in-out property
  <bool> scan-open`, and `if root.scan-open: RadioScanOverlay {…}` — **the same
  overlay pattern as launcher/AOD** (`if root.launcher-open` @ shell.slint:259 /
  `if root.aod` @ 265). Callbacks `scan-confirm() / scan-exit() / scan-reboot()`.
- `ui/slint/theme.slint`: `ScanRow` struct (mirrors `PeerRow`).
- Iterate visuals in `slint-viewer` with dummy data.
- **Accept:** renders in slint-viewer across all four phases.

## RS6 — Launcher + ShellUi + main dispatch wiring
- `src/apps/mod.rs`: add `AppState::RadioScan`.
- `src/ui/slint_shell.rs`:
  - add `RadioScan` to `LAUNCHER_APPS` **and** the `for` list in
    `launcher.slint` (lock-step order — existing comment contract).
  - `ShellRequests`: `scan_confirm/scan_exit/scan_reboot: Cell<bool>`.
  - setters: `set_scan_open`, `set_scan_phase`, `set_scan_channels(&[…;16])`,
    `set_scan_pans(&[PanEntry])` — **gated on `scan_open`, VecModel swap-in-place
    exactly like `set_mesh_rows`** (slint_shell.rs:404). Wire the new callbacks
    like `wifi-tap`/`launch-app`.
- `src/main.rs` launch dispatch — **CRITICAL (oracle RS6):** branch
  `AppState::RadioScan` **BEFORE** the launch-drain `Framebuffer::try_new(...)`
  (the scan overlay is **Slint → needs NO framebuffer**). RadioScan sets
  `scan_open=true`, `scan_phase=Warn`; the loop stays in **shell render mode**.
  - **Re-entry:** second and later scans take the radio via `IEEE802154::steal()`
    (the peripheral singleton was consumed on the first `Ieee802154::new`).
  - **Interaction with morpheus's fb-fix (task #27, `Option<WatchShell>` scene-
    drop):** games drop the Slint scene to free RAM for the fb; **RadioScan must
    NOT drop the scene** — it renders through it. Ensure the RadioScan branch is
    on the shell side of that split, never the scene-drop side.
    ⚠️ **RE-VALIDATE when #27 merges:** the `Option<WatchShell>` scene-drop
    mechanism is **not in the integration HEAD yet** — it lives on
    `feat/migration-tail` (unmerged). This "render-through-scene, don't drop it"
    instruction (and the identical one in mic-capture MC5) must be re-checked
    against the actual scene-drop API once #27 lands, since the exact
    hook/method it exposes will determine how RadioScan opts out of the drop.
- **Accept (HW):** launcher → Radio Scan → **Warn**; Back cancels with radios
  untouched; Scan → live results; Exit → mesh restored (per RS0 strategy).

## RS7 — Verification + measurement + ship
- Full HW pass (spec §Testing): confirm the Warn gate; **cross-check captured
  PAN-ID/channel against JP's real Zigbee coordinator** (truth source); RSSI
  sanity; exit restores mesh (peers/WiFi/NTP) — or clean reboot per RS0-B.
- Heap before/during/after; flash-size delta (`cargo size`/espflash); confirm the
  app/ota partition still fits (`partitions.csv`).
- Exercise Error→Reboot.
- README feature line; `/ship`.
- **Accept:** all gates green; measurements in the PR.

---

## Dependency graph / sequencing
- **RS0 first and blocking.** Its two gates decide RS3's exit strategy.
- RS1 + RS2 can run in parallel with RS0 (no HW dep).
- RS3 depends on RS0's verdict (live vs reboot) + RS1/RS2.
- RS4 depends on RS1+RS2+RS3.
- RS5 is UI-only (parallel to RS3/RS4).
- RS6 joins RS3/RS4/RS5 + depends on task #27 (fb-fix) having defined the
  launch/scene-drop split.
- RS7 gates the ship.

## GAP summary (oracle, verified on 78356f2)
- **GAP-A (BLE):** `ble_host_task` spawned forever (main.rs:446) → no live BLE
  teardown → **reboot-on-exit is the BLE path** unless a task-teardown refactor
  lands first. RS0 Gate B decides.
- **GAP-B (ESP-NOW):** `esp_now`/`station` come from the `wifi_interfaces` bundle
  (main.rs:476, 483) and are used inline at **11 `esp_now` object sites** (full
  list in RS3 — incl. the oracle-caught RX-drain trio `receive()` @1019,
  `handle_rx` @1031, `broadcast_fam` @1112; 16 total `esp_now` textual
  occurrences counting the `esp_now_peer_added` flag + type paths) → RS3 must
  Option-slot them and re-derive from the fresh bundle on restore.

## Deferred to v2 (out of this plan)
- Zigbee vs Thread classifier (JP: v2). v1 shows "802.15.4 PAN" only.
- Active beacon-request TX (passive-only v1).
- Channel-park polish; capture logging; live mic-while-scan.
```
