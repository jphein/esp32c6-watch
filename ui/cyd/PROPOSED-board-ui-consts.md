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
| `SLIDER_BAND` | `330..=430` | ⏳ entirely off-panel; moves with the power page's brightness slider | power |
| `HUB_SLIDER_BAND` | `170..=240` | ⏳ clips exactly at the bottom edge; moves with the Settings DISPLAY slider. ⚠️ `settings.slint:360-363`'s comment says `180..220` while the code says `170..240` — **they already disagree, and whoever moves it must reconcile both** | settings |
| `SWITCHER_CARD_TOP` / `_H` / `_PITCH` | `110 / 84 / 96` | ⏳ 4 stacked 84 px cards need ~380 px of 240; the landscape answer is a horizontal strip, which changes the slot-inverse math | switcher |
| `SWITCHER_CARDS` | `4` | **`3`** (`Geom.max-cards`) | switcher |
| `SHADE_CARD_TOP` / `_H` / `_PITCH` | `76 / 84 / 92` | ⏳ same | shade |
| `SHADE_CARDS` | `4` | **`3`** (`Geom.max-cards`) | shade |
| `STORY_PAUSE_RECT` | `(22,198,378,438)` | ⏳ currently `(0,0,0,0)` on the C5, which **gates story playback off** — so nobody can ship a mis-mapped story page by accident. Duplicated in `main.rs`'s inline hit-test, whose own comment warns the geometry *"must match story.slint's READ-page tiles exactly"* | story |
| `VISIBLE_CHAPTERS` | `5` | **`3`** (`Geom.max-chapters`) — ⚠️ it is also the **pager stride**, so NEWER/OLDER paging behaviour changes with it | story |

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
