# PROPOSAL — `board::ui` hit-geometry for the CYD-C5 landscape scene set

**Author:** Luna (layout workstream) · branch `feat/cyd-c5-layout`
**For:** the watch session, which owns `src/board/*.rs` and `src/ui/slint_shell.rs`.
**Status:** grows as each wave of `ui/cyd/` lands. Values marked ⏳ are not decided yet
because the page that owns them has not been laid out.

---

## Why this file exists instead of an edit

Every number below is one half of a **two-sided constant**: the `.slint` geometry places a
control, and a Rust constant hit-tests it. They are invisible to each other, and the
failure mode is not an error — `switcher_slot()` / `shade_slot()` invert a `start_y` back
to an index by arithmetic and return a **wrong index rather than `None`** at the wrong
geometry, so a dismiss-swipe dismisses the wrong notification with nothing to observe.

So the layout work states its half here, in one reviewable place, rather than editing
`src/board/*.rs` across a tree it does not own. **Applying these is the watch session's
call**, and each one should land in the SAME commit as the `ui/cyd/` page it pairs with.

Eight of these constants are currently **file-scoped in `src/ui/slint_shell.rs`**, not in
`board::ui` — which already exists (`src/board/cyd_c5.rs`, and `waveshare_c6.rs` has the
matching module with `STORY_PAUSE_RECT` populated). Moving them is a prerequisite for the
launcher / switcher / shade waves.

---

## 1. Decided — shell chrome and gesture shell (wave 1, landed)

| constant | C6 value | **CYD proposal** | derivation |
|---|---|---|---|
| `EDGE_BOTTOM_Y` | `427` | **`214`** | **Derived from the HOLD, not the panel proportion.** The 204 in this row's earlier version was `204/240 = 85 %`, matching the C6 — but the proportion was never the binding constraint. The bottom band *holds* (`HOLD_MS` arms on every press inside it and `hold_fired` swallows the lift that would have been the click), so it may not overlap a tap target; the top band only *swipes*, which is why `EDGE_TOP_Y == chrome-hit` is fine. At 204 the band cut 26 px into the 44 px clock chips and APPS became unpressable. Split the 26: band yields 10 (204→**214**, landing exactly on the shell's up-hint handle at 214..236 so hit area and affordance are one number), layout yields 16 (date/nook slide into the hero's and seconds' guaranteed-empty descender space). Enforced as `const _: () = assert!(EDGE_BOTTOM_Y >= LOWEST_TAP_BOTTOM_Y)` — the C6 fails the same assert at 446 vs 427 and is tracked as #95. |
| `EDGE_TOP_Y` | `75` | **`44`** | top 18 %. Not the strict 15 % (36) on purpose: 44 is exactly the chrome band's hit height, so "the top 44 px is edge territory" is one number instead of two that nearly agree. The C6 pair (dots y8..72 vs edge ≤75) had the same relationship, and it works because Rust classifies swipes from the touch driver, not from Slint hit-testing — a tap on a radio chip stays a tap. |
| `LAUNCHER_PAGE_SLOTS` | `9` | **`8`** ✅ | 4×2. **Two-sided with `Geom.launcher-slots` and the `page * slots + slot` indexing** — change both halves or tapping app N launches app M, silently. **Re-verified after Maze was dropped: still 8** (§1c). |

**Not a `board::ui` constant but coupled to the same change — `src/peripherals/touch.rs:159`:**

`SWIPE_MIN = 36`, whose own comment calls it *"~10 % of the 410px panel"*. On this panel
that single number is **11 % horizontally and 15 % vertically** — a landscape panel wants
two thresholds, not one:

```rust
SWIPE_MIN_X = 32   // 10 % of 320
SWIPE_MIN_Y = 24   // 10 % of 240
HOLD_SLOP_PX = 18  // must stay UNDER the smaller threshold, as the C6's 24 < 36 did
```

The invariant to preserve is the C6 comment's, not the value: `HOLD_SLOP_PX` is kept below
the swipe threshold so a **cancelled hold can still classify as the edge-swipe**.

---

