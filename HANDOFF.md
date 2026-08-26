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