# Watch UI Overhaul — Touch Feedback & Interaction Design Spec

**Date:** 2026-07-23
**Author:** Nebula (research + design foundation)
**Implementer:** Luna (`fix/ui-overhaul`)
**Status:** design foundation — for JP review via `design-preview.html`
**Directive (JP):** *"I need more touch feedback on all actions… do a complete overhaul on everything using all the best design practices."*

---

## 1. The finding (why this spec exists)

Exactly **one** screen in the whole UI gives a finger any acknowledgment that a
tap registered: `climate.slint` (the v0.5.1 steppers + mode segments bind their
fill/border to `TouchArea.pressed`). **Every other interactive element** —
launcher tiles, the WIFI/BLE/MESH radio dots, the back-chevron, the reboot
button, the brightness slider knob, all nine WLED tiles, the mic-gain steppers,
the voice push-to-talk button, the page dots, the clock tap regions, the theme
swatches, the climate *list* rows, the hunt "next" button — is a static
`Rectangle` with a `TouchArea { clicked => … }` and **zero visual change on
press.** You touch it, nothing happens, then ~100–170 ms later the *result*
appears (or a whole overlay swaps in). That gap is the entire complaint.

The overhaul is therefore not a repaint — it is **rolling out the pattern
`climate.slint` already proves, to every interactive element, through a shared
component library, within the hard limit of our render budget.**

---

## 2. Hardware & render constraints (these shape every decision)

