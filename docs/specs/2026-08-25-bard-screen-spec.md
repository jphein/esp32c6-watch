# bard on-glass screen — implementation spec (for Luna)

**Status:** ready to build. The engine is done and proven; this is the UI slice.
**Owner:** Luna (cross-scene-root UI, board geometry, glass verification).
**Author:** this session, after wiring the on-device engine (`bard` feature, be34fac).

## What already exists (do not rebuild)
- `crates/bard-core` — the SBRD transformer + tokenizer + persona + textflow + delivery. Pure, 40 golden tests. **Generation is proven correct.**
- `src/apps/bard.rs` (feature `bard`) — `generate(prompt, seed, max_tokens)` runs `Story::step` to completion and prints over serial. This proves the engine FITS + RUNS on-device (verify at the next tether: `bard <prompt>` on the console).
- `bard_core::MODEL` — the 277 KB model blob, flash-resident (feature-gated, not `.bss`; the bard image holds a 73,584 B stack gap over the 71,680 floor).

## What to build
A **story-generation screen** — the payoff being that **the watch SPEAKS its stories** (bard text → the existing TTS path). Model it on the existing `Story` overlay (the LitRPG reader) — same registry+overlay shape, different source (local model, not the daemon).

### 1. Registry entry (`src/apps/registry.rs`)
Add an `AppState::Bard` variant (append to the enum — order is the launch index, never insert) and a REGISTRY row:
```rust
AppDescriptor { state: AppState::Bard, name: "Bard", icon_id: <next free>, accent: 0xa78bfa, section: Games /* or Audio */, kind: Overlay, flags: AppFlags::NONE }, // idx <n>
```
Gate the whole thing on the `bard` feature (like `story` gates its row). `AppFlags::NONE` — bard needs NO WiFi (that's the point: local generation, offline).

### 2. Scene page — BOTH roots (`ui/slint/story.slint` sibling + `ui/cyd/`)
A `BardPage` overlay component. Minimum viable surface:
- A **title/persona line** (from `bard_core::persona::protagonist(node_id)` — "the little dragon" etc., pushed from Rust).
- A **scrolling text region** for the generated story (grows as tokens stream; a rolling window like textflow's `append_rolling`).
- A **GENERATE** button (regenerate with a fresh seed) and a **SPEAK** button (pipe current text to TTS).
- Close chevron (Flag idiom, like ThemeOverlay).
Geometry is yours per root: C6 portrait 410×502, CYD landscape 320×240 (`board::ui` for hit rects). The text region is the layout question that needs glass.

### 3. Rust glue (`src/ui/slint_shell.rs` + `src/main.rs`, feature `bard`)
- Shell setters: `set_bard_title`, `set_bard_text`, `set_bard_generating`.
- A generation task/step pump: bard's `Story::step` is INCREMENTAL — call it a few tokens per render tick (NOT to completion in one call, which would park the loop for ~1-2 s and stall the UI). Push the rolling text each tick. This is the one runtime subtlety: **step incrementally off the render clock**, like the ping-pulse tick.
- SPEAK button → the existing `voice_tts::speak_text` path (the same one notifications use). The text is already in a buffer; hand it over. **This is the marquee feature — the bard speaks.**
- The `Box<Bufs>` + Session live for the screen's lifetime (heap; free on close — the OTA/cast decline-on-pressure discipline: if `try_reserve` fails, show "not enough memory" rather than OOM).

## Guardrails (from this campaign's hard-won rules)
- **Feature-gated `bard`** end to end — default C6/C5/S3 builds must stay byte-identical (verify: `fambuild build --release` diff-clean).
- **Incremental stepping** — never run generation to completion synchronously in the render/UI loop.
- **Heap, not `.bss`** for Bufs/KV — decline-on-pressure, never a static.
- **Both scene roots compile** — `slint_shell.rs` builds against C6 portrait AND CYD landscape; the `bard` properties must exist on both roots (dormant is fine, like §1d's pattern).
- Board-parameterize any hit rect via `board::ui` — don't hardcode a panel's geometry in the shared scene (the §1d lesson).

## Verification
- Host: none needed (engine is golden-tested).
- Glass (Luna + bench): layout on both panels, incremental streaming looks smooth at the render fps, GENERATE reseeds, SPEAK plays through TTS. The runtime fit (Bufs + KV heap under the live scene) is the thing to watch — confirm the console `bard` generator runs first at a tether so the budget is known before the screen rides on top of it.

## Why a spec, not a blind wire
The engine is proven and feature-gated; the SCREEN is cross-root layout that genuinely needs glass to get right, and geometry-in-shared-scenes is exactly the class of bug §1d existed to kill. Building it blind would violate the no-untested-UI discipline this campaign held throughout. Over to you.