## 1b. Settings hub — SOUND dropped, so the page count changes

JP dropped the SOUND section (owner-confirmed no `has-audio`: no codec, no speaker, so
volume/mic-gain/touch-tick were all inert). `ui/cyd/settings.slint` is now **five pages** —
DISPLAY · RADIOS · NETWORK · SYSTEM · BUTTONS.

| constant | C6 | **CYD** | note |
|---|---|---|---|
| `SETTINGS_PAGE_COUNT` | `6` | **`5`** | what the swipe handler pages against; the `titles` array and the tick rail in `settings.slint` are the other half |
| `HUB_PAGE_DISPLAY` | `1` | **`0`** | DISPLAY is now the first page. ⚠️ Its only consumer was the `HUB_SLIDER_BAND` check, which §2b-bis retires — so this may become dead rather than needing a value. Check before setting it. |

🟢 **This one fails VISIBLY, unlike the switcher/shade slot maps.** A mismatch lets Rust reach
page 5, which renders nothing — a blank page, not a wrong action. That is why this change
could land ahead of its Rust half and the card geometry could not. Still land them together.

### The BUTTONS page needs a board constant that does not exist yet

Per JP: *"treat button rows as gated on a board const, not assumed"* — **BOOT's existence on
this board is UNCONFIRMED**, and PWRON is the absent AXP2101's key.

`ui/cyd/settings.slint` therefore carries a LOCAL `property <bool> has-boot-key: false`, as a
stand-in. It cannot read a real board value: adding an `in property` to `WatchShell` would
break the 168+55 parity with the C6 root, since that root cannot be edited from this
workstream.

**Two ways to resolve it, and the choice is the watch session's:**
1. **Confirm the pin and flip the literal** — a one-character edit here, no Rust involved.
2. **Plumb it properly** — add `board::HAS_BOOT_KEY`, expose it as an `in property <bool>` on
   **both** roots, and push it once at boot. That is the right long-term shape (it is the
   same "predicate on a declared capability" pattern the manifest already uses) but it is a
   parity change, so it needs both roots in one commit.

Until then the page states that there are no mappable buttons and that the watch is driven by
the glass — which is true either way, and is the more useful sentence if this board really is
touch-only by design.

---

## 1c. Launcher slots, re-verified after Maze was dropped

With Voice, Sound and Maze all dropped the registry sections are **GAMES 6 · SYSTEM 7 ·
AUDIO 1**. `LAUNCHER_PAGE_SLOTS` stays **8**, and the reason is worth recording rather than
re-asserted: the binding section was never GAMES, it is **SYSTEM at 7**.

| slots | pages | note |
|---|---|---|
| **8** | **3** | GAMES 6/8, SYSTEM 7/8, AUDIO 1/8 |
| 7 | 3 | same page count — no gain |
| 6 | 4 | SYSTEM splits |

7 would page identically and buy nothing, because **tile size is set by the GRID, not the
slot count**: 4 columns is what makes a tile 70 px wide, 2 rows of 78 is what holds a 46×46
`AppIcon` plus two label lines. A 4×2 grid with 7 slots is the same 70 px tile with a hole.

### 🟥 Drop the three tiles from the BUILDER, not from `REGISTRY`

`REGISTRY`'s own comment: *"order == launch index, so adding anywhere else would silently
re-point every launcher tile after it."* Removal has the mirror hazard — Maze is `idx 5`, so
deleting its row shifts Settings 6→5, WLED 7→6, and everything after.

Verified against `build_launcher_pages`: each tile's `idx` comes from
`REGISTRY.iter().enumerate()` and `launch_state(idx)` indexes `REGISTRY`, so the two stay
consistent *with each other* across a removal — a freshly built launcher works. **The hazard
is anything holding an index from before the change**: a suspended-session record, a
persisted mapping, an OTA'd device with stale state. Those resolve to the wrong app, which is
the same silent-wrong-index class as the switcher slot map.

**So filter in the builder** — one line in `build_launcher_pages`'s `.filter()` closure,
skipping by capability (`has-imu` for Maze, `has-audio` for Voice/Sound) with `REGISTRY`
intact. No `idx` moves, every stored index stays valid, and it reads the way the manifest's
own comment says gates should: *"predicate on a declared capability, never on a chip name."*

