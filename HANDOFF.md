# HANDOFF — 2026-07-29 (late), esp32c6-watch

**Active goal (Stop hook):** get Endless LitRPG story stably running on a watch.
**Status: story loads, plays, and volume works mid-chapter. One MEASURED crash is
armed and waiting on a server-side change — read §1 before touching the daemon.**

---

## 1. ⛔ DO NOT SHIP EQUIPMENT DATA FROM THE DAEMON YET

**Measured, 2/2 trials on real hardware:** with all 17 equipment/appearance slots
populated, **opening Story's CHARACTER page reboots the watch.**

    story-p0   up=63s/65s   items 256  tex 256   reclMIN 23,748   app=Story
    story-p3   up=17s/18s   items 128  tex  64   reclMIN 65,536   app=Watchface

`up=` resetting with pools back at boot rungs and the app back to Watchface is an
unplanned reboot on the tap. It fails before a beat can even report a floor, so the
earlier projection ("floor drops to ~7.4 KB") is moot — this is not a low-headroom
state, it is a crash.

**Why nothing on the watch has to change for this to start happening.**
`story.slint:517` is `text: tr.known ? tr.value : "—"`. Every slot is null today, so
`known` is false and each row renders ONE em-dash — the 512 pool rung is
*structurally unreachable*. The moment the daemon sends non-null slots, `known` flips
true, all 17 rows render real 22-28 character names, and the scene jumps to ~436
items / 429 glyphs. It is a **binary flip, not a gradient**: no long-name warning
shot, no gradual degradation.

**The mechanism is a large-CONTIGUOUS failure, not exhaustion.** The 512 rung asks
for `tex` 512 x 28 = **14,336 B contiguous**. Reclaimed had 23,748 B *free* — but free
is a sum of holes, and `maxblk_main=0` means main cannot help. Any guard built on a
free-byte threshold reports this page as comfortable right up to the reboot.

**The fix is the rung, not the cap.** `MAX_SLOT_VAL` clipping only prevents the
crossing at **6 characters**, and 6 characters of a 22-28 char name is deletion, not
truncation. Page 3 has to render fewer rows at once — bounded repeaters or a windowed
list. Host-checkable without hardware: `bash tools/lunameter/measure.sh`, pass
condition is the `story(page3,len24)` frame staying on cap 256.

| frame | items | tex | cap |
|---|---|---|---|
| page3, all slots null (today) | 166 | 159 | 256 |
| page3, 6-char values | 252 | 245 | 256 (11 glyphs margin) |
| page3, 8-char values | 286 | 279 | **512 — crossed** |
| page3, 24-char values (real width) | 436 | 429 | **512** |

## 2. Bench state

| watch | firmware | notes |
|---|---|---|
| eldritch-lantern | lucid's `story,debug-console,story-stub-slots` arm | **re-enumerates across ttyACM\* live** — always resolve by sigil, never hardcode |
| mythic-throne | `Molten Forge · d7cdcee` (`--features story`) | crash reference; panicked live today, see §3 |

`main` is pushed and clean apart from lucid-story-heap's in-flight stub edits
(`Cargo.toml`, `src/ui/slint_shell.rs`). **`watchctl deploy` now pins the ELF** to
`scratch/flashed/<stem>-<sha12>` and flashes the snapshot — the folklore rule about
pinning per flash is now the default behaviour.

## 3. Mythic's "always rebooting" — captured, with a mechanism

```
panicked at i-slint-core-1.17.1/properties.rs:63:17: allocation failed
panicked at library/alloc/src/alloc.rs:573:9:          <- OOM INSIDE the panic printer
[PANIC] rebooting (custom-halt)
rst:0x3 (LP_SW_HPSYS)
[POOL-WARN] main pool can serve only 0 B (< 8192 B) with 1940 B free
```

A **bare** `allocation failed` at `properties.rs:63` is Slint's dep-node allocator —
a `Vec`/`RawVec` path would print `memory allocation of N bytes failed` with a count.
Both candidate types are 16 B, so size discriminates neither; the panic SOURCE does.

Carry forward: the panic printer itself allocates and can OOM again (this survived
only because `custom_halt` reboots), and `maxblk_main = 0` in **12/12** arms including
fresh-boot idle means **the reclaimed pool is the entire safety margin for every 16 B
allocation the UI makes.**

## 4. THE key fact about WiFi (was mis-diagnosed for hours) — still true

WiFi was never broken. The watch was on SSID `roam` → 10.0.11.x; the story daemon runs
on **katana 10.0.6.129:8093** and katana has **no 10.0.11.x interface**, so it was a VLAN
crossing. Voice always worked because its bridge is at 10.0.11.11, same subnet.

JP's SSID `admin` (password in the gitignored `.cargo/config.toml`) fixes it. Proven in
one window: `strongest ch6 rssi-29 (hint, no BSSID pin)` → `[WIFI] connected` →
`[NTP] unix=…` → `[WX] 67F`.

