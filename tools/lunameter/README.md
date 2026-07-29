# lunameter — per-screen scene cost for the Slint software renderer

Renders the **real** `ui/slint/shell.slint` tree through the **real** vendored
renderer (`crates/i-slint-renderer-software`) on the host, and prints what each
screen costs the watch's allocator. Built for #75 (the apps-menu OOM); kept
because "this screen is lighter now" should be a number, not a feeling.

```bash
tools/lunameter/measure.sh                              # this checkout
LUNAMETER_OUT=/tmp/after tools/lunameter/measure.sh      # + dump PPM renders
WATCH_UI_ROOT=/other/checkout tools/lunameter/measure.sh # another tree (A/B)
```

Host build only. Touches no hardware and opens no serial port.

## Why the numbers matter

`PrepareScene` builds one `Vec` per primitive kind per frame, and since the
scene-vector pooling (`f334785`) those buffers **keep their capacity** for the
life of the process. Two consequences:

1. A screen's **resident** cost is the doubling rung its peak count lands on.
2. The pool is sized by the **worst screen ever visited**, not the current one.

Element sizes differ between host and target — `SceneTexture` holds a slice, so
it is 40 B on x86_64 but **28 B on riscv32**. The harness prints both the host
`size_of` values and the riscv32 rung arithmetic; trust the latter for the watch.

| Vec | riscv32 elem | cap 64 | cap 128 | cap 256 | cap 512 |
|---|---|---|---|---|---|
| `items` (`SceneItem`) | 16 B | 1024 | 2048 | 4096 | 8192 |
| `vectors.textures` (`SceneTexture`) | 28 B | 1792 | **3584** | 7168 | 14336 |
| `vectors.rounded_rectangles` | 26 B | 1664 | 3328 | 6656 | 13312 |

This is how #75's captured allocation failures were identified: 4096 B was
`SceneItem[256]`, 3584 B was `SceneTexture[128]` (**not** `state_stack` — see
below), 3328 B was `RoundedRectangle[128]`. Each is one screen crossing one
doubling boundary.

## Two things worth knowing before optimising a screen

**Glyphs dominate.** `draw_text_paragraph` emits one scene item *per rendered
glyph* (`process_target_texture` in the glyph loop), so a caption is not "one
item", it is one per non-space character. On most screens 80-90 % of the items
are glyphs. Shorter copy is the lever; flattening element nesting is not.

**Nesting depth is a non-lever.** `max_state_depth` is the `SceneBuilder`
state-stack high-water, which equals item-tree traversal depth (i-slint-core's
`render_item_children` does `save_state` → recurse → `restore_state`). It
measures 12 across every screen, so `Vec<RenderState>` never exceeds capacity 16
= 448 B. A 3584 B `state_stack` would need a 65-deep tree.

**There is no occlusion culling.** Items behind a full-screen opaque cover are
still built. That is what the `covered` gate in `shell.slint` exists to prevent,
and it is why AOD costs 15 items instead of 106.

## What it measures

Every screen, plus the worst-case content variants that actually decide a rung
(`MUTED` reads 5 glyphs where `11` reads 2; the longest `ButtonAction` label on
all four mapping rows; a full 6-row WiFi picker). The watchface is re-measured
after an overlay closes, as a re-show regression check.

It also runs an **input probe**: it dispatches a real pointer press at the WIFI
radio dot with the chrome covered and reports whether the callback fires. That is
how the claim "`visible: false` culls hit-testing as well as draw" is verified
rather than asserted.

## Keeping the numbers real

`instrument.py` re-derives the instrumented renderer from
`crates/i-slint-renderer-software` on every run and **asserts every patch
anchor**. If a Slint bump or a local renderer change moves one, the run fails
loudly instead of measuring a stale copy. There is exactly one renderer in the
repo.

Feature flags mirror the firmware's `slint` dependency and the build uses
`EmbedForSoftwareRenderer`, so the glyph and scene code paths under measurement
are the ones that ship.

`RepaintBufferType::NewBuffer` is used deliberately, even though the firmware
runs `ReusedBuffer`: it forces a complete scene build every frame. Partial
rendering filters items by dirty region, which understates the peak — and the
peak is what has to be allocatable, and what the pool is sized by. (First
measured with `request_redraw()` on `ReusedBuffer`; every count matched except
one frame that changed only four short strings, which is exactly the failure mode
`NewBuffer` removes.)

## Adding a screen

Set its properties and call `frame("label", &mut sink)` in `src/main.rs`. Mirror
whatever Rust pushes into the model (`build_launcher_pages` and friends) — the
counts are content-dependent, so measure the realistic worst case, not the
default-constructed one.