### 🔶 One-line reorder worth making at the same time

`build_launcher_pages` emits sections in the order `[Audio, Games, System]`, and the C6
comment says why AUDIO leads: *"so Voice/Sound are reachable the instant the launcher opens."*
**Both are now dropped**, so page 0 is a single Story tile in an 8-slot grid — the launcher
opens on a nearly-empty page for a reason that no longer exists. `[Games, System, Audio]`
opens on the six games instead. Not a layout change, and not urgent, but it is a stale
rationale rather than a preference.

---

## 2. Pending — one per unlanded page

| constant | C6 value | fate | owned by wave |
|---|---|---|---|
| `SLIDER_BAND` | `330..=430` | **`182..=230`** — the CYD power page's slider sits at y186..226, padded for finger slop. ⚠️ At 330 the band is entirely off a 240 px panel, so TODAY every drag on that slider would ALSO flip the page | power ✅ |
| `HUB_SLIDER_BAND` | `170..=240` | **`66..=114`** — the CYD hub's DISPLAY slider is at absolute y70..110. ⚠️ **Reconcile BOTH C6 values, do not retune one:** `settings.slint:360-363`'s comment says `180..220` while the code says `170..240`. And note the old upper bound (240) was the C6 panel's *edge* — on this panel 240 IS the edge, so a stale value swallows **every** horizontal swipe on the page | settings ✅ |
| `SETTINGS_PAGE_COUNT` | `6` | **stays `6`** — page 0 (SOUND) is inert on this board but the count is Rust-owned and the tick rail reads it. Dropping to 5 is a later, separate call | settings ✅ |
| `STORY_PAUSE_RECT` | `(22,198,378,438)` | **`(8, 125, 168, 224)`** — tuple order is `(x0, x1, y0, y1)`, confirmed against the C6 value and `story.slint`'s PAUSE tile. The CYD PAUSE tile is x8..125, y168..224. ⚠️ It is `(0,0,0,0)` today, which **gates C5 playback off** — and that gate is the only thing preventing a mis-mapped tap, because at the C6's y378..438 **every** tap on this panel falls through to STOP and PAUSE simply vanishes. Duplicated in `main.rs`'s inline hit-test, whose own comment warns *"a stale constant here mis-routes a tap"* | story ✅ |
| `VISIBLE_CHAPTERS` | `5` | **`3`** (`Geom.max-chapters`) — ⚠️ it is also the **pager stride**, so NEWER/OLDER paging behaviour changes with it | story ✅ |

---

## 2b. Switcher and shade — DECIDED, and the stacks stayed VERTICAL on purpose

The spec's landscape answer for both was a horizontal card strip. It is the
better-looking answer and it is the wrong one, for a reason that only shows up on
the Rust side.

`switcher_slot()` / `shade_slot()` invert a swipe's `start_y` back to a slot index
by arithmetic. Going horizontal does not RETUNE those functions, it REPLACES them
— a new inverse over `start_x`, new constants, in the two functions whose failure
mode is the nastiest in the port: **they return a wrong index rather than `None`**,
so a kill-swipe kills the wrong session and a dismiss-swipe dismisses the wrong
notification, with nothing to observe.

Three cards fit the 200 px content band vertically with room to spare. So the
change is **three constants in a function whose shape is already proven**, instead
of a new function whose bugs are silent. Reviewability beats elegance on a
two-sided constant.

