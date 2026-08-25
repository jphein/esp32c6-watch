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
4. #36 epic remainder: etx → ledger/crdt → mesh_snake → cfgsched/cast → bard (audit + order on the issue).

## Live constraints (unchanged unless noted)
- .cargo/config.toml gitignored (credentials) — never commit; fambuild supplies it to worktrees; preflight fails loudly without it.
- All firmware builds via fambuild (familiar); S3 arm via tools/build-s3.sh (espup +esp, opt=2 — s/z crash the Xtensa LLVM; the isel family is esp-rs/rust#282, our evidence posted).
- /tmp on katana is BANNED for working files (JP directive): project tmp/ (gitignored) or /var/tmp.
- rust/clock + docs/protocol.md in smol route through the smol-d8 session; land smol-tree changes as PRs, never self-merge.
- morpheus's branches: never commit to them; the refused: error contract is shared (prefix, not suffix).
- CYD/S3 physical flashing stays behind their sessions' serial-pinned guards.
- Mesh protocol is UNFORKED by vendoring smol's pure modules verbatim (mesh-flood, ota-proto) — re-vendor on wire changes, never edit locally.
