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

### maze — `CELL 40 → 18`

```rust
const CELL: i32 = 18;   // was 40
// OX / OY formulas unchanged, panel literals -> board::LCD_*
```

10×12 cells. Height allows 20 (`240/12`), which would make `OY` exactly 0 — full-bleed,
with the outer wall on the bezel. 18 gives 180×216 with `OX: 70` / `OY: 12`, so the maze
has a visible border on a panel whose edges a finger will actually touch.

🔶 **Maze is IMU-tilt-only.** `AppInput.accel` is a plain tuple, so it receives `(0,0,0)`
and the ball never moves. It **compiles and runs** — unplayable, not broken. JP's drop list
did not name it; flagging it because "the game opens and nothing happens" is the exact
shape of report the dropped-app policy exists to prevent.

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

**Recommendation: B.** 34 % of the shared world is close enough to the C3's 26 % that a CYD
player is competing like a fleet node rather than like a watch, and this board is a *watch*
firmware. The cost is chunkier cells — see §3, which makes that cost concrete.

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

---

## 4. Acceptance per game

1. **Playfield fully visible** — walk all four edges. Landscape bugs surface at the RIGHT
   edge, where portrait bugs surfaced at the bottom, so check the right edge deliberately.
2. **No out-of-bounds draw.** `OX`/`OY` formulas make this self-correcting; snake's new
   formula is the one to verify, since it replaces a hand-set margin.
3. **HUD not overlapped by the field** — every `OFFSET_Y` / `BOARD_Y` / `VIEW_Y` above is a
   HUD budget, and the HUD is drawn by code this document does not touch.
4. **flappy: playable.** Not "renders" — playable. `GRAVITY`/`JUMP_VEL` are derived, and
   derived physics is a hypothesis.
5. 🔶 **maze: expect a stationary ball** (no IMU). That is the correct behaviour for this
   hardware, not a regression to chase.
