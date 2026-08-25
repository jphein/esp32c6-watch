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
| `EDGE_BOTTOM_Y` | `427` | **`204`** | bottom 15 % of the panel — `427/502 = 85 %`, `204/240 = 85 %`. The C6 value is entirely **off** a 240 px panel, which makes bottom-edge swipe-up (launcher) and hold-to-switcher **unreachable** until it moves. |
| `EDGE_TOP_Y` | `75` | **`44`** | top 18 %. Not the strict 15 % (36) on purpose: 44 is exactly the chrome band's hit height, so "the top 44 px is edge territory" is one number instead of two that nearly agree. The C6 pair (dots y8..72 vs edge ≤75) had the same relationship, and it works because Rust classifies swipes from the touch driver, not from Slint hit-testing — a tap on a radio chip stays a tap. |
| `LAUNCHER_PAGE_SLOTS` | `9` | **`8`** ⏳ | landscape wants 4×2. **Two-sided with `Geom.launcher-slots` and the `page * slots + slot` indexing** — change both halves or tapping app N launches app M, silently. Confirm when the launcher wave lands. |

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

## 2. Pending — one per unlanded page

| constant | C6 value | fate | owned by wave |
|---|---|---|---|
| `SLIDER_BAND` | `330..=430` | **`182..=230`** — the CYD power page's slider sits at y186..226, padded for finger slop. ⚠️ At 330 the band is entirely off a 240 px panel, so TODAY every drag on that slider would ALSO flip the page | power ✅ |
| `HUB_SLIDER_BAND` | `170..=240` | ⏳ clips exactly at the bottom edge; moves with the Settings DISPLAY slider. ⚠️ `settings.slint:360-363`'s comment says `180..220` while the code says `170..240` — **they already disagree, and whoever moves it must reconcile both** | settings |
| `STORY_PAUSE_RECT` | `(22,198,378,438)` | ⏳ currently `(0,0,0,0)` on the C5, which **gates story playback off** — so nobody can ship a mis-mapped story page by accident. Duplicated in `main.rs`'s inline hit-test, whose own comment warns the geometry *"must match story.slint's READ-page tiles exactly"* | story |
| `VISIBLE_CHAPTERS` | `5` | **`3`** (`Geom.max-chapters`) — ⚠️ it is also the **pager stride**, so NEWER/OLDER paging behaviour changes with it | story |

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

## 3. One thing that does NOT need changing

`src/ui/slint_platform.rs:49-50` derives `WIDTH`/`HEIGHT` from `board::LCD_*` and is already
parametric. Only its trailing `// 410` / `// 502` comments go stale.

---

## 4. Acceptance, restated because "it looks right" does not cover it

Every one of these must be checked by **tapping the control at its own location and
confirming that control fired** — not by confirming that a tap does something. The
slot-inverse maps return wrong indices rather than errors, so a per-slot tap check is the
only test that can distinguish a correct map from a plausible one.
