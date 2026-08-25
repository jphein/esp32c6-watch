# PROPOSAL — framebuffer-game geometry for 320×240

**Author:** Luna (layout workstream) · branch `feat/cyd-c5-layout`
**For:** the watch session. Every constant below lives in `src/apps/*.rs`, which is outside
this workstream's tree ownership — so this is a reviewable proposal, not an edit, exactly
like `PROPOSED-board-ui-consts.md`.

The six `kind: Framebuffer` games touch **no `.slint` at all**. They draw through
`embedded-graphics` `DrawTarget` and their geometry is Rust constants, which is precisely
why they are invisible to every search that found the rest of this port.

---

## 0. The good news, stated first

**Every grid fits. No game needs its grid reduced** — only its cell size re-picked. And
four of the six already express their origin as a centring formula, so the origin
recomputes itself once the panel constants are right.

Five games, not six: **Maze is dropped** (IMU-only, JP 2026-08-25) — see §2.

**`src/drivers/framebuffer.rs` needs no change at all.** It already derives `WIDTH`/`HEIGHT`
from `board::LCD_*`, and the half-res backing store's `/2` is exact on both panels
(410×502 → 205×251; 320×240 → 160×120). The store shrinks from ~51 KB to **19.2 KB** for
free. Its header comment goes stale, though — it says "no PSRAM" and names the C6's
512 KB, both of which are C6 facts.

---

## 1. Two literals to parameterise first

Four games hardcode the panel. These should read `board::LCD_WIDTH` / `board::LCD_HEIGHT`
rather than being retyped, because a second board is exactly the situation that makes a
literal a bug:

| file | literals |
|---|---|
| `tetris.rs:21-22` | `SCREEN_W: 410`, `SCREEN_H: 502` |
| `game2048.rs:19` | `410` inside `BOARD_X` |
| `maze.rs:18-19` | `410`, `502` inside `OX` / `OY` |
| `flappy.rs:14-15` | `W: 410`, `H: 502` |
| `world_snake.rs:566-567` | `SCREEN_W: 410`, `SCREEN_H: 502` |

`snake.rs` has no panel literal — it has no centring formula either (`OFFSET_X: 5` is a
raw margin), so it is the one game that needs a formula ADDED rather than a literal
replaced.

---

## 1b. 🟥 DROP TILES FROM THE BUILDER, NOT FROM `REGISTRY`

Three apps are dropped on this board — **Voice**, **Sound** and now **Maze**. There are two
ways to do that and only one is safe.

`REGISTRY`'s own comment is explicit: *"order == launch index, so adding anywhere else would
silently re-point every launcher tile after it."* Removing has the mirror hazard. Maze is
`idx 5`, so deleting its row shifts Settings 6→5, WLED 7→6, and everything after.

Verified how the indices are actually produced (`build_launcher_pages`): each tile carries
`idx` from `REGISTRY.iter().enumerate()`, and `launch_state(idx)` indexes `REGISTRY`. So the
two stay consistent *with each other* across a removal — a freshly built launcher would work.
The hazard is anything holding an index from BEFORE the change: a suspended-session record,
a persisted mapping, an OTA'd device with stale state. Those resolve to the wrong app, and
the wrong app launching is the same silent-wrong-index failure class as the switcher slot map.

**So: filter in the builder.** One line in `build_launcher_pages`'s `.filter()` closure —
skip the three by capability (`has-imu` for Maze, `has-audio` for Voice/Sound) and leave
`REGISTRY` intact. Then no `idx` moves at all, every stored index stays valid, and the
capability gate reads the way the manifest's own comment says gates should
(*"predicate on a declared capability, never on a chip name"*).

`Geom.launcher-slots` and the grid are unaffected either way — see §0 and the launcher's own
header for why 8 survives the drop.

---

## 2. Per-game constants

Height is the binding axis in every single case. That is worth noticing: it means none of
these numbers came out of taste — each is `floor((240 − HUD) / rows)`.

### snake — `GRID_SIZE 20 → 10`

```rust
const GRID_SIZE: i32 = 10;                                    // was 20
const OFFSET_X: i32 = (board::LCD_WIDTH as i32 - GRID_W * GRID_SIZE) / 2;  // was 5
const OFFSET_Y: i32 = 28;                                     // was 60 (HUD strip)
```