| Fact | Source | Implication |
|---|---|---|
| Board = **Waveshare ESP32-C6-Touch-AMOLED-2.06**, CO5300, **410×502**, RGB565 | vendor BSP `esp32-c6-touch-amoled-2.06/config.h` (`DISPLAY_WIDTH 410 / HEIGHT 502`); `src/drivers/co5300.rs:3` | fixed canvas, sw renderer |
| **No haptic motor.** None of the Waveshare C6 AMOLED family (1.32/1.43/1.8/2.06/2.16) declares a `VIBRATING_MOTOR_PIN`; motors exist only on `xingzhi-abs-2.0`, `m5stack-stopwatch`, `lilygo-t-display-p4`, `esp-sparkbot` | vendor BSP grep | **feedback is visual-only** — we cannot buzz the wrist |
| **No audio click.** ES8311 DAC beep path exists but is disabled pending shared-TX work; mics are on the separate ES7210 | project memory `c6-i2s-rx-mic`; board config | **feedback is visual-only** for now. If shared-TX lands later, a 1-frame click *tone* could reinforce press — noted as future, not in scope |
| **Full-frame render ≈ 90–170 ms; no partial rendering** (reverted, issue #18). Renderer line-streams two-line RGB565 strips; no framebuffer | task brief; `README.md:26`; `src/ui/slint_platform.rs:92` | effective **6–11 fps**. Continuous animation is off the table. A single element changing colour still forces a **full-frame repaint** |
| Slint **1.17**, MCU backend, `no_std`, software renderer, 4-scheme theme | `ui/slint/theme.slint` | all feedback must be expressed in `Theme.*` tokens so it works in Midnight / Paper / Amber / Violet with no per-scheme code |

### 2.1 The cost model, precisely

`TouchArea.pressed` is a reactive `bool` (Slint docs: *"Set to `true` when the
mouse is pressed over it"*). Binding a property to it — `background: ta.pressed ?
A : B` — creates a dependency: when the finger lands, `pressed` flips, the bound
property is marked dirty, and **the next full frame is drawn with the pressed
value.** That is the whole mechanism, and it is the *cheapest possible*
feedback: no extra elements, no timers, one already-scheduled repaint.

The acknowledgment therefore lands **one frame later — ≈ 90–170 ms** after touch.
Research puts the "feels instant" threshold at **~100 ms** ([NN/g / Miller
response-time rule](https://www.codestudy.net/blog/what-is-the-shortest-perceivable-application-response-delay/)).
We sit right at the edge of it and cannot do better on a full-frame software
renderer. **The design response is to make the one delayed frame unmistakable:**
a large, high-contrast state change (fill inversion, border ignite, a lit halo)
so that even a single late frame reads unambiguously as *"got it."* Subtlety is
the enemy here — a 12 % Material state-layer overlay would be invisible at our
contrast and framerate; we go bolder.

---

## 3. THE FEEDBACK STANDARD (the contract every interactive element must meet)

> **Every element a finger can touch declares three visual states, driven by
> `TouchArea.pressed`, expressed only in `Theme` tokens, delivered as a hard
> 1-frame swap (no tween):**
>
> 1. **REST** — the idle look.
> 2. **PRESSED** — a *bold* state change while the finger is down. This is the
>    acknowledgment. It must be visible from arm's length on all four schemes.
> 3. **DISABLED** — where the action can be unavailable: `opacity: 0.4` +
>    `TouchArea.enabled: false` (a disabled control must not also flash pressed).
>
> Toggles/selectors add a fourth, orthogonal **SELECTED/ACTIVE** state (a
> *persistent* fill), distinct from the *momentary* pressed state.

Three hard rules that make the standard cheap and consistent:

- **Hard swap, no `animate` on press.** A tween needs one repaint *per displayed
  step*; at 6–11 fps a 120 ms `animate` yields 0–1 intermediate frames (wasted)
  and can queue extra full repaints on a fast tap-release. Bind directly to
  `pressed` and let the single scheduled frame carry it. *(This refines the
  reference: `climate.slint` wraps its press in `animate … 120ms` — keep its
  `pressed` binding, drop the animate.)*
- **Token-only.** No literal hex in feedback states. The palette already ships
  the exact tokens the standard needs (see §4).
- **≥ 44 px hit target**, glyph/label centred inside it. We already exceed this
  (radio dots 78×64, climate steppers 72×72); codify it so nothing regresses.

---

## 4. The feedback token vocabulary (already in `theme.slint`)

The palette was built with press-feedback tokens in place — we do not need new
colours, we need to *use* them. The load-bearing four:

| Token | Palette role (comment in `theme.slint`) | Feedback use |
|---|---|---|
| `accent-bg` | *"tinted fill behind a primary/accent action"* | **pressed fill** for panels / tiles / icon halos |
| `accent` | primary brand | **pressed border / glyph ignite**; active fill |
| `on-accent` | *"near-black ink/detail on a bright fill"* | **inverted text/glyph** when a control fills with `accent` |
| `line` → | hairline border | **rest border**, becomes `accent` on press |

`warn` / `warn-bg` / `on-accent` do the same job for danger actions (reboot).
Because every scheme guarantees `accent` stays bright enough that `on-accent`
reads on it (palette contract, `theme.slint:76`), the invert recipe is safe on
all four schemes with no per-scheme logic.

---

## 5. Recipe catalog (six archetypes → exact rest/pressed/disabled)

These are the reusable shapes. Phase 0 turns each into **one shared component**
(§8); screens then compose components instead of re-deriving ternaries. Token
values shown for reference are Midnight; all are `Theme.*` so they follow the
scheme.

### Recipe A — `PressTile` (panel surfaces: launcher tiles, WLED tiles, theme swatches, hunt "next", list cards)
```
rest:     background: Theme.panel;     border: 1px Theme.line;
pressed:  background: Theme.accent-bg; border: 1px Theme.accent;   // fill lights, edge ignites
disabled: opacity: 0.4;  TouchArea.enabled: false;
```
Icon/label tint is unchanged by press (the tile "lights up under the thumb").
This is the single biggest win — the launcher is the most-touched surface.

### Recipe B — `IconButton` (glyph-only targets: BackChevron, RadioDot, clock cpu/gyro/apps, page-dots)
No panel at rest, so add a **pressed halo** — a circular `Rectangle` behind the
glyph that only appears while pressed:
```
halo:     background: ta.pressed ? Theme.accent-bg : transparent;  // Ø ≈ hit height, border-radius: 50%
glyph:    color: ta.pressed ? Theme.accent : <rest color (soft/dim)>;
```
A lit disc under the thumb + the glyph igniting to accent is unmistakable with
zero chrome at rest (keeps the minimal look).

### Recipe C — `Stepper` (± setpoint, ± mic gain)
Full invert — the boldest 1-frame delta, ideal for repeat-tap controls:
```
rest:     background: Theme.panel;   border: 2px Theme.accent;  glyph: Theme.accent;
pressed:  background: Theme.accent;  border: 2px Theme.accent;  glyph: Theme.on-accent;
disabled: opacity: 0.4;  TouchArea.enabled: false;
```
*(Reference `climate.slint` does `panel→line` + border; the full invert reads
harder and reuses `on-accent`. Standardize on invert.)*

### Recipe D — `PillButton` / `DangerButton` (reboot, primary CTAs, WLED ON)
```
accent  rest:    background: Theme.accent-bg; border: 1px Theme.accent; text: Theme.accent;
accent  pressed: background: Theme.accent;    border: 1px Theme.accent; text: Theme.on-accent;
danger  rest:    background: Theme.warn-bg;    border: 1px Theme.warn;  text: Theme.warn;
danger  pressed: background: Theme.warn;       border: 1px Theme.warn;  text: Theme.on-accent;
```

### Recipe E — `Seg` / `Toggle` (climate mode chips, RadioDot on/off)
Momentary press ≠ persistent selection:
```
inactive rest:    background: Theme.track;     border: 1px Theme.line;  text: Theme.soft;
inactive pressed: background: Theme.accent-bg;  border: 1px Theme.accent;
active (selected): background: Theme.accent;    border: 1px Theme.accent; text: Theme.on-accent;
```
RadioDot: keep the active dot = `accent`; **add** pressed halo + label→accent.

### Recipe F — `Slider` knob (brightness)
The one place a size change earns its cost — the user is already watching the
knob track a continuous drag, so grow + brighten it while the pointer is down:
```
knob rest:    background: Theme.ink;    26px
knob pressed: background: Theme.accent;  32px    // driven by the pointer-event's self.pressed
```
Track/fill unchanged. No `animate` — the drag itself provides the motion.

---

## 6. Component-by-component overhaul spec

`✗` = no feedback today · `✓` = reference (already good) · target = recipe to apply.

| # | Element | File | Today | Target |
|---|---|---|---|---|
| 1 | **Launcher `AppTile`** (×N, top surface) | `launcher.slint:148` | ✗ static panel | **Recipe A**. Also bump label `soft→ink` for legibility. Highest priority. |
| 2 | **`RadioDot`** WIFI/BLE/MESH (every screen) | `theme.slint:126` | ✗ | **Recipe B + E** (halo + label ignite; keep active dot). |
| 3 | **`BackChevron`** (every overlay) | `theme.slint:209` | ✗ | **Recipe B** (halo behind `‹`, glyph `soft→accent`). |
| 4 | **Reboot button** | `power.slint:161` | ✗ | **Recipe D danger** (invert to solid `warn` on press). |
| 5 | **Brightness slider knob** | `power.slint:139` | drag only, no knob state | **Recipe F** (knob grow+brighten while dragging). |
| 6 | **WLED tiles** (×9) | `wled.slint:14` | ✗ | **Recipe A**, label keeps its accent tint. Nine dead buttons → nine live ones. |
| 7 | **Mic-gain steppers ±** | `soundlevel.slint:148,163` | ✗ | **Recipe C**. |
| 8 | **Voice PTT** | `voice.slint:129` | state-driven only (waits for loop round-trip → multi-100 ms lag) | Add a **local `pressed` fill** so the button reacts to the finger *immediately*, before `voice-state` returns from the loop. Recipe C-style invert on the circle. |
| 9 | **Page dots** (tap-to-advance) | `shell.slint:324` | ✗ | **Recipe B** on the tap area (dots brighten/enlarge while pressed). |
| 10 | **Clock cpu/gyro/apps taps** | `clock.slint:105,116,127` | ✗ invisible regions | **Recipe B** halo on the tapped chip/region so the affordance is discoverable. |
| 11 | **Theme `SchemeTile`** swatches | `theme_overlay.slint:12` | active ring only, ✗ press | **Recipe A/E** (pressed border ignite; keep the active ring + check). |
| 12 | **Hunt "next" area** | `hunt.slint:104` | ✗ | **Recipe A**. |
| 13 | **Climate list cards** | `climate.slint:131` | ✗ (only detail steppers have feedback) | **Recipe A** on the row. |
| 14 | **Climate steppers + ModeSeg** | `climate.slint:214,235,37` | ✓ reference | **Migrate to shared `Stepper`/`Seg`; drop the `animate … 120ms`** on press (keep the binding). Consistency pass. |
| — | StatRow, PowerCell, mesh rows | display-only | — | no change (not interactive). |
| — | **fb-apps** (games/settings, embedded-graphics, non-Slint) | `src/apps/*.rs` | separate render path | Out of scope for the Slint standard, but the *principle* applies: `settings.rs` T9 keys / Connect button should draw a **pressed cell** (invert fill for one frame on tap) via `handle_tap`. Small, optional Phase 5 item; note for the fb owner. |

---

## 7. Typography & spacing tokens (codify the magic numbers)

The current UI hard-codes `font-size`/`letter-spacing`/spacing inline
everywhere. Part of "best practices" is a named scale in `Theme` so screens stop
inventing sizes. Recommended (matches what's already de-facto in use):

**Type scale** (`out property <length>` on `Theme`, or a `TypeScale` global):

| Token | px / weight / tracking | Used for |
|---|---|---|
| `t-display` | 84 / 300 | AOD & meter hero numerals |
| `t-hero` | 46–76 / 700 | setpoint, current temp, dBFS |
| `t-title` | 26 / 600 / +5 | `PageTitle` |
| `t-value` | 22 / 600 | `StatRow` values |
| `t-body` | 17 | tile labels, body |
| `t-label` | 13–14 / +2–3, `dim` | field labels, captions |
| `t-caption` | 12–13 / `faint` | honesty notes, hints |

**Spacing scale:** `4 · 8 · 12 · 16 · 22 · 30` px (already the de-facto grid —
`safe-side` 22, section gaps 12, card radius 16). Name them `sp-1…sp-6`.

**Radii:** pill `= height/2`; card `16`; swatch `18`; chip `10–12`. Name `r-card
16`, `r-chip 12`, `r-pill 999`.

**Touch target floor:** `44 px` minimum, enforced by the shared components. Our
existing 78×64 / 72×72 targets stay — bigger is fine on a 410 px panel.

---

## 8. What NOT to do at our frame budget (guardrails)

- **No `animate` on press/hover/selection.** Hard swap only (§3). Retire the
  existing `animate background/color … 120–250 ms` in `climate.slint` press +
  action-colour — at 6–11 fps they're a 1-step jump anyway.
- **No continuous / decorative motion** — no spinners, pulsing glows, breathing
  rings, progress shimmers. Each frame is a full 410×502 repaint; a "spinner"
  would stutter at ~8 fps and pin the CPU. (The voice listening-level bar is
  data-driven and only repaints when the level changes — acceptable, but do not
  add a *time-based* pulse.)
- **No `has-hover` styling** — touch panel has no hover; it will never trigger.
- **No shadows, blur, backdrop-filter, or motion-gradients** on device — the sw
  renderer can't afford them and near-black gradients band (see `shell.slint`
  backdrop comment). Flat fills + hairline borders only.
- **No overlay fade-in scrims** — overlays hard-cut in; that's correct here. A
  200 ms cross-fade (climate list↔detail) is the *maximum* motion we allow, and
  only because it's a large, self-explanatory transition; do not add more.
- **Don't shrink hit targets** to make room for new pressed chrome — the halo
  lives *inside* the existing target.
- **Keep the 260 ms page-slide** (`shell.slint` `animate x`) — it is the one
  earned animation (large positional move, reads as intentional even at low fps)
  and predates this work. Don't touch it.

---

## 9. AOD (always-on-display) note

Current AOD (`shell.slint:443`) is a dim time + date on true black — already
close to correct. Two cheap best-practice additions ([AMOLED burn-in
guidance](https://www.androidauthority.com/screen-burn-in-801760/)):

- **Pixel-shift**: nudge the AOD time block by a few px on a slow cycle (e.g.
  ±4 px every minute, keyed off the existing minute tick) to spread wear. One
  extra binding, no new render cost (AOD already repaints on the minute).
- **Keep it dim & sparse** (it is). Do **not** add an accent element to AOD —
  bright static pixels are exactly what burns in. AOD is the one screen that
  intentionally has *no* touch feedback (it wakes on tap; that's its whole job).

---

## 10. Phased implementation order (for Luna)

**Phase 0 — Foundation (blocking; do first).**
Create `ui/slint/controls.slint` with the shared components implementing §5 once:
`PressTile`, `IconButton`, `Stepper`, `PillButton` (+ danger variant), `Seg`,
`SliderControl`. Add the §7 type/spacing tokens to `theme.slint`. **Everything
else composes these** — this is what makes it "a system" not 40 inline ternaries,
and it's the difference between an overhaul and a patch. Verify one component
end-to-end on-glass before fanning out.

**Phase 1 — Highest-traffic surfaces (biggest perceived win).**
Launcher tiles (#1), RadioDots (#2), BackChevron (#3), page dots (#9). These are
touched on every session and every screen — fixing them alone will make the
whole watch feel responsive.

**Phase 2 — App controls.**
WLED grid (#6), mic-gain steppers (#7), reboot (#4), brightness knob (#5), theme
swatches (#11), hunt next (#12).

**Phase 3 — Round-trip-lag killers.**
Voice PTT local pressed state (#8) and climate list cards (#13) — these have the
*worst* perceived lag today because feedback waits on a loop/network round-trip.

**Phase 4 — Align the reference.**
Migrate `climate.slint` (#14) onto the shared components; drop its press
`animate`s. Now the whole UI is one consistent system.

**Phase 5 — Polish (optional).**
Typography/spacing token sweep across remaining literals, AOD pixel-shift (§9),
corner-safe re-audit of any new halos near the arc, fb-app pressed cells.

Each phase is independently shippable and on-glass verifiable (device = ttyACM3;
watch-face → launcher → each overlay). Because feedback is a per-element visual
swap, phases don't collide — Luna can land them as small PRs.

---

## Sources

- Slint TouchArea reference — `pressed` / pointer-event / feedback binding:
  <https://docs.slint.dev/latest/docs/slint/reference/gestures/toucharea/>
- Slint MCU software-renderer performance (animation cost on slow MCUs, line
  buffers, no-GPU): <https://slint.dev/blog/porting-slint-to-microcontrollers>,
  <https://github.com/slint-ui/slint/discussions/5649>,
  <https://deepwiki.com/slint-ui/slint/3.1-software-renderer>
- 100 ms perceived-instant threshold (Miller / NN/g):
  <https://www.codestudy.net/blog/what-is-the-shortest-perceivable-application-response-delay/>,
  <https://madecurious.com/articles/inp-and-the-illusion-of-speed/>
- Wear OS Material 3 Expressive — pressed/shape-morph, "glanceable buttons",
  responsive feedback: <https://android-developers.googleblog.com/2025/08/introducing-material-3-expressive-for-wear-os.html>,
  <https://developer.android.com/jetpack/androidx/releases/wear-compose-m3>
- Material state-layer / ripple pressed model (why we go bolder than 12 %):
  <https://m2.material.io/go/design-states/>, <https://m2.material.io/develop/ios/supporting/ripple>
- watchOS responsiveness + haptic acknowledgment (context for "visual-only"):
  <https://moldstud.com/articles/p-how-to-design-user-friendly-interfaces-for-apple-watch-apps-essential-tips-and-best-practices>,
  <https://www.sneakycrab.com/blog/2015/6/22/haptic-feedback-with-the-taptic-engine-in-watchkit-and-watchos-2-wkinterfacedevice-and-wkhaptic>
- Touch-target minimums on wearables (44 px):
  <https://www.nngroup.com/articles/touch-target-size/>,
  <https://developer.android.com/training/wearables/accessibility>
- AMOLED AOD burn-in mitigation (pixel-shift, dim):
  <https://www.androidauthority.com/screen-burn-in-801760/>
- Board BSP: vendor `scratch/vendor-xiaozhi/main/boards/waveshare/esp32-c6-touch-amoled-2.06/config.h`
