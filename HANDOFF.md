# HANDOFF — esp32c6-watch (updated 2026-08-25 end-of-day, the convergence day)

## The goal and where it stands
JP's standing goal: **the watch as a full smol target, all features + parity, on the C6 AND C5 AND S3.** Closure semantics (JP's ruling): parity is per-board hardware-relative — ruled drops, documented degradations.

- **Structure: DONE.** smol#402 merged (subtree targets/c6-watch, full history); refreshes #417 (multi-board seam) + #423 (S3 paints) merged, #427 (S3 touch) pending merge. Repo convention (JP's ruling): BOTH repos permanent — standalone = working repo, smol subtree = delivery, refreshed from watch main via PRs.
- **C6**: the reference. Today's adds: #75 wedge mechanism FOUND+FIXED (unbounded dns_query in the UI loop, cbc853b), automator fixed (3-frame taps, truthful state, ip= field), BLE reclaim (01c91e4, ~24-29.5KB, lazy init — on-device bracket pending tether), announce re-check (#90 fixed), refusal-path hardening (855bac7), multihop citizenship (#64 COMPLETE in code: mesh-flood crate + relay duty + HopLatch escalation, 9bcbd3c).
- **C5** (arcane-beacon, 176): watch OS live on glass, JP mid-acceptance; provision.py's first live run passed. morpheus's feat/cyd-c5-gating (st7789/xpt2046 drivers, 451-line board module) merges after his image 9 — until then the C5 arm on main is LINK-ONLY (its glass runs his branch's image).
- **S3** (eldritch-insignia, 162): builds COMPLETE from main — full landscape scene (isel workaround fdfe822), ILI9341V driver + ActivePanel seam (2827ce9), FT6336U touch (e69a8ea). Bench flash armed at the s3-cyd session (their serial-pinned guard; wconfig partition carved at 0xC20000; provision --config-offset). Waits on #427's merge.

## Next session's headline candidates
1. ~~mesh-OTA (#86)~~ **LANDED same day** (293c117) — remaining: live verify vs a serving gateway + the persisted anti-rollback floor (config byte). #64 also complete in code. WorldSnake was ALREADY mesh_snake (audit corrected).
2. **On-device verification batch** (needs mythic tethered): BLE reclaim heap bracket + 5x50 soak; #90 announce live test; eldritch-lantern factory-table migration (flash-full + provision — see the factory-partition memory).
3. **Story E2E**: blocked ONLY on JP's AP fix (#89 — the 'admin' SSID L2-isolates on one AP; he took it).
4. ~~#36 epic remainder~~ **The pure-services vendoring is COMPLETE** (same night): flood/wire/etx/cfgsched in mesh-flood, ledger L1-L4 in mesh-ledger, the OTA leaf in ota-proto — all host-tested, per-arch-safe. mesh_snake already existed (WorldSnake). App-tier remainder only: cast (needs scene capture — design work) + bard (own epic). mesh-OTA is ON ALL THREE boards (per-arch ed25519, 2e847ac).

## Live constraints (unchanged unless noted)
- .cargo/config.toml gitignored (credentials) — never commit; fambuild supplies it to worktrees; preflight fails loudly without it.
- All firmware builds via fambuild (familiar); S3 arm via tools/build-s3.sh (espup +esp, opt=2 — s/z crash the Xtensa LLVM; the isel family is esp-rs/rust#282, our evidence posted).
- /tmp on katana is BANNED for working files (JP directive): project tmp/ (gitignored) or /var/tmp.
- rust/clock + docs/protocol.md in smol route through the smol-d8 session; land smol-tree changes as PRs, never self-merge.
- morpheus's branches: never commit to them; the refused: error contract is shared (prefix, not suffix).
- CYD/S3 physical flashing stays behind their sessions' serial-pinned guards.
- Mesh protocol is UNFORKED by vendoring smol's pure modules verbatim (mesh-flood, ota-proto) — re-vendor on wire changes, never edit locally.


## S3 IS A FUNCTIONAL FLEET LEAF (2026-08-26)

#447 soak CLEAN: eldritch-insignia joins the mesh as id 162 (0 reboots, 20/20 relays acked, associated). Fix ladder: PSRAM init → internal 96KB → **PSRAM-first (radio reserve)**. smol#448 has the post-soak polish (has-pmu gate + MQTT retry). Awaiting: JP's glass verdict, #446/#447/#448 merges, then blessed-sha build. C5 = morpheus's branch (image 9); C6 shipping.

## HOLD-OPEN STATE (2026-08-25 end — staying live for bench relays per JP)

**Everything code-reachable from this seat is done, pushed, or specced.** All 3 fleet boards physically absent; gatekeeper refuses ssh (JP on the AP, #89).

Landed today (watch main tip 242182c): full board seam (C6/C5/S3) · Luna's ui/cyd scene · S3 ILI9341V+touch drivers · S3 PSRAM fix (the reboot-loop root cause, 8a6ad9e) · §1d board-facts · provision.py · #75 wedge fix · BLE reclaim · #90 announce · refusal-path hardening · the ENTIRE #36 services layer host-tested (mesh-flood[flood/wire/etx/cfgsched], mesh-ledger[L1-L4], ota-proto[+leaf], cast-core, bard-core[40 golden]) · multihop #64 + mesh-OTA #86 wired live on all 3 boards · cast wired (feature) · bard on-device (feature). 409 host tests, 3 arms link.

smol PRs: #417/#423/#427/#444/#445 ALL MERGED (445 at 02:54Z 2026-08-26 — the PSRAM fix is in targets/c6-watch on smol main; s3-cyd unblocked for second first-light via the canonical path).

**Waiting on (external, will arrive as relays):**
- S3 SECOND first-light: s3-cyd rebuilds from #445. Watch for `[PSRAM] octal, 8192 KB` boot line.
- C5 on glass: morpheus image 9 under test; his feat/cyd-c5-gating driver merge is his lane.
- cast pixels (WLED matrix), cross-arch mesh-OTA + multihop (multi-node windows): bench verifications.
- story E2E: JP's AP fix (#89).
- bard screen: spec at docs/specs/2026-08-25-bard-screen-spec.md → Luna + glass.

**On a relay:** if a bench reports a bug, root-cause + fix + push + refresh-PR (the PSRAM pattern). If a verification passes, mark the issue. No polling — relays arrive via SendMessage.
## C5 MERGE-READINESS (2026-08-26, prepped while holding)

Dry-run `git merge-tree main github/feat/cyd-c5-gating` (his tip **42a4900**, 24 commits ahead) = **12 conflicting files**: Cargo.toml · board/{mod,cyd_c5,waveshare_c6}.rs · drivers/{mod,framebuffer,spi_bus}.rs · main.rs · net/{mqtt_ha,ota_http}.rs · ui/{slint_platform,slint_shell}.rs. This is a real reconciliation, NOT a fast-forward — the S3 work moved main out from under his branch.

**DOUBLE-FIXES (dedup at merge, do NOT stack both):**
- has-pmu 4Hz power-key poll: his `884c7e8` ↔ my `f0452a1` (S3).
- BLE ~29.5KB boot cost radio-off: his `88601c4` ↔ my `ble_bring_up()` already on main ([[ble-init-before-config-load]]). Pick one impl.

**Fold-in at merge:** his `5e77086` (pin C5 2.4GHz) = the has-5ghz-wifi finding; main's board-cyd-c5 lacks it. His `40f5da7` (print compiled-in broker) overlaps my per-seat MQTT VLAN-trap docs.

Gated on morpheus/cyd-c5-e2 blessing image 9/10 on glass — merge is MINE to own on watch main when he pings (never self-merge smol). Heads-up sent to cyd-c5-e2. Re-measure C5 heap floor vs mesh-ota after.

### C5 dedup addendum (from cyd-c5-e2, 2026-08-26)
- has-pmu has a THIRD site in their lineage; confirmed my f0452a1 covers it (vol_during_play poll_power_key main.rs:5560, has-pmu gated; S3 target sets neither has-pmu nor has-audio).
- **LATENT (found while checking):** `power.enable_mic_rail()` main.rs:1524 is a PMU call gated under **has-audio, not has-pmu** — dormant on S3 (has-audio off) but resurfaces on any PMU-less board that gains audio. `let _ =` silent (one NACK/boot, not spam). AT MERGE: move to has-pmu (or has-pmu && has-audio). Add to dedup inventory.
- Floor re-measure is POST-MERGE only (C5 target on his branch + mesh-ota on main = combined floor doesn't exist until reconciliation). Build merged C5 arm on familiar, send number before their 0.3 scores against pre-mesh-ota +14,632B.

## C6 CURRENT-WINDOW LIVENESS PROBE (2026-08-26 ~01:30, via s3-cyd-45)
No C6 board on my USB; broker denies anon read; #89 scopes only the watch AP, homelab LAN is up (realm CLI works). s3-cyd-45's S3 (id162) is a live mesh peer — used as the observer.
- Its live mesh window saw acks from C3 crown (ac:a7:04:ba:1f:24, id50) + one unknown 10:00:3b:ce:95:cc. Attributed via sigil-id crate (seed=mac[2..6] XOR-fold): unknown → id **172**, NOT a C6 (fleet C6 = eldritch-lantern 98:A3:16:A7:2F:E4→122, mythic-throne 98:A3:16:A5:A7:F8→236). ESP-NOW = STA/base MAC, so a C6 peer would show 98:A3:16:*. No C6 mesh ack in-window.
- Retained MQTT diag EXISTS: smol/122 up=42.8h, smol/236 up=2.7h (mythic-throne booted tonight) — but retained ≠ now.
- Cadence: firmware broadcasts mesh diag every 60s (main.rs:3683), gateway relays to smol/<id>/diag. **s3-cyd-45 running a 10-min flip-watch** on smol/122|236/diag. ANY new-value flip ⇒ C6 verified live-now; zero flips ⇒ record as night duty-cycle (NOT dead), don't claim verification. Awaiting its flip log.

### C6 observability from THIS seat — all paths closed (2026-08-26 01:35, don't re-drill)
- USB: fleet absent (watchctl).
- MQTT: broker (10.0.11.110/10.0.8.111:1883) TCP-open but REQUIRES AUTH — `mosquitto_sub -d` anon shows "sending CONNECT" then no CONNACK (silent drop). Raw authed client also no-CONNACK (packet/cred mismatch). Can't compare w/ authed mosquitto_sub w/o argv-exposing pw (CLAUDE.md forbids). Only s3-cyd-45 connects authed → sole observer.
- Mesh ESP-NOW: via s3-cyd-45 peer table — only C3 crown (id50) + unknown id172 acked in-window; no 98:A3:16:* (C6) ack.
- WiFi/realm: `realm find` C6 MACs = no match (registry only, not live assoc); no DNS for eldritch-lantern/mythic-throne; no lease/dhcp/arp command in realm; watches absent from `realm watch` WOL stream; gatekeeper ssh blocked (#89).
- NET: C6 live-now provable ONLY via s3-cyd-45's authed MQTT flip-watch (running) or a board on USB. Retained diag (122 up=42.8h, 236 up=2.7h) shows both alive tonight but retained≠now.

## FLIP-WATCH RESULT (s3-cyd-45, window 01:29:22 PDT, 569s live span)
Gateway→broker relay proven healthy end-to-end (4 nodes flipping ~24s cadence over full window).
- **C5 (arcane-beacon, id176 — smol-allocated override) = LIVE.** smol/176/diag flipped ×24 → C5 hardware alive, meshing, gateway→MQTT publishing NOW (on morpheus's bench firmware). **Current-window C5 hardware liveness ESTABLISHED (network/mesh/MQTT stack).** Remaining C5 gates are narrower: morpheus's on-glass GUI bless + the merge-to-main I own.
- **S3 (id162) = LIVE** — smol/162/diag ×24, broker-side corroboration of the already-verified #447 fleet node.
- **C6 both ASLEEP, not dead.** smol/122 (eldritch-lantern) & smol/236 (mythic-throne): ZERO live flips, retained byte-identical to 70min prior (122 up=154054 heap=64380; 236 up=9699 heap=51420). Channel was live (others flipping) → strong negative. mythic-throne up=2.7h = alive tonight, duty-cycled at 01:30. C6 current-live NOT captured; recent-alive confirmed.
- Also flipping: smol/8, smol/51 (C3 crown family).

BOARD STATUS NOW: S3 verified (deploy + broker-live). C5 hardware network-verified live; on-glass+merge pending. C6 alive-tonight, asleep at probe time (no board on USB to wake+verify from this seat).

## S3 GUI GARBLE — ROOT-CAUSED + FIXED (2026-08-26, JP on-glass)
JP at bench: S3 GUI garbled = **chunks displaced/torn** (colors + signal intact, NOT noise). Root cause: `arm_ramwr()` defined in spi_bus.rs but NEVER called. SharedSpiBus latches RAMWR_CONT (0x3C) after first pixel push; a new CASET/RASET window needs the next push to restart with RAMWR (0x2C). ST7789 sibling's set_addr_window arms it; **ILI9341 set_addr_window omitted it** → every strip after the first resumed at the prior GRAM pointer = displaced. Below the flusher (pairing-counter blind); emberburrito clean because mipidsi always RAMWRs.
**FIX 928d35d** (pushed): one line `self.bus.arm_ramwr();` after RASET in ili9341.rs set_addr_window. s3-cyd-45 building/flashing with JP at glass. Awaiting on-glass verdict. Prediction: renders clean; C5 (ST7789, already arms) should be unaffected on this axis — if C5 also displaced, separate bug.

## C5 ON-GLASS GUI BLESSED (2026-08-26, JP at bench)
JP on glass: **C5 GUI "looks great."** Combined with id176 flipping ×24 live (network) + this GUI bless, C5 hardware is GUI+network+ verified. Remaining C5: (a) "trouble swiping" = XPT2046 touch calibration, separate cyd-c5 lane; (b) merge-to-main (MINE) — still gated on cyd-c5-e2's acceptance-closure ping (swipe cal may still be open their side; tip still 42a4900). S3 wake-on-tap also confirmed on glass.
BOARD STATUS: S3 = deploy+network verified, GUI fix 928d35d awaiting glass verdict. C5 = network + GUI blessed, swipe-cal + merge pending. C6 = alive-tonight, asleep at probe.

## S3 GUI GATE CLOSED (2026-08-26, JP verdict "gui looks good" on 928d35d)
RAMWR_CONT diagnosis CONFIRMED on glass — arm_ramwr one-liner fixed the displaced chunks. S3 GUI renders clean. NEW S3 items surfaced (pre-existing, exposed now GUI renders; 928 diff = ili9341.rs+docs only, so NOT caused by the fix):
- **Touch dead in UI** (no taps, no swipes). Wake-on-tap worked on 448d (FT6336U/I2C/INT path alive). Likely never-verified UI-dispatch path. Diagnostic: println per touch report (raw+transformed xy).
- **Time not synced**: gatekeeper conntrack shows NO board→broker conn this boot; boot burst NTP+MQTT didn't complete (environmental/2AM; prior builds green). Q: periodic re-burst/NTP retry or boot-only?

### S3 touch + NTP (2026-08-26, s3-cyd-45 driving touch per JP "go full auto")
- **Touch root cause (s3-cyd-45's lane, branch fix/s3-ft6336-active-mode):** FT6336U Monitor mode (0xA5=0x01, written by Ft3168Touch::init) is DEAF and the chip self-re-arms into it — correct on C6's FT3168, lethal on S3. Cross-verified vs emberburrito on same panel. Fix: 0xA5=0x00 active + 0x86=0x00 stay-active + 0xA4=0x00 level-INT, S3-gated, C6 byte-identical. **touch.rs + board const are s3-cyd-45's; do NOT edit.** Register map + transform + main.rs dispatch gating all verified clean by me — only the power-mode was wrong.
- **main.rs touch gating (MINE):** `touch_active = screen_state>=2 && (int_low||was_touching)` assumes level-INT. s3-cyd-45's 0xA4=0x00 makes it valid. FALLBACK if INT still pulses: drop int_low gate, poll unconditionally while screen_state>=2 (cfg-gated, C6 unchanged). Not needed unless level-INT fails on glass.
- **NTP cadence (net_task.rs):** retries every 10s while !ntp_synced+associated (self-heals on assoc), STOPS after first sync (NO periodic re-sync — drift gap for multi-day uptime, real follow-up). MQTT announce every 300s ONLY after sync. So no-NTP ⇒ no-MQTT-burst (explains conntrack silence). S3 time-unsynced 26min w/ IP 10.0.8.214 ⇒ NTP server unreachable on VLAN8 leg this boot (per-seat firewall shape, like the MQTT broker), not firmware. "hourly NTP burst" comment in mqtt_climate is inaccurate.

## S3 FULLY GLASS-VERIFIED (2026-08-26) — touch merged to watch main
JP verdicts on glass: GUI "gui looks good" (arm_ramwr 928d35d) + touch "the s3 touch works really well" (FT6336U active-mode e2efaad). S3 now: GUI + touch + mesh(id162) + WiFi + NTP + MQTT + wake-on-tap all verified. Touch merged to watch main **4922dad** (merge commit). Watch main is the complete verified S3 source of truth.
- JP's earlier "not working" was C5 SWIPES (XPT2046 resistive, cyd-c5-e2/morpheus lane) — NOT S3. C5 GUI+taps fine.
MERGE TRAIN → smol main (smol-d8's subtree-refresh lane, never-self-merge):
- #448 CUMULATIVE (subsumes #446/#447 — its main.rs diff carries PSRAM-first+heap-96). S3 scan/PSRAM/pmugate/MQTT-retry.
- #449 arm_ramwr (mine, ili9341.rs-only, orthogonal, glass-verified).
- Touch fix (e2efaad, in watch main 4922dad) NOT yet in a smol PR; its esp32s3_cyd.rs edits OVERLAP #448 → smol-d8 should refresh subtree to watch main @4922dad cumulatively rather than stack conflicting PRs.
- Recommend: smol-d8 drives ONE cumulative subtree refresh to 4922dad (supersedes #446-449) OR merges #448+#449 then a touch refresh. smol-d8's call.
Follow-ups (mine, queued, no urgency): NTP re-sync deadlock (periodic re-sync decoupled from announce gate); C5 12-file merge to watch main (pending cyd-c5-e2 closure ping) w/ dedup inventory.