20×21 cells. Width allows 16 (`320/20`), height allows 10 (`(240−28)/21`), so height binds.
Playfield 200×210 at y28..238. `OFFSET_X` becomes the formula the other games already have.

### tetris — `BLOCK 30 → 13`

```rust
const BLOCK: i32 = 13;   // was 30;  (240 - OY) / GH = (240-20)/16 = 13.75
const OY: i32 = 20;      // unchanged
```

Playfield 156×208 at y20..228; `OX` recomputes to 82.

🟢 **An opportunity, not a requirement:** 156 px of 320 leaves **164 px of unused width**.
On the C6 this game is a full-panel column with nothing beside it; landscape has room for a
next-piece preview and a score panel to the right of the well. That is a gameplay
improvement rather than a port task — flagging it because the space is free and will
otherwise just be black.

⚠️ Tetris's *tilt-assist* input needs the IMU this board lacks. Buttons and touch still
play it — **not a drop candidate, just a degraded input.**

### 2048 — `CELL_SIZE 90 → 45`, `GAP 8 → 6`, `BOARD_Y 70 → 40`

```rust
const CELL_SIZE: i32 = 45;   // was 90
const GAP: i32 = 6;          // was 8
const BOARD_Y: i32 = 40;     // was 70 (HUD strip)
const BOARD_X: i32 = (board::LCD_WIDTH as i32 - GRID as i32 * CELL_SIZE
                      - (GRID as i32 - 1) * GAP) / 2;   // formula kept, literal replaced
```

Board 198×198 at y40..238; `BOARD_X` recomputes to 61.

The layout spec computed 52 px cells instead. That is the *maximum* rather than the right
answer: `4*52 + 3*6 = 226`, which leaves `240 − 226 = 14 px` of HUD — not enough for the
score line this game draws. 45 keeps a 40 px HUD, and the board is square either way.

### maze — 🔴 **DROPPED. No geometry needed.** (JP, 2026-08-25)

Maze is IMU-tilt-only: `AppInput.accel` is a plain tuple, so on this board it receives
`(0,0,0)` and the ball never moves. It compiles, runs, and does nothing.

JP dropped it on exactly that argument — *"the game opens and nothing happens"* is the shape
of report the dropped-app policy exists to prevent, and it is worse than an absent tile
because the user cannot tell it from a bug in their own input.

**So there is no `CELL` to re-pick.** For the record, had it been kept: `CELL 40 → 18`
(height allows 20 at `240/12`, which puts `OY` at exactly 0 and the outer wall on the bezel;
18 gives 180×216 at `OX: 70` / `OY: 12`).

⚠️ **Drop it from the TILE BUILDER, not from `REGISTRY`** — see §1b. This applies to Voice
and Sound identically.

### flappy — a scroller, so it scales by PROPORTION not by grid

This is the only game with no grid, and the only one where the numbers change *feel*
rather than just fit. Every value below is the C6's, scaled by its own axis:

```rust
const W: i32 = board::LCD_WIDTH  as i32;   // was 410
const H: i32 = board::LCD_HEIGHT as i32;   // was 502
const BIRD_X: i32   = 70;    // was 90   — 22 % of width, unchanged proportion
const BIRD_R: i32   = 10;    // was 14   — 14 px is 2.8 % of 502 but 5.8 % of 240
const PIPE_W: i32   = 44;    // was 55   — 13.4 % of width, unchanged
const PIPE_GAP: i32 = 60;    // was 140  — 6x BIRD_R, vs the C6's 10x
const GROUND_H: i32 = 16;    // was 30   — ~6.5 % of height, unchanged
const MARGIN: i32   = 0;     // was 6    — "safe margin for rounded screen": no arc here
const PIPE_SPEED: f32 = 2.0; // was 2.5  — preserves time-to-cross on a narrower field
const GRAVITY: f32  = 0.22;  // was 0.45 — the vertical axis halved
const JUMP_VEL: f32 = -3.1;  // was -6.5 — same, so the arc keeps its shape
```

⚠️ **`GRAVITY` and `JUMP_VEL` are the two numbers in this whole document that cannot be
derived, only tuned.** Left at the C6 values on a half-height field the bird flings off the
top on one tap; scaled by `240/502` the arc *should* occupy the same fraction of the field,
but "should" is doing real work in that sentence. **Tune on glass, and expect to.**