**Persisted config beats compile-time creds**: `option_env!("WIFI_SSID")` is used only
`if watch_cfg.ssid.is_empty()`. To switch SSID you must also erase the config partition:
`espflash erase-region --port <dev> 0xC10000 0x10000`.

**Open trade:** `[MQTT] failed: tcp read` — broker is 10.0.11.110, on the OLD subnet.
Infra is READ-ONLY (JP's rule).

## 5. Heap facts established today (~20 hardware arms)

- Story open costs **~39.6-40.0 KB of reclaimed**; peak consumption 45.5-46.9 KB of
  65,536 B.
- The CHAR page switch costs **~10.7 KB more**, double-sourced to 0.6 % (region deltas
  10,734 B vs hook counters 10,800 B) — and it is **not scene-pool**: `items`/`tex`
  stayed at 256/256 throughout. Hundreds of small blocks, ~675 at 16 B, consistent with
  one `Box<PropertyTracker>` per tracked PROPERTY (166 items x ~4), not per item.
- `maxblk_main = 0` means **"cannot serve >= 16 B"**, not "can serve nothing".
- Idle `main` free varies **4,312 B** (prod) / **9,916 B** (`story,debug-console`) boot
  to boot. **No single-reading claim under ~10 KB is evidence.** 4+ trials, reboot
  between.
- `main_low` is a **per-beat-window** floor, not a lifetime minimum. The briefed
  "296 B low-water" that framed this whole pass was one such window.
- `[POOL]` is a **bounded** instrument, not incomplete: `PartialRendererCache` is
  private in i-slint-core, so it can never be added. On page 3 `items`/`tex` understate
  heap cost by ~8x.
- `heap-hooks` costs **416 B of stack** (not the ~64 B its `.bss` suggests), and its
  histogram counts allocation **events**, not live blocks — read `net`.
- Watchface idle churns 603 allocations / 42,564 B in ~2 s at **net=0**:
  allocation-heavy, perfectly leak-free.
- A `story` build's stack margin is **+3,344 B**, not the ~+9,000 a default build gets;
  story costs 5,584 B of stack, 94 % of the #65 insurance `d47d2c6` bought.

## 6. Tooling traps fixed today (do not re-derive)

- **fambuild excludes `/.git`** → the sigil stamped `no-git`. Fixed:
  `tools/build_hash.sh` is the ONE implementation; fambuild exports
  `WATCH_BUILD_HASH`; `build.rs` prefers it.
- **fambuild left the ELF on familiar** → deploy flashed a stale binary (664 B `.bss` /
  784 B stack different) while the log said success. Fixed: fambuild fetches bin
  artifacts back; deploy compares the image's `WSIGIL:` hash against the tree.
- **`heap-hooks` couldn't link the whole package** (slint_demo lacked the hook-symbol
  module). Preflight never caught it because it passes `--bin esp32c6-watch` — the gate
  was scoped past the bug.
- **`watchctl slot` silence could not distinguish** a dead watch from a busy port. Now
  runs `fuser`/`lsof` first and names the holder.
- **N agents share one `target/`** — the ELF there is rewritten by whoever built last.
  An agent read it expecting its instrumented build and got another session's prod image.
- **lunameter is exempt from the fambuild rule** — a host build-and-run that must
  execute locally to emit frames.
- **Never-ship builds** (`story-stub-slots`) are refused by `watchctl deploy` via a
  `NEVER-SHIP:` marker grep and by preflight's combo assert. That build does not carry
  inert test code — it makes the watch permanently render the crash regime.

## 7. Open

- **Luna** holds the page-3 rung fix (bounded/windowed rows), briefed with §1's numbers.
- **lucid-story-heap** is re-running to capture the reboot's panic line verbatim; its
  first filter recorded the consequences and dropped the cause.
- **JP:** glance at mythic's launcher for a **Story tile**. Five seconds, and it settles
  the last unexplained number of the pass — the idle-row discrepancy (briefed 4,876 B
  vs measured 7,440-12,696), which is the only remaining support for a mixed-device
  baseline.
- Full working record: `scratch/story-heap/lucid-measurements.md` (gitignored).

## 8. The pattern that cost the most today

Eight instruments read **identically in both states they were meant to distinguish** —
a version string that never changed, a scene count blind to overpainting,
`maxblk_main=0`, `state`'s `page=`, `slot` silence, a gate swallowing its own errors, a
GPIO sampled coarser than the press, and bucket counts measuring churn. In most cases
the output was literally correct and still useless.

**Before trusting a probe, ask what it prints in the FAILURE case.** If that matches
the success case it cannot reassure you no matter what it says. Fix by adding the
discriminator, not by rewording — and prefer a check on the shipped artifact over a
comment or a list.
