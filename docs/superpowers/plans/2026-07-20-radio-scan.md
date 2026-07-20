# Radio Scan — Implementation Plan

Date: 2026-07-20
Status: draft for JP review
Spec: `docs/superpowers/specs/2026-07-20-radio-scan-design.md`
Prereq: Slint migration merged to `main` (plan tasks T9, ideally T10 + T13).

Each task is a compiling, revertible commit on a `feat/radio-scan` branch cut
from `main` **after** the Slint migration lands. HW gates called out per task.
Order matters: **RS0 (the spike) must pass before RS3–RS7 are worth building.**

---

## RS0 — Radio-switch spike (DE-RISK FIRST, throwaway) 🚦

**Why first:** the whole feature hinges on being able to drop the WiFi/BLE
controllers mid-run and later re-create them. This is documented but never
exercised in the firmware. Prove it on hardware before building UI.

- Minimal branch/scratch bin: after boot + WiFi up, on a button: disconnect +
  drop `wifi_controller`/interfaces, drop BLE, `Ieee802154::new(peripherals
  .IEEE802154)`, `set_config(promiscuous)`, `start_receive()`, print any frames.
  On a second button: drop `Ieee802154`, `esp_radio::wifi::new(WIFI::steal())`,
  reconnect, confirm WiFi + ESP-NOW mesh come back.
- **Acceptance:** captures ≥1 real 802.15.4 frame; **and** WiFi + mesh cleanly
  restore afterwards (peers reappear). Log heap at each transition.
- **If restore is flaky:** record it; switch the spec to "always soft-reboot on
  exit" and RS3 simplifies to teardown-only + reboot. Report to JP either way.

## RS1 — Cargo feature + build wiring

- `Cargo.toml`: add `ieee802154` to esp-radio features (keep `unstable`).
- Confirm both bins still build + clippy clean; note flash-size delta now
  (baseline for RS7).
- **Acceptance:** `cargo build --release` green; size delta recorded.

## RS2 — Data model + MAC parse + classifier (host-testable, pure)

- `src/net/scan_model.rs`: `PanEntry`, `ChannelStat`, `ScanState` (heapless,
  capped 16 PANs / 16 channels / 8 devices-per-PAN), fold-frame method,
  LRU-evict-weakest on overflow.
- MAC-header extraction (PAN id, src addr, frame type) from a `ReceivedFrame`/
  raw bytes; RSSI/LQI passthrough.
- `PanKind` classifier (Zigbee-beacon signature / Thread-data heuristic /
  Unknown) — best-effort, documented as such.
- `#[cfg(test)]` host tests over canned frame byte-arrays (beacon, data,
  malformed). Parser is pure → runs under `cargo test` without HW.
- **Acceptance:** tests pass; folding/dedup/evict verified on host.

## RS3 — Radio lifecycle module

- `src/net/radio_mode.rs`: `enter_scan()` (pause mesh → drop WiFi → drop BLE →
  bring up 15.4 promiscuous) and `exit_scan()` (drop 15.4 → re-init WiFi via
  `WIFI::steal()` → re-arm BLE → resume mesh), returning a `Result`.
- Reserve `peripherals.IEEE802154` into a slot at boot.
- Failure ladder: N=3 restore retries → surface `Error`; expose a
  `reboot()` helper reusing the existing reset path.
- **Acceptance (HW):** enter/exit cycles 10× without hang; heap stable across
  cycles (no leak from repeated controller drop/recreate).

## RS4 — Scan engine (channel hop)

- Embassy task: sweep ch 11–26, 300 ms dwell (const), poll `received()` into
  the RS2 model; push to UI ≤2 Hz. Optional "park on channel" via a request cell.
- Auto-exit timeout (90 s no interaction) → `scan_exit`.
- **Acceptance (HW):** channel strip animates; PANs accumulate; sweep timing
  matches; auto-exit fires.

## RS5 — Slint scan UI

- `ui/slint/scan.slint`: Warn / Scanning / Restoring / Error phases driven by
  `scan-phase`; channel strip; PAN Flickable list (mesh-row pattern); footer;
  empty state; the "passive · headers only · payloads encrypted" caption.
- `ui/slint/theme.slint`: `ScanRow` struct.
- `ui/slint/shell.slint`: `scan-open` + `scan-phase` properties, `if scan-open:`
  overlay, `scan-confirm/scan-exit/scan-reboot` callbacks.
- Iterate visuals via `slint-viewer` with dummy data (per migration workflow).
- **Acceptance:** renders in slint-viewer across all four phases.

## RS6 — Launcher + ShellUi wiring + main.rs dispatch

- `src/apps/mod.rs`: `AppState::RadioScan`.
- `src/ui/slint_shell.rs`: add to `LAUNCHER_APPS`; `ShellRequests` scan cells;
  `set_scan_open/phase/channels/pans/footer`; wire the new callbacks.
- `ui/slint/launcher.slint`: add "Radio Scan" item (lock-step order w/ LAUNCHER_APPS).
- `src/main.rs`: special-case `AppState::RadioScan` in launch dispatch → drive
  scan mode via RS3/RS4 instead of eg-app/framebuffer path; drain scan requests.
- **Acceptance (HW):** launcher → Radio Scan → Warn; Back cancels (radios
  untouched); Scan → live results; Exit → mesh restored.

## RS7 — Verification + measurement + ship

- Full HW pass from the spec's Testing section, incl. the **PAN-ID/channel
  cross-check against JP's real Zigbee/Thread coordinator** (the truth source).
- Record heap before/during/after; record final flash-size delta; confirm the
  app/ota partition still fits (`partitions.csv`).
- Error→Reboot path exercised.
- Update `README.md` feature list; `/ship`.
- **Acceptance:** all gates green; measurements recorded in the PR.

---

## Sequencing notes

- RS0 gates everything. RS1–RS2 can proceed in parallel with RS0 (no HW dep).
- RS3 depends on RS0's verdict (live-restore vs reboot-only).
- RS4 depends on RS1+RS2+RS3. RS5 is UI-only (parallel to RS3/RS4). RS6 joins
  them. RS7 is the gate.
- If RS0 fails (restore flaky): drop `Restoring` complexity, RS3 = teardown +
  reboot-on-exit, RS5 Error phase becomes the only exit path. Smaller, safer v1.

## Not in this plan (deferred / v2 candidates)

- Active beacon-request TX (would surface silent PANs; not pure monitoring).
- Decrypting anything (needs keys; out of scope by physics).
- OpenThread join / participation (this is monitor-only).
- Logging captures to flash / exporting to the mesh.