| constant | C6 | **CYD** | derivation |
|---|---|---|---|
| `SWITCHER_CARD_TOP` | `110` | **`40`** | first card sits 6 px under the 34 px title strip |
| `SWITCHER_CARD_H` | `84` | **`52`** | floored by the 46x46 `AppIcon`, which cannot shrink — its 17 glyphs are hand-placed rects. Holds icon y3..49 + name y6..24 + PAUSED y30..44 |
| `SWITCHER_CARD_PITCH` | `96` | **`58`** | 52 + 6 gap. `40 + 3*58 = 214`, last card ends y208 |
| `SWITCHER_CARDS` | `4` | **`3`** | `Geom.max-cards` |
| `SHADE_CARD_TOP` | `76` | **`38`** | |
| `SHADE_CARD_H` | `84` | **`60`** | needs one more line than a switcher card (title + age + body) |
| `SHADE_CARD_PITCH` | `92` | **`66`** | 60 + 6 gap. `38 + 3*66 = 236`, last card ends y230 |
| `SHADE_CARDS` | `4` | **`3`** | `Geom.max-cards` |

🟢 **The shade reduction is also a heap win on the page that needs one most.** With
4 cards it is **264 items / 207 glyphs** — the largest single scene in the whole
watch. Scene-item counts do not shrink with the panel (items are per-element, not
per-pixel) and `PrepareScene`'s Vecs grow by DOUBLING, so the rungs that fail at
54-66 kB free sit exactly where they did on the C6. 3 cards with a one-line body
is fewer items on the one scene that was already at the top of the ladder.

---

## 2c. Not a hit-rect — a one-line Rust change the power page needs

`set_power` fuses each subsystem cell into ONE string:

```rust
ui.set_cpu_cell(slint::format!("{}MHz \u{00b7} {}mA", stats.cpu_mhz, stats.base_ma()));
```

The left half is a **fact** (the CPU really is at 160 MHz; WiFi really is on). The right
half is `power_stats.rs`'s model — which on this board is a model of current drawn from a
battery that does not exist. Slint cannot split a string it is handed, so `ui/cyd/power.slint`
renders both halves and the mA figures survive against JP's "no mA estimator readings".

**Proposed:** drop the `· NNmA` suffix for the six cells under
`#[cfg(not(feature = "has-pmu"))]`. Every cell becomes state-only with **no layout change** —
the CYD page is already laid out for the shorter strings, so nothing reflows when this lands.

`total-ma`, `runtime-text`, `left-hours` and `lp-core-text` need no Rust change: the CYD page
simply does not render them. `runtime_text` is the worst of the four — it is
`full_runtime_hours(BATTERY_CAPACITY_MAH)`, the model divided by the capacity of a cell that
is not there, and it would cheerfully report "100%: 4h · left: ~3h" for a device that runs
until unplugged.

---

## 2d. Soft douse — what the UI now promises

JP, 2026-08-25: deep sleep is **version-blocked** on the C5 (the HAL generation with sleep
breaks the radio), so `power-shutdown-tap` becomes a **SOFT DOUSE** — screen off, radios off,
UI halted, **tap the glass to wake**.

`ui/cyd/power_menu.slint` states exactly that, in words and in a drawn ring-and-dot tap mark:
*"screen and radios off · tap the glass to wake"*. **The UI is now making that promise**, so
the Rust side has to keep it:

* the wake path must be the **touch IRQ** (confirmed wake-capable), not a button — an earlier
  revision of this caption said "press BOOT to relight", which was wrong twice over: the wake
  is a touch, and BOOT's existence here is unconfirmed. Sending a user hunting for a button
  that may not exist, to recover from a state they deliberately entered, is the worst failure
  this menu could have;
* **radios must come back with the screen.** The caption says radios go off, so the user will
  expect them back on wake without a further action. If the relight path cannot restore them,
  the caption needs to change *before* the firmware ships, not after.

The button keeps its `danger` variant even though the recovery is one tap: it is still the
control that makes the watch stop responding, and the only one here whose undo is a separate
deliberate act.

---

## 3. One thing that does NOT need changing

`src/ui/slint_platform.rs:49-50` derives `WIDTH`/`HEIGHT` from `board::LCD_*` and is already
parametric. Only its trailing `// 410` / `// 502` comments go stale.

---

## 4. Acceptance, restated because "it looks right" does not cover it

Every one of these must be checked by **tapping the control at its own location and
confirming that control fired** — not by confirming that a tap does something. The
slot-inverse maps return wrong indices rather than errors, so a per-slot tap check is the
only test that can distinguish a correct map from a plausible one.