`MARGIN` is a free deletion and worth naming: its own comment says *"Safe margin for
rounded screen."* This glass is rectangular.

### world_snake — 🔶 A GAMEPLAY DECISION, NOT A LAYOUT ONE. FOR JP.

`VIEW_COLS`/`VIEW_ROWS` is a **viewport into a shared 256×256 multiplayer world**. Shrinking
it means a CYD player *sees less of the world than a C6 player* — a competitive asymmetry,
not a cosmetic choice.

🟢 **There is decisive precedent**, and it is in the file's own comment: *"the C3 fleet
renders 4 px on a 72×40 OLED; the watch has room for 16 px."* Heterogeneous viewports are
already shipping across three panel sizes. So this is a tuning call with precedent, not a
new fairness problem — but the numbers deserve to be seen rather than summarised, because
the gap is wider than "smaller panel" suggests:

| board | cell | viewport | **cells visible** | vs C6 |
|---|---|---|---|---|
| C6 watch | 16 px | 25×28 | **700** | — |
| C3 fleet | 4 px | ~18×10 | **~180** | 26 % |
| CYD, option A | 16 px | 20×12 | **240** | **34 %** |
| CYD, option B | 12 px | 25×16 | **400** | **57 %** |

**Option A** (the layout spec's recommendation) keeps the C6 cell size, so the world looks
identical and you simply see a keyhole of it:
```rust
const CELL_PX: i32 = 16;  const VIEW_COLS: u16 = 20;  const VIEW_ROWS: u16 = 12;
const VIEW_Y: i32 = 48;   // 20*16 = 320 exactly, so VIEW_X computes to 0 (full-bleed)
```

**Option B** trades cell size for field of view — 400 cells is a *playable* share of the
C6's 700, and 12 px is still 3× the C3's 4 px:
```rust
const CELL_PX: i32 = 12;  const VIEW_COLS: u16 = 25;  const VIEW_ROWS: u16 = 16;
const VIEW_Y: i32 = 44;   // 300x192 at VIEW_X 10, VIEW_Y 44
```

**✅ RULED: OPTION B** (JP, 2026-08-25). 57 % of the shared world, at 12 px cells.

The reasoning that carried it: 34 % is close enough to the C3 fleet's 26 % that a CYD player
would be competing like a fleet node rather than like a watch, and this is *watch* firmware.
The cost is chunkier cells — 12 px is 6 effective pixels after the half-res store, see §3 —
and that cost was accepted deliberately in exchange for field of view.

---

## 3. ⚠️ EFFECTIVE RESOLUTION IS HALF, AND IT CHANGES HOW THESE NUMBERS READ

The games render through `Framebuffer`, a **half-res RGB332 store upscaled 2× at flush**.
So every cell size above is **half** what it looks like on paper:

| game | proposed cell | **effective pixels** |
|---|---|---|
| snake | 10 px | **5** |
| tetris | 13 px | **6.5** |
| maze | 18 px | **9** |
| 2048 | 45 px | **22.5** |
| world_snake (A) | 16 px | **8** |
| world_snake (B) | 12 px | **6** |

A 5-effective-pixel snake segment is visibly blocky. That is not new — the C6's 20 px cell
was 10 effective — but it halves again here, and snake/tetris/world_snake are the three
that feel it.

🔶 **The C6's reason for half-res is gone.** `framebuffer.rs`'s own header explains it:
*"the C6 has 512 KB of SRAM total… a full-res RGB332 frame can't coexist with the Slint
scene + WiFi/BLE/mesh in the one main heap region."* This board has **8 MB of PSRAM**, and
at 320×240 the numbers are much smaller anyway:

| store | bytes |
|---|---|
| half-res RGB332 (current scheme) | **19.2 KB** |
| full-res RGB332 | **76.8 KB** |
| full-res RGB565 | **153.6 KB** |

**JP called Q6 out of scope — measure first.** Recorded here so the measurement has a
target, and so nobody re-derives it. Two cautions if it is picked up:

* It is a **driver/memory change, not a layout one** — sequence it separately from these
  constants. Doing both at once means a blocky-game report has two possible causes.
* `cyd_c5.rs` warns explicitly against inheriting C6 memory numbers. The 76.8 KB above is
  arithmetic; whether it *coexists* with the scene on this board is a measurement nobody
  has taken.

If full-res lands, every cell size in §2 becomes less cramped and 2 of the 6 games
(snake, world_snake) would be worth re-picking.

⚠️ **But full-res does not make anything FASTER — it makes it slower.** A full-res store is
the same number of pixels to stream out of, and 4× the bytes to fill. It buys sharpness, not
frame rate; §3b is the constraint that decides how the games feel.

---

## 3b. 🟥 THE DISPLAY LINK CAPS EVERY GAME'S FRAME RATE, BEFORE RENDER TIME

This landed after §2 was written and it is the most important thing in this document,
because it bounds all six games from outside their own code.

A full 320×240 RGB565 flush over this panel's SPI link is **61.4 ms at the vendor's
20 MHz** (37.2 ms even at the 33 MHz hard ceiling). So:

> **A game that flushes a full frame cannot exceed ~16 fps, with ZERO time left for
> render or game logic.**

`flappy.rs`'s own header says *"throttled 30fps"*. That throttle is now unreachable — the
link will not carry 30 full frames a second at any legal clock. It is not a tuning number
any more, it is a ceiling to be lowered to match reality (~15 fps, leaving headroom) rather
than a target to be missed silently.

**This splits the six games into two classes, and the split is not about cell size:**

| class | games | why | consequence |
|---|---|---|---|
| **grid, mostly static** | snake · 2048 · tetris · maze | only changed CELLS need redrawing between frames — the board is otherwise identical | can flush **dirty rects** and run far above 16 fps |
| **full-field scrollers** | flappy · world_snake | the whole field translates every frame, so every pixel is dirty by construction | hard-capped at **~16 fps**; no layout number changes this |

⏳ **Flagged, not solved:** whether the four grid games actually flush dirty rects today is
a `Framebuffer`/runner question I cannot answer from the geometry — `flush` may well push
the whole store regardless of what changed. If it does, all six are capped at 16 fps and the
grid games are paying for a partial-update path they do not use. **That is worth checking
before any cell size above is judged**, because "the game feels sluggish" would then have
nothing to do with the numbers in §2.

Two consequences for the constants themselves:

* **`PIPE_SPEED` is a per-FRAME delta, not a per-second one.** At 2.0 px/frame it crosses
  320 px in 160 frames — which is 5.3 s at 30 fps but **10.7 s at 16 fps**. So the value
  proposed in §2 makes the game *half as fast* as intended, not the same speed. The same
  applies to `GRAVITY` and `JUMP_VEL`: they are per-frame accumulations, so a halved frame
  rate halves the bird's apparent acceleration too. **Tune all three together, on glass,
  against the real frame rate — not against the C6's.**
* **`world_snake`'s viewport choice interacts with this.** Option B's smaller cells mean
  more cells changing per step, but the field scrolls anyway, so its cost was already
  full-frame. Option B is therefore free on this axis — the viewport decision stays a
  gameplay call.

⚠️ And do not reach for PSRAM here. The wall is the **display link**, not memory. A
framebuffer in PSRAM would still have to be streamed out over the same SPI bus at the same
61.4 ms per full frame.

---

## 4. Acceptance per game

1. **Playfield fully visible** — walk all four edges. Landscape bugs surface at the RIGHT
   edge, where portrait bugs surfaced at the bottom, so check the right edge deliberately.
2. **No out-of-bounds draw.** `OX`/`OY` formulas make this self-correcting; snake's new
   formula is the one to verify, since it replaces a hand-set margin.
3. **HUD not overlapped by the field** — every `OFFSET_Y` / `BOARD_Y` / `VIEW_Y` above is a
   HUD budget, and the HUD is drawn by code this document does not touch.
4. **flappy: playable.** Not "renders" — playable. `GRAVITY`/`JUMP_VEL`/`PIPE_SPEED` are
   derived, derived physics is a hypothesis, and §3b means they are derived against a frame
   rate that is itself now a different number. Measure the achieved fps FIRST, then tune the
   three together against it.
5. **Measure the achieved frame rate per game** and record it. It is the number that decides
   whether §2's cell sizes are the problem or a red herring.
6. 🔶 **maze: expect a stationary ball** (no IMU). That is the correct behaviour for this
   hardware, not a regression to chase.
