# Slint UI Migration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the embedded-graphics watchface shell (5 pages + launcher + AOD) in the main firmware with Slint, keeping games/Settings on embedded-graphics via an on-demand framebuffer handover.

**Architecture:** The main loop keeps ownership of all peripherals and gains two render modes: shell mode (`AppState::Watchface | Launcher`) renders a Slint scene line-by-line to the CO5300 with no framebuffer; app mode allocates the 202KB RGB332 framebuffer on demand and runs the existing eg apps unchanged. UI state flows loop→Slint via property setters, Slint→loop via callback request cells.

**Tech Stack:** Rust no_std (esp-hal 1.1, embassy, esp-rtos), Slint 1.17 software renderer (`EmbedForSoftwareRenderer`), CO5300 QSPI DMA driver (unchanged).

**Spec:** `docs/superpowers/specs/2026-07-19-slint-ui-migration-design.md`

**Branch:** `feat/slint-shell` (create from `main` before Task 1; return to `main` when done per CLAUDE.md).

**Testing convention (overrides TDD steps):** This repo has no test framework and per JP's global CLAUDE.md we don't add one unprompted. Every task verifies with `cargo check -q` (must be silent), and stage gates build both bins release. Hardware flashing (`cargo run --release --bin esp32c6-watch`) requires the watch on USB — check `ls /dev/ttyACM*` first; if absent, mark the step HW-GATE-HELD and continue.

`cargo` is at `~/.cargo/bin/cargo` (not on PATH in fresh shells). `.cargo/config.toml` must exist (copy from `config.example.toml` if missing).

For visual iteration on `.slint` files without hardware: `slint-viewer ui/slint/shell.slint` (install once with `cargo install slint-viewer` if absent) — properties show their defaults; it validates layout, not data flow.

---

### Task 1: Shared Slint platform module

Hoist `EspPlatform` + `TwoLineFlusher` out of the demo bin so both binaries can use them.

**Files:**
- Create: `src/ui/slint_platform.rs`
- Modify: `src/ui/mod.rs`
- Modify: `src/bin/slint_demo.rs`

- [ ] **Step 1: Create `src/ui/slint_platform.rs`**

Move the platform + flusher code verbatim from `src/bin/slint_demo.rs:84-172`, adjusted to module paths:

```rust
// Slint platform glue for the CO5300 AMOLED: embassy-clocked Platform and a
// line-streaming flusher. No framebuffer — the software renderer paints into
// a 2-line RGB565 strip (410 x 2 x 2 B) streamed to the panel's GRAM.
// Two lines per flush because the CO5300 requires a min 2x2 address window.

extern crate alloc;

use alloc::boxed::Box;
use alloc::rc::Rc;

use slint::platform::software_renderer::{
    MinimalSoftwareWindow, RepaintBufferType, Rgb565Pixel,
};
use slint::platform::{Platform, WindowAdapter};

use crate::board;
use crate::drivers::co5300::Co5300Display;

pub const WIDTH: usize = board::LCD_WIDTH as usize; // 410
pub const HEIGHT: usize = board::LCD_HEIGHT as usize; // 502

pub struct EspPlatform {
    window: Rc<MinimalSoftwareWindow>,
}

impl Platform for EspPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.window.clone())
    }

    fn duration_since_start(&self) -> core::time::Duration {
        core::time::Duration::from_micros(embassy_time::Instant::now().as_micros())
    }
}

/// Create the window, register the platform. Call exactly once per boot —
/// `slint::platform::set_platform` panics on a second call.
pub fn init_platform() -> Rc<MinimalSoftwareWindow> {
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    window.set_size(slint::PhysicalSize::new(WIDTH as u32, HEIGHT as u32));
    slint::platform::set_platform(Box::new(EspPlatform {
        window: window.clone(),
    }))
    .expect("set_platform failed");
    window
}

/// LineBufferProvider that batches two rendered lines per panel write.
pub struct TwoLineFlusher<'a, 'd> {
    pub display: &'a mut Co5300Display<'d>,
    /// 2 x WIDTH pixels: line A in the first half, line B in the second.
    pub buf: &'a mut [Rgb565Pixel],
    /// Raw u16 staging for the QSPI bus.
    pub scratch: &'a mut [u16],
    /// y of the line waiting in the first half of `buf`, if any.
    pub pending: Option<usize>,
}

impl TwoLineFlusher<'_, '_> {
    fn flush_two(&mut self, y: usize) {
        for (dst, src) in self.scratch.iter_mut().zip(self.buf.iter()) {
            *dst = src.0;
        }
        self.display.set_addr_window(0, y as u16, WIDTH as u16, 2);
        self.display.bus_mut().write_pixels(self.scratch);
    }

    pub fn flush_pending(&mut self) {
        if let Some(y) = self.pending.take() {
            let (first, second) = self.buf.split_at_mut(WIDTH);
            second.copy_from_slice(first);
            let y = y.min(HEIGHT - 2);
            self.flush_two(y);
        }
    }
}

impl slint::platform::software_renderer::LineBufferProvider for &mut TwoLineFlusher<'_, '_> {
    type TargetPixel = Rgb565Pixel;

    fn process_line(
        &mut self,
        line: usize,
        range: core::ops::Range<usize>,
        render_fn: impl FnOnce(&mut [Self::TargetPixel]),
    ) {
        let second_half = match self.pending {
            Some(p) if line == p + 1 => true,
            Some(_) => {
                self.flush_pending();
                false
            }
            None => false,
        };

        let offset = if second_half { WIDTH } else { 0 };
        let dst = &mut self.buf[offset..offset + WIDTH];
        if range.start != 0 || range.end != WIDTH {
            dst.fill(Rgb565Pixel(0));
        }
        render_fn(&mut dst[range]);

        if second_half {
            let y = self.pending.take().unwrap();
            self.flush_two(y);
        } else {
            self.pending = Some(line);
        }
    }
}
```

- [ ] **Step 2: Register the module in `src/ui/mod.rs`**

```rust
pub mod watchface;
pub mod pages;
pub mod launcher;
pub mod t9_keyboard;
pub mod power_page;
pub mod slint_platform;
```

Note: `main.rs` does not yet reference Slint; `slint_platform` compiles in the main bin because the `slint` crate is already a workspace dependency.

- [ ] **Step 3: Rewire `src/bin/slint_demo.rs` to the shared module**

Add `pub mod slint_platform;` inside a `#[path = "../ui"] mod ui { ... }` block mirroring how `drivers`/`peripherals` are included (`src/bin/slint_demo.rs:25-38`):

```rust
#[path = "../ui"]
#[allow(dead_code)]
mod ui {
    pub mod slint_platform;
}
```

Delete the local `EspPlatform` and `TwoLineFlusher` definitions (`src/bin/slint_demo.rs:84-172`), and replace their uses:
- `use crate::ui::slint_platform::{init_platform, TwoLineFlusher, WIDTH, HEIGHT};`
- Delete the local `const WIDTH` / `const HEIGHT` (lines 81-82).
- Replace the window/platform setup block (lines 283-288) with `let window = init_platform();`
- The render closure keeps constructing `TwoLineFlusher { display: &mut display, buf: &mut line_buf, scratch: &mut scratch, pending: None }` — now the imported type (fields are `pub`).

- [ ] **Step 4: Verify both bins compile**

Run: `PATH="$HOME/.cargo/bin:$PATH" cargo check -q`
Expected: silent success (checks both bins).

- [ ] **Step 5: Commit**

```bash
git add src/ui/slint_platform.rs src/ui/mod.rs src/bin/slint_demo.rs
git commit -m "refactor(ui): hoist Slint platform + line flusher into shared module"
```

---

### Task 2: Slint shell skeleton — theme, chrome, clock page

Create the `ui/slint/` source tree with the shell root, port the demo's clock page + chrome, add the new interaction callbacks. Switch `build.rs` to compile the shell; the demo bin switches to `WatchShell` (this retires `src/bin/watchface.slint`).

**Files:**
- Create: `ui/slint/theme.slint`
- Create: `ui/slint/clock.slint`
- Create: `ui/slint/shell.slint`
- Modify: `build.rs:8-11`
- Modify: `src/bin/slint_demo.rs`
- Delete: `src/bin/watchface.slint`

- [ ] **Step 1: Create `ui/slint/theme.slint`**

```slint
// Shared palette + small components for the watch shell.
export global Theme {
    out property <color> ink: #f2f5ff;
    out property <color> soft: #9fb4d8;
    out property <color> dim: #55617a;
    out property <color> faint: #3c4761;
    out property <color> accent: #35e0b0;
    out property <color> accent2: #3a7bd5;
    out property <color> panel: #101728;
    out property <color> line: #2a3550;
    out property <color> track: #16202f;
    out property <color> warn: #ff5566;
    out property <color> off-dot: #223048;
}

// One radio indicator: dot + label, tappable.
export component RadioDot {
    in property <string> label;
    in property <bool> active;
    callback tapped();
    width: 54px;
    height: 30px;

    Rectangle {
        x: 0;
        y: (parent.height - self.height) / 2;
        width: 8px; height: 8px; border-radius: 4px;
        background: root.active ? Theme.accent : Theme.off-dot;
    }
    Text {
        x: 14px;
        y: (parent.height - self.height) / 2;
        text: root.label;
        color: root.active ? Theme.soft : Theme.faint;
        font-size: 13px;
        letter-spacing: 1px;
    }
    TouchArea { clicked => { root.tapped(); } }
}

// One label/value line for stats-style pages.
export component StatRow {
    in property <string> label;
    in property <string> value;
    in property <color> value-color: Theme.ink;
    width: 330px;
    height: 48px;

    Text {
        x: 0;
        y: (parent.height - self.height) / 2;
        text: root.label;
        color: Theme.dim;
        font-size: 16px;
        letter-spacing: 2px;
    }
    Text {
        x: parent.width - self.width;
        y: (parent.height - self.height) / 2;
        text: root.value;
        color: root.value-color;
        font-size: 22px;
        font-weight: 600;
    }
}

// Page title, centered at the top.
export component PageTitle {
    in property <string> title;
    width: 100%;
    Text {
        x: (parent.width - self.width) / 2;
        y: 56px;
        text: root.title;
        color: Theme.soft;
        font-size: 26px;
        font-weight: 600;
        letter-spacing: 5px;
    }
}
```

- [ ] **Step 2: Create `ui/slint/clock.slint`**

Port of the demo's page 0 plus the eg clock page's interactive chips (CPU cycle, gyro toggle, apps) and info line (steps + weather):

```slint
import { Theme } from "theme.slint";

export component ClockPage {
    in property <string> time-text;
    in property <string> seconds-text;
    in property <string> date-text;
    in property <float> minute-progress;
    in property <int> steps;
    in property <string> weather-text; // "" when no data
    in property <string> cpu-text;     // e.g. "160 MHz"
    in property <bool> gyro-on;
    // parallax offsets (stage-4 polish feeds these; 0 until then)
    in property <length> par-x: 0px;
    in property <length> par-y: 0px;
    callback cpu-tap();
    callback gyro-tap();
    callback apps-tap();

    // Top date pill
    Rectangle {
        x: (parent.width - self.width) / 2 + root.par-x / 4;
        y: 56px + root.par-y / 4;
        width: date.width + 44px;
        height: 42px;
        border-radius: 21px;
        border-width: 1px;
        border-color: Theme.line;
        background: Theme.panel;
        date := Text {
            x: 22px;
            y: (parent.height - self.height) / 2;
            text: root.date-text;
            color: Theme.soft;
            font-size: 19px;
            letter-spacing: 2px;
        }
    }

    time := Text {
        x: (parent.width - self.width) / 2 + root.par-x;
        y: 138px + root.par-y;
        text: root.time-text;
        color: Theme.ink;
        font-size: 108px;
        font-weight: 700;
    }
    secs := Text {
        x: (parent.width - self.width) / 2 + root.par-x / 2;
        y: 282px + root.par-y / 2;
        text: root.seconds-text;
        color: Theme.accent;
        font-size: 40px;
        font-weight: 600;
    }

    // Minute progress bar
    track := Rectangle {
        x: (parent.width - self.width) / 2;
        y: 348px;
        width: 300px; height: 10px; border-radius: 5px;
        background: Theme.track;
        Rectangle {
            x: 0; y: 0;
            width: max(10px, track.width * root.minute-progress);
            height: parent.height;
            border-radius: 5px;
            background: @linear-gradient(90deg, #3a7bd5 0%, #35e0b0 100%);
            animate width { duration: 350ms; easing: ease-out; }
        }
    }

    // Steps + weather line
    Text {
        x: (parent.width - self.width) / 2;
        y: 376px;
        text: root.weather-text == ""
            ? "\{root.steps} STEPS"
            : "\{root.steps} STEPS  \u{00b7}  \{root.weather-text}";
        color: Theme.dim;
        font-size: 18px;
        letter-spacing: 2px;
    }

    // Interactive chips row: CPU · GYRO · APPS
    HorizontalLayout {
        x: (parent.width - 330px) / 2;
        y: 408px;
        width: 330px;
        height: 36px;
        spacing: 10px;
        Rectangle {
            border-radius: 18px; border-width: 1px; border-color: Theme.line;
            background: Theme.panel;
            Text {
                x: (parent.width - self.width) / 2;
                y: (parent.height - self.height) / 2;
                text: root.cpu-text; color: Theme.soft; font-size: 15px;
            }
            TouchArea { clicked => { root.cpu-tap(); } }
        }
        Rectangle {
            border-radius: 18px; border-width: 1px; border-color: Theme.line;
            background: root.gyro-on ? Theme.track : Theme.panel;
            Text {
                x: (parent.width - self.width) / 2;
                y: (parent.height - self.height) / 2;
                text: "GYRO"; color: root.gyro-on ? Theme.accent : Theme.soft;
                font-size: 15px; letter-spacing: 1px;
            }
            TouchArea { clicked => { root.gyro-tap(); } }
        }
        Rectangle {
            border-radius: 18px; border-width: 1px; border-color: Theme.line;
            background: Theme.panel;
            Text {
                x: (parent.width - self.width) / 2;
                y: (parent.height - self.height) / 2;
                text: "APPS"; color: Theme.accent; font-size: 15px;
                letter-spacing: 1px;
            }
            TouchArea { clicked => { root.apps-tap(); } }
        }
    }
}
```

- [ ] **Step 3: Create `ui/slint/shell.slint`**

Root window: 5-page carousel (clock is real; the other four are placeholder rectangles replaced in Tasks 4-7), persistent chrome, page dots, AOD overlay.

```slint
import { Theme, RadioDot, StatRow, PageTitle } from "theme.slint";
import { ClockPage } from "clock.slint";

export struct PeerRow {
    name: string,
    rssi: string,
    age: string,
}

export component WatchShell inherits Window {
    width: 410px;
    height: 502px;
    background: #000000;

    // --- clock ---
    in property <string> time-text: "--:--";
    in property <string> seconds-text: "--";
    in property <string> date-text: "SYNCING RTC";
    in property <float> minute-progress: 0.0;
    in property <int> steps: 0;
    in property <string> weather-text: "";
    in property <string> cpu-text: "160 MHz";
    in property <bool> gyro-on: false;
    in property <length> par-x: 0px;
    in property <length> par-y: 0px;

    // --- battery / radios (chrome) ---
    in property <int> battery-percent: -1;
    in property <bool> charging: false;
    in property <bool> wifi-on: false;
    in property <bool> ble-on: false;
    in property <int> mesh-peers: 0;

    // --- paging / modes ---
    in-out property <int> current-page: 0; // 0 clock 1 sensors 2 system 3 power 4 mesh
    in-out property <bool> launcher-open: false;
    in property <bool> aod: false;
    in property <string> toast-text: "";

    // --- callbacks (drained by the Rust loop via request cells) ---
    callback brightness-changed(float);
    callback wifi-tap();
    callback ble-tap();
    callback cpu-tap();
    callback gyro-tap();
    callback reboot-tap();
    callback launch-app(int); // launcher item index

    in-out property <float> brightness: 0.8;

    // Ambient AMOLED-friendly gradient (mostly true black)
    Rectangle {
        width: 100%; height: 100%;
        background: @linear-gradient(155deg, #140b2e 0%, #000000 40%, #000000 72%, #002733 100%);
    }

    pages := Rectangle {
        width: 100%; height: 100%;
        clip: true;

        ClockPage {
            x: (0 - root.current-page) * pages.width;
            width: pages.width; height: pages.height;
            animate x { duration: 260ms; easing: ease-out; }
            time-text: root.time-text;
            seconds-text: root.seconds-text;
            date-text: root.date-text;
            minute-progress: root.minute-progress;
            steps: root.steps;
            weather-text: root.weather-text;
            cpu-text: root.cpu-text;
            gyro-on: root.gyro-on;
            par-x: root.par-x;
            par-y: root.par-y;
            cpu-tap => { root.cpu-tap(); }
            gyro-tap => { root.gyro-tap(); }
            apps-tap => { root.launcher-open = true; }
        }

        // Placeholder pages 1-4, replaced by Tasks 4-7.
        for title[idx] in ["SENSORS", "SYSTEM", "POWER", "MESH"]: Rectangle {
            x: (idx + 1 - root.current-page) * pages.width;
            width: pages.width; height: pages.height;
            animate x { duration: 260ms; easing: ease-out; }
            PageTitle { title: title; }
        }
    }

    // ==================== persistent chrome ====================
    RadioDot { x: 22px;  y: 14px; label: "WIFI"; active: root.wifi-on;
               tapped => { root.wifi-tap(); } }
    RadioDot { x: 84px;  y: 14px; label: "BLE";  active: root.ble-on;
               tapped => { root.ble-tap(); } }
    RadioDot {
        x: 138px; y: 14px;
        label: root.mesh-peers > 0 ? "MESH \{root.mesh-peers}" : "MESH";
        active: root.mesh-peers > 0;
    }

    // Battery pill (top-right)
    batt-pill := Rectangle {
        x: parent.width - self.width - 20px;
        y: 12px;
        width: batt-text.width + 62px;
        height: 30px;
        border-radius: 15px;
        border-width: 1px;
        border-color: Theme.line;
        background: Theme.panel;
        batt-body := Rectangle {
            x: 12px;
            y: (parent.height - self.height) / 2;
            width: 26px; height: 13px; border-radius: 3px;
            border-width: 1px;
            border-color: root.charging ? Theme.accent : Theme.soft;
            Rectangle {
                x: 2px; y: 2px;
                width: (batt-body.width - 4px)
                    * (clamp(root.battery-percent, 0, 100) / 100.0);
                height: parent.height - 4px;
                border-radius: 1px;
                background: root.charging ? Theme.accent
                    : (root.battery-percent >= 0 && root.battery-percent < 20
                        ? Theme.warn : Theme.soft);
            }
        }
        Rectangle {
            x: 38px;
            y: (parent.height - self.height) / 2;
            width: 3px; height: 7px;
            background: root.charging ? Theme.accent : Theme.soft;
        }
        batt-text := Text {
            x: 48px;
            y: (parent.height - self.height) / 2;
            text: (root.charging ? "+" : "")
                + (root.battery-percent < 0 ? "--" : "\{root.battery-percent}%");
            color: root.charging ? Theme.accent : Theme.soft;
            font-size: 15px;
        }
    }

    // Page dots: 5 pages, tap advances.
    dots := TouchArea {
        x: (parent.width - self.width) / 2;
        y: 456px;
        width: 160px;
        height: 40px;
        clicked => { root.current-page = mod(root.current-page + 1, 5); }
        HorizontalLayout {
            x: (parent.width - 5 * 10px - 4 * 12px) / 2;
            y: (parent.height - 10px) / 2;
            spacing: 12px;
            for i in 5: Rectangle {
                width: 10px; height: 10px; border-radius: 5px;
                background: root.current-page == i ? Theme.accent : Theme.line;
            }
        }
    }

    // Toast (RAM-busy etc.): visible while toast-text != ""
    if root.toast-text != "": Rectangle {
        x: (parent.width - self.width) / 2;
        y: 420px;
        width: min(360px, toast-label.width + 40px);
        height: 44px;
        border-radius: 22px;
        background: #1a2233ee;
        border-width: 1px; border-color: Theme.line;
        toast-label := Text {
            x: (parent.width - self.width) / 2;
            y: (parent.height - self.height) / 2;
            text: root.toast-text; color: Theme.ink; font-size: 16px;
        }
    }

    // ==================== AOD overlay ====================
    if root.aod: Rectangle {
        width: 100%; height: 100%;
        background: #000000;
        Text {
            x: (parent.width - self.width) / 2;
            y: 180px;
            text: root.time-text;
            color: #6b7690;
            font-size: 84px;
            font-weight: 300;
        }
        Text {
            x: (parent.width - self.width) / 2;
            y: 300px;
            text: root.date-text;
            color: #3c4761;
            font-size: 17px;
            letter-spacing: 2px;
        }
    }
}
```

- [ ] **Step 4: Point `build.rs` at the shell**

Replace `build.rs:8-11` with:

```rust
    let slint_config = slint_build::CompilerConfiguration::new()
        .embed_resources(slint_build::EmbedResourcesKind::EmbedForSoftwareRenderer);
    slint_build::compile_with_config("ui/slint/shell.slint", slint_config)
        .expect("failed to compile ui/slint/shell.slint");
```

- [ ] **Step 5: Switch the demo bin to `WatchShell` and delete `src/bin/watchface.slint`**

In `src/bin/slint_demo.rs`:
- `let ui = WatchFace::new()...` → `let ui = WatchShell::new().expect("failed to create WatchShell");`
- `ui.set_steps(0);` stays (property exists on the shell).
- Swipe handling: page count is now 5 and up-swipe opens the launcher property; replace the swipe match (lines 344-356) with:

```rust
                if let Some(sw) = swipe {
                    let on_slider =
                        ui.get_current_page() == 3 && SLIDER_BAND.contains(&sw.start_y);
                    if !on_slider {
                        match sw.direction {
                            SwipeDirection::Left => {
                                ui.set_current_page((ui.get_current_page() + 1).min(4))
                            }
                            SwipeDirection::Right => {
                                if ui.get_launcher_open() {
                                    ui.set_launcher_open(false);
                                } else {
                                    ui.set_current_page((ui.get_current_page() - 1).max(0))
                                }
                            }
                            SwipeDirection::Up => ui.set_launcher_open(true),
                            _ => {}
                        }
                    }
                }
```

`rm src/bin/watchface.slint`

- [ ] **Step 6: Verify**

Run: `PATH="$HOME/.cargo/bin:$PATH" cargo check -q`
Expected: silent success. (slint-build failures surface here as build-script errors with .slint line numbers.)

- [ ] **Step 7: Commit**

```bash
git add ui/slint/ build.rs src/bin/slint_demo.rs
git rm src/bin/watchface.slint
git commit -m "feat(ui): Slint shell skeleton — theme, chrome, clock page, 5-page carousel"
```

---

### Task 3: `ShellUi` Rust wrapper (requests, property push, render)

One focused module so `main.rs` never touches Slint types directly.

**Files:**
- Create: `src/ui/slint_shell.rs`
- Modify: `src/ui/mod.rs`
- Modify: `src/bin/slint_demo.rs`

- [ ] **Step 1: Create `src/ui/slint_shell.rs`**

```rust
// Rust-side wrapper around the WatchShell Slint component: owns the window,
// the render strip, and the callback→loop request cells. main.rs talks to
// this module only; no Slint types cross its boundary except in render().

extern crate alloc;

use alloc::rc::Rc;
use alloc::vec::Vec;
use core::cell::Cell;

use slint::platform::software_renderer::{MinimalSoftwareWindow, Rgb565Pixel};
use slint::platform::{PointerEventButton, WindowEvent};
use slint::{ComponentHandle, SharedString, VecModel};

use crate::apps::AppState;
use crate::drivers::co5300::Co5300Display;
use crate::net::names;
use crate::net::smol_mesh::PeerView;
use crate::peripherals::rtc::DateTime;
use crate::peripherals::touch::{SwipeDirection, TouchPoint};
use crate::ui::slint_platform::{init_platform, TwoLineFlusher, WIDTH};

slint::include_modules!(); // WatchShell, PeerRow

const WEEKDAYS: [&str; 7] = ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];
const MONTHS: [&str; 12] = [
    "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
];

/// Map the UI slider fraction (0.0..1.0) onto the CO5300 brightness range,
/// with a floor so the slider can never black the panel out completely.
const BRIGHTNESS_MIN: u8 = 0x10;
pub fn brightness_raw(frac: f32) -> u8 {
    let frac = frac.clamp(0.0, 1.0);
    BRIGHTNESS_MIN + (frac * (0xFF - BRIGHTNESS_MIN) as f32) as u8
}

/// y-band of the brightness slider on the power page: horizontal swipes
/// starting here are slider drags, not page switches.
pub const SLIDER_BAND: core::ops::RangeInclusive<u16> = 330..=430;

/// Launcher item order — MUST match the `for` list in ui/slint/launcher.slint.
pub const LAUNCHER_APPS: [AppState; 7] = [
    AppState::Snake,
    AppState::WorldSnake,
    AppState::Game2048,
    AppState::Tetris,
    AppState::Flappy,
    AppState::Maze,
    AppState::Settings,
];

#[derive(Default)]
pub struct ShellRequests {
    pub brightness: Cell<Option<u8>>, // raw CO5300 value
    pub launch: Cell<Option<AppState>>,
    pub wifi_toggle: Cell<bool>,
    pub ble_toggle: Cell<bool>,
    pub cpu_cycle: Cell<bool>,
    pub gyro_toggle: Cell<bool>,
    pub reboot: Cell<bool>,
}

pub struct ShellUi {
    window: Rc<MinimalSoftwareWindow>,
    ui: WatchShell,
    pub req: Rc<ShellRequests>,
    line_buf: Vec<Rgb565Pixel>,
    scratch: Vec<u16>,
    touch_down: bool,
    last_pos: slint::LogicalPosition,
    last_second: u8,
}

impl ShellUi {
    /// Call exactly once per boot (registers the Slint platform).
    pub fn new() -> Self {
        let window = init_platform();
        let ui = WatchShell::new().expect("failed to create WatchShell");
        let req = Rc::new(ShellRequests::default());

        {
            let r = req.clone();
            ui.on_brightness_changed(move |frac| r.brightness.set(Some(brightness_raw(frac))));
        }
        {
            let r = req.clone();
            ui.on_wifi_tap(move || r.wifi_toggle.set(true));
        }
        {
            let r = req.clone();
            ui.on_ble_tap(move || r.ble_toggle.set(true));
        }
        {
            let r = req.clone();
            ui.on_cpu_tap(move || r.cpu_cycle.set(true));
        }
        {
            let r = req.clone();
            ui.on_gyro_tap(move || r.gyro_toggle.set(true));
        }
        {
            let r = req.clone();
            ui.on_reboot_tap(move || r.reboot.set(true));
        }
        {
            let r = req.clone();
            ui.on_launch_app(move |idx| {
                if let Some(app) = LAUNCHER_APPS.get(idx as usize) {
                    r.launch.set(Some(*app));
                }
            });
        }

        ui.show().expect("show failed");

        Self {
            window,
            ui,
            req,
            line_buf: alloc::vec![Rgb565Pixel(0); WIDTH * 2],
            scratch: alloc::vec![0u16; WIDTH * 2],
            touch_down: false,
            last_pos: slint::LogicalPosition::new(0.0, 0.0),
            last_second: 0xFF,
        }
    }

    // === input ===

    /// Feed one iteration's touch sample. `point` is Some while a finger is
    /// down (synthesizes press/move); None after it lifts (synthesizes
    /// release). Swipes drive page/launcher navigation.
    pub fn handle_touch(&mut self, point: Option<TouchPoint>, swipe: Option<SwipeDirection>,
                        swipe_start_y: u16) {
        if let Some(tp) = point {
            let pos = slint::LogicalPosition::new(tp.x as f32, tp.y as f32);
            let event = if self.touch_down {
                WindowEvent::PointerMoved { position: pos }
            } else {
                WindowEvent::PointerPressed { position: pos, button: PointerEventButton::Left }
            };
            self.touch_down = true;
            self.last_pos = pos;
            let _ = self.window.window().try_dispatch_event(event);
        } else if self.touch_down {
            self.touch_down = false;
            let _ = self.window.window().try_dispatch_event(WindowEvent::PointerReleased {
                position: self.last_pos,
                button: PointerEventButton::Left,
            });
        }

        if let Some(direction) = swipe {
            let on_slider =
                self.ui.get_current_page() == 3 && SLIDER_BAND.contains(&swipe_start_y);
            if on_slider {
                return;
            }
            if self.ui.get_launcher_open() {
                if direction == SwipeDirection::Right {
                    self.ui.set_launcher_open(false);
                }
                return;
            }
            match direction {
                SwipeDirection::Left => {
                    self.ui.set_current_page((self.ui.get_current_page() + 1).rem_euclid(5))
                }
                SwipeDirection::Right => {
                    self.ui.set_current_page((self.ui.get_current_page() + 4).rem_euclid(5))
                }
                SwipeDirection::Up if self.ui.get_current_page() == 0 => {
                    self.ui.set_launcher_open(true)
                }
                _ => {}
            }
        }
    }

    pub fn touch_is_down(&self) -> bool {
        self.touch_down
    }

    // === property push (call only when the source value changed) ===

    /// Returns true when the second ticked (caller may gate 1Hz work on it).
    pub fn set_time(&mut self, dt: &DateTime) -> bool {
        if dt.seconds == self.last_second {
            return false;
        }
        self.last_second = dt.seconds;
        self.ui.set_time_text(slint::format!("{:02}:{:02}", dt.hours, dt.minutes));
        self.ui.set_seconds_text(slint::format!("{:02}", dt.seconds));
        let weekday = WEEKDAYS[(dt.weekday % 7) as usize];
        let month = MONTHS[(dt.month.clamp(1, 12) - 1) as usize];
        self.ui.set_date_text(slint::format!(
            "{} {:02} {} 20{:02}", weekday, dt.day, month, dt.year
        ));
        self.ui.set_minute_progress(dt.seconds as f32 / 59.0);
        true
    }

    pub fn set_battery(&self, pct: u8, mv: u16, charging: bool) {
        self.ui.set_battery_percent(pct.min(100) as i32);
        self.ui.set_charging(charging);
        self.ui.set_battery_mv(mv as i32);
    }

    pub fn set_radios(&self, wifi: bool, ble: bool, mesh_peers: u8) {
        self.ui.set_wifi_on(wifi);
        self.ui.set_ble_on(ble);
        self.ui.set_mesh_peers(mesh_peers as i32);
    }

    pub fn set_steps(&self, steps: u32) {
        self.ui.set_steps(steps as i32);
    }

    pub fn set_cpu_mhz(&self, mhz: u16) {
        self.ui.set_cpu_text(slint::format!("{} MHz", mhz));
    }

    pub fn set_gyro(&self, on: bool) {
        self.ui.set_gyro_on(on);
    }

    pub fn set_weather(&self, temp_f: Option<i16>, code: u8) {
        match temp_f {
            Some(t) => self
                .ui
                .set_weather_text(slint::format!("{}\u{00b0}F {}", t, weather_label(code))),
            None => self.ui.set_weather_text(SharedString::new()),
        }
    }

    pub fn set_brightness_frac(&self, raw: u8) {
        self.ui
            .set_brightness((raw.saturating_sub(BRIGHTNESS_MIN)) as f32
                / (0xFF - BRIGHTNESS_MIN) as f32);
    }

    pub fn set_aod(&self, on: bool) {
        self.ui.set_aod(on);
    }

    pub fn set_toast(&self, text: &str) {
        self.ui.set_toast_text(SharedString::from(text));
    }

    pub fn set_launcher_open(&self, open: bool) {
        self.ui.set_launcher_open(open);
    }

    pub fn launcher_open(&self) -> bool {
        self.ui.get_launcher_open()
    }

    pub fn page(&self) -> i32 {
        self.ui.get_current_page()
    }

    pub fn set_mesh_rows(&self, our_id: u8, rows: &[PeerView], now_ms: u64) {
        let (first, last) = names::name_for_id(our_id);
        self.ui
            .set_mesh_self_text(slint::format!("#{:03} {} {}", our_id, first, last));
        let model: Vec<PeerRow> = rows
            .iter()
            .map(|p| {
                let name = match p.id {
                    Some(id) => {
                        let (f, l) = names::name_for_id(id);
                        slint::format!("#{:03} {} {}", id, f, l)
                    }
                    None => slint::format!(
                        "{:02x}:{:02x}:{:02x}", p.mac[3], p.mac[4], p.mac[5]
                    ),
                };
                PeerRow {
                    name,
                    rssi: match p.rssi_dbm {
                        Some(r) => slint::format!("{} dBm", r),
                        None => SharedString::new(),
                    },
                    age: slint::format!("{}s", now_ms.saturating_sub(p.age_ms) / 1000),
                }
            })
            .collect();
        self.ui.set_mesh_rows(slint::ModelRc::new(VecModel::from(model)));
    }

    // === render ===

    pub fn has_active_animations(&self) -> bool {
        self.window.has_active_animations()
    }

    /// Run timers/animations and repaint if the scene is dirty.
    pub fn render(&mut self, display: &mut Co5300Display) {
        slint::platform::update_timers_and_animations();
        self.window.draw_if_needed(|renderer| {
            let mut flusher = TwoLineFlusher {
                display,
                buf: &mut self.line_buf,
                scratch: &mut self.scratch,
                pending: None,
            };
            renderer.render_by_line(&mut flusher);
            flusher.flush_pending();
        });
    }
}

fn weather_label(code: u8) -> &'static str {
    match code {
        0 => "CLEAR",
        1..=3 => "CLOUDS",
        45 | 48 => "FOG",
        51..=67 => "RAIN",
        71..=77 => "SNOW",
        80..=82 => "SHOWERS",
        85 | 86 => "SNOW",
        95..=99 => "STORM",
        _ => "",
    }
}
```

Notes for the implementer:
- `set_mesh_self_text` / `set_mesh_rows` reference shell properties added in Task 7; until then, ADD the properties `mesh-self-text: string` and `mesh-rows: [PeerRow]` to `shell.slint`'s root in THIS task (unused by placeholders is fine) so this module compiles now.
- Check `src/peripherals/rtc.rs` for the exact `DateTime` field names (`weekday`, `day`, `month`, `year`, `hours`, `minutes`, `seconds` per current usage in `slint_demo.rs:181-185`).
- `PeerView.age_ms` is already an age (ms since last heard), not a timestamp — verify against `src/net/smol_mesh.rs:90-95` and drop the `now_ms` subtraction if so (then the signature loses `now_ms`). `main.rs:1203` passes `now.as_millis()` into `mesh.peers(...)` which computes ages — read `SmolMesh::peers` to confirm, and match its semantics.

- [ ] **Step 2: Register in `src/ui/mod.rs`**

Append: `pub mod slint_shell;`

- [ ] **Step 3: Confirm the demo bin is unaffected**

The demo keeps driving `WatchShell` directly (Task 2 form) and does NOT adopt `ShellUi` — `slint_shell` pulls in `apps`, `net`, and `peripherals` modules the demo doesn't include. Nothing to change; just confirm it still compiles in Step 4.

- [ ] **Step 4: Verify**

Run: `PATH="$HOME/.cargo/bin:$PATH" cargo check -q`
Expected: silent success. (`slint_shell` compiles in the main bin even though `main.rs` doesn't use it yet — it's `pub mod` under `ui`. Dead-code warnings are acceptable at this stage; silence with `#[allow(dead_code)]` on the module in `ui/mod.rs` if clippy is run.)

- [ ] **Step 5: Commit**

```bash
git add src/ui/slint_shell.rs src/ui/mod.rs ui/slint/shell.slint
git commit -m "feat(ui): ShellUi wrapper — request cells, property push, line-streamed render"
```

---

### Task 4: Sensors page

**Files:**
- Create: `ui/slint/sensors.slint`
- Modify: `ui/slint/shell.slint`

- [ ] **Step 1: Create `ui/slint/sensors.slint`**

```slint
import { Theme, StatRow, PageTitle } from "theme.slint";

export component SensorsPage {
    in property <string> accel-text;  // "+0.02  -0.98  +0.03 g"
    in property <string> gyro-text;   // "+1.2  -0.4  +0.0 dps"
    in property <string> imu-temp-text; // "27.5 C"

    PageTitle { title: "SENSORS"; }

    VerticalLayout {
        x: (parent.width - 330px) / 2;
        y: 130px;
        width: 330px;
        spacing: 8px;
        StatRow { label: "ACCEL"; value: root.accel-text; value-color: Theme.accent; }
        StatRow { label: "GYRO"; value: root.gyro-text; }
        StatRow { label: "IMU TEMP"; value: root.imu-temp-text; value-color: #ffd166; }
    }

    Text {
        x: (parent.width - self.width) / 2;
        y: 420px;
        text: "QMI8658 \u{00b7} 100ms";
        color: Theme.dim;
        font-size: 15px;
        letter-spacing: 2px;
    }
}
```

- [ ] **Step 2: Wire into `shell.slint`**

Add root properties:

```slint
    in property <string> accel-text: "--";
    in property <string> gyro-text: "--";
    in property <string> imu-temp-text: "--";
```

Replace the placeholder `for` loop's first entry: change the loop list to `["SYSTEM", "POWER", "MESH"]` with `x: (idx + 2 - root.current-page) * pages.width;` and insert before it:

```slint
        SensorsPage {
            x: (1 - root.current-page) * pages.width;
            width: pages.width; height: pages.height;
            animate x { duration: 260ms; easing: ease-out; }
            accel-text: root.accel-text;
            gyro-text: root.gyro-text;
            imu-temp-text: root.imu-temp-text;
        }
```

(Import at top: `import { SensorsPage } from "sensors.slint";`)

Add setter to `ShellUi` (`src/ui/slint_shell.rs`):

```rust
    pub fn set_sensors(&self, accel: (f32, f32, f32), gyro: (i16, i16, i16), temp_dc: i16) {
        self.ui.set_accel_text(slint::format!(
            "{:+.2} {:+.2} {:+.2} g", accel.0, accel.1, accel.2
        ));
        self.ui.set_gyro_text(slint::format!(
            "{:+.1} {:+.1} {:+.1}", gyro.0 as f32 / 10.0, gyro.1 as f32 / 10.0,
            gyro.2 as f32 / 10.0
        ));
        self.ui.set_imu_temp_text(slint::format!("{:.1} C", temp_dc as f32 / 10.0));
    }
```

- [ ] **Step 3: Verify** — `PATH="$HOME/.cargo/bin:$PATH" cargo check -q` → silent.

- [ ] **Step 4: Commit**

```bash
git add ui/slint/sensors.slint ui/slint/shell.slint src/ui/slint_shell.rs
git commit -m "feat(ui): Slint sensors page"
```

---

### Task 5: System page

**Files:**
- Create: `ui/slint/system.slint`
- Modify: `ui/slint/shell.slint`, `src/ui/slint_shell.rs`

- [ ] **Step 1: Create `ui/slint/system.slint`**

Before writing, read `src/ui/pages.rs:111-154` (`draw_system_page`) and mirror whatever rows it actually shows. Baseline component (adjust rows to match the eg page):

```slint
import { Theme, StatRow, PageTitle } from "theme.slint";

export component SystemPage {
    in property <string> chip-text: "ESP32-C6 \u{00b7} RISC-V";
    in property <string> display-text: "410x502 AMOLED";
    in property <string> heap-text;    // "123k free"
    in property <string> uptime-text;
    in property <string> battery-text; // "83% · 4012 mV"

    PageTitle { title: "SYSTEM"; }

    VerticalLayout {
        x: (parent.width - 330px) / 2;
        y: 120px;
        width: 330px;
        spacing: 6px;
        StatRow { label: "CHIP"; value: root.chip-text; }
        StatRow { label: "DISPLAY"; value: root.display-text; }
        StatRow { label: "HEAP"; value: root.heap-text; value-color: Theme.accent; }
        StatRow { label: "UPTIME"; value: root.uptime-text; }
        StatRow { label: "BATTERY"; value: root.battery-text; }
    }
}
```

- [ ] **Step 2: Wire into `shell.slint`** — same pattern as Task 4: import, add root properties (`heap-text`, `uptime-text`, `battery-text` as `in property <string>` defaulting `"--"`), replace the "SYSTEM" placeholder with `SystemPage { x: (2 - root.current-page) * pages.width; ... }`, shrink the placeholder loop to `["POWER", "MESH"]` at offsets `idx + 3`.

- [ ] **Step 3: `ShellUi` setter**

```rust
    pub fn set_system(&self, heap_free: usize, batt_pct: u8, batt_mv: u16) {
        self.ui.set_heap_text(slint::format!("{}k free", heap_free / 1024));
        let s = embassy_time::Instant::now().as_secs();
        self.ui.set_uptime_text(slint::format!(
            "{}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60
        ));
        self.ui.set_battery_text(slint::format!("{}% \u{00b7} {} mV", batt_pct, batt_mv));
    }
```

Heap-free source: `esp_alloc::HEAP.free()` (verify exact API in esp-alloc 0.10 docs — `HEAP.stats()` exists and implements `Display`; if there's no `free()`, format the stats' free field or log stats and pass a computed number).

- [ ] **Step 4: Verify + commit**

```bash
PATH="$HOME/.cargo/bin:$PATH" cargo check -q
git add ui/slint/system.slint ui/slint/shell.slint src/ui/slint_shell.rs
git commit -m "feat(ui): Slint system page with live heap stats"
```

---

### Task 6: Power page (stats rows + brightness slider + reboot)

> **AMENDED during execution (2026-07-19):** the baseline below lost the old
> page's core feature — the per-subsystem mA monitor. As built: a 2-column
> subsystem grid (CPU/DISPLAY/WIFI/BLE/IMU/AUDIO, each "name · state · mA"),
> full-width color-coded TOTAL row, RUNTIME row (100%/left estimates), slider
> at y366 (SLIDER_BAND unchanged), reboot at y414 h36 (clears page dots). The
> separate BATTERY row was consolidated away (chrome pill + system page carry
> it). The mA/total/runtime estimation math moved from `power_page.rs` into
> `PowerStats` methods in `src/peripherals/power_stats.rs` (UI-free), with the
> old eg page delegating to it — so Task 13 deletes rendering only. Page-index
> constants (PAGE_CLOCK..PAGE_MESH, PAGE_COUNT) were also introduced in
> `slint_shell.rs` here and replace all magic page indices.

**Files:**
- Create: `ui/slint/power.slint`
- Modify: `ui/slint/shell.slint`, `src/ui/slint_shell.rs`

- [ ] **Step 1: Create `ui/slint/power.slint`**

Read `src/ui/power_page.rs` first and mirror its rows. Baseline:

```slint
import { Theme, StatRow, PageTitle } from "theme.slint";

export component PowerPage {
    in property <string> display-state-text; // "ON" / "DIM" / "AOD" / "OFF"
    in property <string> wifi-state-text;    // "OFF" / "STA" / ...
    in property <string> radios-text;        // "BLE on · IMU off"
    in property <string> cpu-text;           // "160 MHz"
    in property <string> battery-text;       // "83% · 4012 mV · CHG"
    in-out property <float> brightness;
    callback brightness-changed(float);
    callback reboot-tap();

    PageTitle { title: "POWER"; }

    VerticalLayout {
        x: (parent.width - 330px) / 2;
        y: 110px;
        width: 330px;
        spacing: 4px;
        StatRow { label: "DISPLAY"; value: root.display-state-text; }
        StatRow { label: "WIFI"; value: root.wifi-state-text; }
        StatRow { label: "RADIOS"; value: root.radios-text; }
        StatRow { label: "CPU"; value: root.cpu-text; }
        StatRow { label: "BATTERY"; value: root.battery-text; value-color: Theme.accent; }
    }

    // Brightness slider — same custom control as the old demo stats page.
    // Rust swipe handling ignores horizontal swipes starting in y 330..430
    // while this page is showing (SLIDER_BAND in slint_shell.rs).
    Text {
        x: (parent.width - 330px) / 2;
        y: 336px;
        text: "BRIGHTNESS";
        color: Theme.dim; font-size: 16px; letter-spacing: 2px;
    }
    slider := Rectangle {
        x: (parent.width - self.width) / 2;
        y: 366px;
        width: 330px; height: 40px;
        Rectangle {
            x: 0; y: (parent.height - self.height) / 2;
            width: parent.width; height: 10px; border-radius: 5px;
            background: Theme.track;
            Rectangle {
                x: 0; y: 0;
                width: max(10px, parent.width * root.brightness);
                height: parent.height; border-radius: 5px;
                background: @linear-gradient(90deg, #3a7bd5 0%, #35e0b0 100%);
            }
        }
        Rectangle {
            x: clamp(slider.width * root.brightness - self.width / 2,
                     0px, slider.width - self.width);
            y: (parent.height - self.height) / 2;
            width: 26px; height: 26px; border-radius: 13px;
            background: Theme.ink;
        }
        TouchArea {
            pointer-event(ev) => {
                if (ev.kind == PointerEventKind.down
                    || (ev.kind == PointerEventKind.move && self.pressed)) {
                    root.brightness = clamp(self.mouse-x / self.width, 0.0, 1.0);
                    root.brightness-changed(root.brightness);
                }
            }
        }
    }

    // Reboot button (kicks OTA check first when WiFi is up, then resets)
    reboot := Rectangle {
        x: (parent.width - self.width) / 2;
        y: 430px;
        width: 180px; height: 40px; border-radius: 20px;
        border-width: 1px; border-color: Theme.warn;
        background: #200a10;
        Text {
            x: (parent.width - self.width) / 2;
            y: (parent.height - self.height) / 2;
            text: "REBOOT"; color: Theme.warn; font-size: 16px; letter-spacing: 3px;
        }
        TouchArea { clicked => { root.reboot-tap(); } }
    }
}
```

- [ ] **Step 2: Wire into `shell.slint`** — import; add root string properties `display-state-text`, `wifi-state-text`, `radios-text`, `battery-text` (defaults `"--"`); place `PowerPage { x: (3 - root.current-page) * pages.width; ... }` binding `brightness <=> root.brightness`, `brightness-changed => root.brightness-changed(...)`, `reboot-tap => root.reboot-tap()`, `cpu-text: root.cpu-text`; placeholder loop shrinks to `["MESH"]` at `idx + 4`.

- [ ] **Step 3: `ShellUi` setter**

```rust
    pub fn set_power(&self, stats: &crate::peripherals::power_stats::PowerStats) {
        use crate::peripherals::power_stats::{DisplayState, WifiMode};
        self.ui.set_display_state_text(SharedString::from(match stats.display {
            Some(DisplayState::On) => "ON",
            Some(DisplayState::Dim) => "DIM",
            Some(DisplayState::Aod) => "AOD",
            Some(DisplayState::Off) | None => "OFF",
        }));
        self.ui.set_wifi_state_text(SharedString::from(match stats.wifi {
            Some(WifiMode::Sta) => "STA",
            Some(WifiMode::Off) | None => "OFF",
        }));
        self.ui.set_radios_text(slint::format!(
            "BLE {} \u{00b7} IMU {}",
            if stats.ble_on { "on" } else { "off" },
            if stats.imu_on { "on" } else { "off" }
        ));
        self.ui.set_battery_text(slint::format!(
            "{}% \u{00b7} {} mV{}",
            stats.battery_pct, stats.battery_mv,
            if stats.charging { " \u{00b7} CHG" } else { "" }
        ));
    }
```

Check the real `DisplayState` / `WifiMode` variant names in `src/peripherals/power_stats.rs` before writing the matches — the ones above are guesses to be corrected against the source.

- [ ] **Step 4: Verify + commit**

```bash
PATH="$HOME/.cargo/bin:$PATH" cargo check -q
git add ui/slint/power.slint ui/slint/shell.slint src/ui/slint_shell.rs
git commit -m "feat(ui): Slint power page — stats, brightness slider, reboot"
```

---

### Task 7: Mesh page

**Files:**
- Create: `ui/slint/mesh.slint`
- Modify: `ui/slint/shell.slint`, `src/ui/slint_shell.rs` (only if Task 3's mesh setter needs signature fixes)

- [ ] **Step 1: Create `ui/slint/mesh.slint`**

Read `src/ui/pages.rs:167-230` (`draw_mesh_page`) for the roster layout it renders (realm names + RSSI + age). Baseline:

```slint
import { Theme, PageTitle } from "theme.slint";
import { PeerRow } from "shell.slint";

export component MeshPage {
    in property <string> self-text;   // "#042 Realm Name"
    in property <[PeerRow]> rows;

    PageTitle { title: "MESH"; }

    Text {
        x: (parent.width - self.width) / 2;
        y: 100px;
        text: root.self-text;
        color: Theme.accent;
        font-size: 20px;
        font-weight: 600;
    }

    VerticalLayout {
        x: 30px;
        y: 150px;
        width: 350px;
        spacing: 4px;
        for peer in root.rows: Rectangle {
            height: 40px;
            border-radius: 8px;
            background: Theme.panel;
            Text {
                x: 14px; y: (parent.height - self.height) / 2;
                text: peer.name; color: Theme.ink; font-size: 17px;
            }
            Text {
                x: parent.width - self.width - 90px;
                y: (parent.height - self.height) / 2;
                text: peer.rssi; color: Theme.soft; font-size: 15px;
            }
            Text {
                x: parent.width - self.width - 14px;
                y: (parent.height - self.height) / 2;
                text: peer.age; color: Theme.dim; font-size: 15px;
            }
        }
    }

    if root.rows.length == 0: Text {
        x: (parent.width - self.width) / 2;
        y: 240px;
        text: "NO PEERS";
        color: Theme.faint;
        font-size: 18px;
        letter-spacing: 3px;
    }
}
```

Import-cycle note: if importing `PeerRow` from `shell.slint` creates a cycle (shell imports mesh), move the `export struct PeerRow` into `theme.slint` and import it from there in both files.

- [ ] **Step 2: Wire into `shell.slint`** — import `MeshPage`; delete the placeholder `for` loop entirely; add `MeshPage { x: (4 - root.current-page) * pages.width; ...; self-text: root.mesh-self-text; rows: root.mesh-rows; }` (the two root properties exist since Task 3).

- [ ] **Step 3: Verify + commit**

```bash
PATH="$HOME/.cargo/bin:$PATH" cargo check -q
git add ui/slint/mesh.slint ui/slint/shell.slint
git commit -m "feat(ui): Slint mesh roster page"
```

---

### Task 8: Launcher overlay

**Files:**
- Create: `ui/slint/launcher.slint`
- Modify: `ui/slint/shell.slint`

- [ ] **Step 1: Create `ui/slint/launcher.slint`**

Item order MUST match `LAUNCHER_APPS` in `src/ui/slint_shell.rs` (Snake, World Snake, 2048, Tetris, Flappy Bird, Maze (Tilt), Settings).

```slint
import { Theme } from "theme.slint";

export component LauncherOverlay {
    callback launch-app(int);

    Rectangle {
        width: 100%; height: 100%;
        background: #05080aF2;
    }

    Text {
        x: (parent.width - self.width) / 2;
        y: 20px;
        text: "APPS";
        color: Theme.ink; font-size: 24px; font-weight: 600; letter-spacing: 4px;
    }

    Flickable {
        x: 20px; y: 55px;
        width: parent.width - 40px;
        height: parent.height - 75px;
        viewport-height: 7 * 71px;
        VerticalLayout {
            spacing: 6px;
            for item[idx] in [
                { name: "Snake",       accent: #00e000 },
                { name: "World Snake", accent: #00ff80 },
                { name: "2048",        accent: #f0d000 },
                { name: "Tetris",      accent: #00d0f0 },
                { name: "Flappy Bird", accent: #ffffff },
                { name: "Maze (Tilt)", accent: #8090ff },
                { name: "Settings",    accent: #c0ffc0 },
            ]: Rectangle {
                height: 65px;
                border-radius: 12px;
                background: Theme.panel;
                Rectangle {
                    x: 0; y: 8px; width: 4px; height: parent.height - 16px;
                    background: item.accent;
                }
                Text {
                    x: (parent.width - self.width) / 2;
                    y: (parent.height - self.height) / 2;
                    text: item.name; color: item.accent;
                    font-size: 20px; font-weight: 600;
                }
                TouchArea { clicked => { root.launch-app(idx); } }
            }
        }
    }
}
```

- [ ] **Step 2: Wire into `shell.slint`**

Import, then add just above the AOD overlay (so AOD still wins):

```slint
    if root.launcher-open: LauncherOverlay {
        width: 100%; height: 100%;
        launch-app(idx) => { root.launch-app(idx); }
    }
```

- [ ] **Step 3: Verify + commit**

```bash
PATH="$HOME/.cargo/bin:$PATH" cargo check -q
git add ui/slint/launcher.slint ui/slint/shell.slint
git commit -m "feat(ui): Slint launcher overlay (Flickable list)"
```

- [ ] **Step 4 (HW optional): Flash the demo bin**

If `ls /dev/ttyACM*` shows the watch: `PATH="$HOME/.cargo/bin:$PATH" cargo run --release --bin slint-demo`, verify: clock renders, swipes move through 5 pages (4 with live data placeholders), up-swipe opens launcher, brightness slider works on power page. Otherwise mark HW-GATE-HELD.

---

### Task 9: main.rs cutover — shell mode replaces eg watchface + launcher

The heart of the migration. The `AppState::Watchface` and `AppState::Launcher` match arms are replaced by one shell-mode arm; the eg watchface struct's live state moves to plain locals.

**Files:**
- Modify: `src/main.rs`

- [ ] **Step 1: Capture per-iteration touch point**

In the touch poll block (`src/main.rs:694-710`), lift the point into a local visible to the state machine — replace the block with:

```rust
        let mut swipe_event = None;
        let mut swipe_start_y: u16 = 0;
        let mut tap_event = false;
        let mut touch_point: Option<crate::peripherals::touch::TouchPoint> = None;
        let int_low = touch_int.is_low();
        let touch_active = screen_state >= 2 && (int_low || was_touching);
        was_touching = int_low;
        if touch_active {
            if let Ok((point, event)) = touch.poll() {
                touch_point = point;
                if let Some(tp) = point {
                    last_touch_x = tp.x;
                    last_touch_y = tp.y;
                }
                if let Some(swipe) = event {
                    swipe_event = Some(swipe.direction);
                    swipe_start_y = swipe.start_y;
                    tap_event = swipe.direction == SwipeDirection::Tap;
                }
            }
        }
```

(Verify `TouchPoint` is `Copy` — it's used by value in the demo; if not, bind by reference.)

- [ ] **Step 2: Boot-time shell construction and watchface-state locals**

After `display.init()` / before the fb allocation (`src/main.rs:312`), add:

```rust
    let mut shell = crate::ui::slint_shell::ShellUi::new();
    println!("[SLINT] shell up");
```

Introduce locals replacing `WatchFace` live state (place near the other loop state around `src/main.rs:520-570`; exact insertion next to `let mut watchface = WatchFace::new();` which this step DELETES along with its uses):

```rust
    let mut brightness: u8 = 0xA0;
    let mut gyro_enabled = false;
    let mut cpu_mhz: u16 = 160;
    let mut steps: u32 = 0;
```

Then migrate every `watchface.*` reference in `main.rs` (grep `watchface\.`); the full replacement map:

| old | new |
|---|---|
| `watchface.brightness` | `brightness` |
| `watchface.gyro_enabled` | `gyro_enabled` |
| `watchface.cpu_mhz` | `cpu_mhz` |
| `watchface.steps = steps_val` | `steps = steps_val; shell.set_steps(steps);` |
| `watchface.update_time(h, m, s)` | *(delete — shell time push happens in the shell arm via `shell.set_time(&dt)`; keep the RTC read and stash `dt` in a `last_dt: Option<DateTime>` local)* |
| `watchface.update_date(...)` | *(delete — covered by `set_time`)* |
| `watchface.update_battery(pct, mv, chg)` | `shell.set_battery(pct, mv, chg);` |
| `watchface.update_accel(x, y, z)` | *(delete — parallax is Task 12; accel already lives in the `accel` local)* |
| `watchface.force_redraw()` / `page_dirty = true` | *(delete — Slint tracks its own dirty state)* |
| `watchface.fam = ...` | *(delete for now — Familiar returns in Task 12)* |
| `watchface.update_weather(...)` (grep exact name) | `shell.set_weather(temp_f, code);` |
| tap-zone helpers (`is_wifi_zone` etc.) | *(delete — Slint TouchAreas own this)* |

- [ ] **Step 3: Replace the `AppState::Watchface` arm (`src/main.rs:1163-1325`) and the `AppState::Launcher` arm (`src/main.rs:1392+`) with one shell arm**

```rust
            AppState::Watchface | AppState::Launcher => {
                // Mirror launcher state into the scene (boot button toggles below).
                shell.set_launcher_open(app_state == AppState::Launcher);

                // Touch → pointer events + page/launcher swipes.
                shell.handle_touch(touch_point, swipe_event, swipe_start_y);
                if shell.launcher_open() != (app_state == AppState::Launcher) {
                    // Swipe-driven open/close inside the shell.
                    app_state = if shell.launcher_open() {
                        AppState::Launcher
                    } else {
                        AppState::Watchface
                    };
                }

                // Per-page live data on its existing cadence.
                match shell.page() {
                    1 => shell.set_sensors(accel, gyro_data, imu_temp),
                    2 => {
                        if now >= next_flush {
                            shell.set_system(esp_alloc::HEAP.free(), batt_pct, batt_mv);
                            next_flush = now + Duration::from_secs(2);
                        }
                    }
                    3 => {
                        if now >= next_flush {
                            update_power_stats(
                                &mut power_stats, screen_state, imu_powered,
                                wifi_connected, wifi_on_request, brightness,
                                batt_mv, batt_pct, charging,
                            );
                            shell.set_power(&power_stats);
                            next_flush = now + Duration::from_secs(1);
                        }
                    }
                    4 => {
                        if now >= next_flush {
                            let mut rows = [PeerView::default(); pages::MESH_MAX_ROWS];
                            let n = mesh.peers(now.as_millis(), &mut rows);
                            shell.set_mesh_rows(watch_cfg.node_id, &rows[..n], now.as_millis());
                            next_flush = now + Duration::from_secs(1);
                        }
                    }
                    _ => {}
                }

                // 1Hz time push (uses the dt stashed by the RTC block).
                if let Some(dt) = last_dt.as_ref() {
                    let _ = shell.set_time(dt);
                }

                // Drain UI requests.
                if let Some(raw) = shell.req.brightness.take() {
                    brightness = raw;
                    display.set_brightness(raw);
                }
                if shell.req.wifi_toggle.take() {
                    wifi_toggle_request = true;
                }
                if shell.req.ble_toggle.take() {
                    ble_toggle_request = true;
                }
                if shell.req.cpu_cycle.take() {
                    cpu_mhz = match cpu_mhz {
                        160 => 80,
                        80 => 40,
                        _ => 160,
                    };
                    let actual = crate::peripherals::cpu_clock::set_cpu_mhz(cpu_mhz);
                    cpu_mhz = actual;
                    power_stats.cpu_mhz = actual;
                    shell.set_cpu_mhz(actual);
                }
                if shell.req.gyro_toggle.take() {
                    gyro_enabled = !gyro_enabled;
                    shell.set_gyro(gyro_enabled);
                    println!("Gyro: {}", if gyro_enabled { "ON" } else { "OFF" });
                }
                if shell.req.reboot.take() {
                    println!("REBOOT requested");
                    if wifi_connected && crate::net::ota_http::URL_SET {
                        if let Err(e) = crate::net::ota_http::ota_update(stack, &mut flash).await {
                            println!("[OTA] failed: {e}");
                        }
                    }
                    esp_hal::system::software_reset();
                }
                if let Some(target) = shell.req.launch.take() {
                    shell.set_launcher_open(false);
                    app_state = target;
                    // (Task 10 inserts the framebuffer allocation here.)
                }

                if boot_button.is_low() {
                    let opening = app_state == AppState::Watchface;
                    shell.set_launcher_open(opening);
                    app_state = if opening { AppState::Launcher } else { AppState::Watchface };
                    Timer::after(Duration::from_millis(200)).await;
                }

                // Repaint if dirty (full frame, line-streamed, no framebuffer).
                if screen_state >= 2 {
                    shell.render(&mut display);
                }
            }
```

Cross-check the `cycle_cpu` ladder against `WatchFace::cycle_cpu` (`src/ui/watchface.rs:514`) and use its exact MHz steps.

- [ ] **Step 4: Stash the RTC read + radio pushes**

RTC block (`src/main.rs:653-660`): keep the read, replace `watchface.update_*` per Step 2's table, add `last_dt = Some(dt);` (declare `let mut last_dt: Option<DateTime> = None;` with the other locals; import `DateTime` from `crate::peripherals::rtc`).

Wherever `wifi_connected`, BLE state, or `last_mesh_peers` change (grep assignments), add `shell.set_radios(wifi_connected, ble_on, last_mesh_peers);` — one call site right after the mesh tick section is enough if it runs every iteration cheaply; only call when a value actually changed (guard with a `prev_radios` tuple local).

- [ ] **Step 5: Tick cadence for shell animations**

In the tick computation (`src/main.rs:584-600`), replace the `AppState::Watchface` match block's page cadences with Slint-aware pacing:

```rust
                AppState::Watchface | AppState::Launcher => {
                    if shell.has_active_animations() {
                        Duration::from_millis(33)
                    } else {
                        match shell.page() {
                            1 => Duration::from_millis(100), // sensors live
                            3 | 4 => Duration::from_secs(1), // power / mesh refresh
                            2 => Duration::from_secs(2),     // system
                            _ => Duration::from_secs(1),     // clock: 1Hz seconds
                        }
                    }
                }
```

Delete the now-unused `AppState::Launcher | AppState::Settings => Duration::from_millis(100)` line's Launcher half (Settings keeps 100ms via the catch-all `_ =>` arm).

- [ ] **Step 6: Screen wake/AOD hooks**

Wake path (`src/main.rs:714-729`): replace `display.set_brightness(watchface.brightness)` → `display.set_brightness(brightness)`, and drop `watchface.force_redraw(); page_dirty = true;` (Slint repaints on next `render`). AOD handling changes land in Task 11; for THIS task, screen_state 1 simply behaves like state 2 (dim, shell renders normally) — set `shell.set_aod(false)` unconditionally at the wake site so nothing sticks.

- [ ] **Step 7: Remove dead references, keep the fb**

Delete: `let mut watchface = WatchFace::new();`, `current_page` local + `Page` imports IF no other arm references them (Settings/games don't), `launcher` local + `Launcher::new()`, `page_dirty`, `aod_last_minute` (returns in Task 11). Keep `fb` boot-allocated — games still use it. `use crate::ui::watchface::WatchFace;`, `use crate::ui::pages::{self, Page};` shrink to what's still referenced (`pages::MESH_MAX_ROWS` still used by the mesh push; keep `use crate::ui::pages;`). `power_page` import goes.

- [ ] **Step 8: Verify**

```bash
PATH="$HOME/.cargo/bin:$PATH" cargo check -q
PATH="$HOME/.cargo/bin:$PATH" cargo build --release 2>&1 | tail -3
```
Expected: check silent; release build succeeds for both bins.

- [ ] **Step 9: HW GATE — flash and smoke-test**

If `ls /dev/ttyACM*` present: `PATH="$HOME/.cargo/bin:$PATH" cargo run --release --bin esp32c6-watch`; verify boot log shows `[SLINT] shell up`, clock ticks, all 5 pages swipe, launcher opens (swipe-up + boot button), Snake launches and exits back to the shell, WiFi/BLE chips toggle, brightness slider works, reboot button resets. Else HW-GATE-HELD.

- [ ] **Step 10: Commit**

```bash
git add src/main.rs
git commit -m "feat(ui): cut main firmware over to the Slint shell (watchface + launcher)"
```

---

### Task 10: On-demand framebuffer + RAM-busy toast

**Files:**
- Modify: `src/drivers/framebuffer.rs`
- Modify: `src/main.rs`

- [ ] **Step 1: Fallible constructor in `src/drivers/framebuffer.rs`**

Add beside `new()` (keep `new()` for now; it goes away in Task 13):

```rust
    /// Allocate without aborting on OOM: games grab ~202KB on entry and the
    /// shell reclaims it on exit. None = heap can't fit a frame right now.
    pub fn try_new() -> Option<Self> {
        let mut buf: Vec<u8> = Vec::new();
        buf.try_reserve_exact(PIXEL_COUNT).ok()?;
        buf.resize(PIXEL_COUNT, 0);
        let mut row: Vec<u16> = Vec::new();
        row.try_reserve_exact(WIDTH).ok()?;
        row.resize(WIDTH, 0);
        Some(Self { buf, row })
    }
```

- [ ] **Step 2: `main.rs` — fb becomes `Option`, allocated at app entry**

- Replace boot alloc (`src/main.rs:312-315`) with heap-stat logging:

```rust
    let mut fb: Option<Framebuffer> = None;
    println!("[HEAP] boot: {}", esp_alloc::HEAP.stats());
```

(Blank the panel once at boot via the shell's first render instead of `fb.flush`.)

- In the shell arm's launch-request drain (Task 9 Step 3 marked the spot), replace `app_state = target;` with:

```rust
                if let Some(target) = shell.req.launch.take() {
                    match Framebuffer::try_new() {
                        Some(f) => {
                            fb = Some(f);
                            println!("[HEAP] app enter: {}", esp_alloc::HEAP.stats());
                            shell.set_launcher_open(false);
                            shell.set_toast("");
                            app_state = target;
                        }
                        None => {
                            shell.set_toast("RAM busy \u{2014} try after the WiFi window");
                            toast_until = now + Duration::from_secs(3);
                        }
                    }
                }
```

Declare `let mut toast_until = Instant::now();` with the loop locals, and in the shell arm (before `shell.render`) add:

```rust
                if now >= toast_until {
                    shell.set_toast("");
                }
```

(Slint property sets are cheap no-ops when unchanged... they are NOT — gate it: keep a `toast_active: bool` local, only call `set_toast("")` once when it flips.)

- Every app-exit site sets `fb = None` and forces a shell repaint. Grep `app_state = AppState::Watchface` and `app_state = AppState::Launcher` inside the game/settings arms — all of these (7 apps: Snake `:1356-1367`, WorldSnake `:1386-1389`, and the equivalents in Game2048/Tetris/Flappy/Maze/Settings arms) get, right after the assignment:

```rust
                        fb = None;
                        println!("[HEAP] app exit: {}", esp_alloc::HEAP.stats());
```

(WorldSnake's boot-button exit goes to `AppState::Launcher` — going to the launcher stays in shell mode, so the fb drop is correct there too.)

- Every in-app use of `fb` becomes `fb.as_mut()`: the app arms' render calls change from `game.render(&mut fb)` to:

```rust
                let Some(fb_ref) = fb.as_mut() else {
                    app_state = AppState::Watchface;
                    continue;
                };
                // ... existing arm body using fb_ref instead of fb ...
```

Put the `let-else` guard at the top of each of the 7 app arms (defensive: unreachable in practice because entry allocates, but it makes the Option safe by construction).

- [ ] **Step 3: Verify esp-alloc stats API**

`esp_alloc::HEAP.stats()` — check the esp-alloc 0.10 docs/source (`~/.cargo/registry/src/*/esp-alloc-0.10*/src/lib.rs`) for the exact method (`stats()` returning a `Display` type, and `free()`). Adjust the three `println!` calls and Task 5's `set_system(esp_alloc::HEAP.free(), ...)` to the real API.

- [ ] **Step 4: Verify + build + commit**

```bash
PATH="$HOME/.cargo/bin:$PATH" cargo check -q
PATH="$HOME/.cargo/bin:$PATH" cargo build --release 2>&1 | tail -3
git add src/drivers/framebuffer.rs src/main.rs
git commit -m "feat(fw): on-demand framebuffer — shell mode runs framebuffer-free, apps allocate on entry"
```

- [ ] **Step 5: HW GATE — memory soak**

If the watch is on USB: flash, then over the serial monitor confirm the three `[HEAP]` log lines; enter/exit Snake 10 times in a row and after a WiFi window; confirm no `RAM busy` toast in normal use and no alloc failure. Note the boot-time free heap in the PR description. Else HW-GATE-HELD.

---

### Task 11: AOD (always-on display) via the shell

**Files:**
- Modify: `src/main.rs`

- [ ] **Step 1: Reintroduce the AOD path in the sleep state machine**

The screen state machine (`src/main.rs:712-748`) keeps its exact thresholds. Changes:

- Where state 1 is entered (`idle_secs >= 15`, clock page, `src/main.rs:735-739`): the old code set `aod_last_minute = 99`. New code (the page check uses the shell):

```rust
        } else if idle_secs >= 15 && screen_state > 1 {
            if app_state == AppState::Watchface && shell.page() == 0 {
                display.set_brightness(0x18);
                screen_state = 1;
                shell.set_aod(true);
            } else {
                display.set_brightness(0x00);
                display.display_off();
                screen_state = 0;
            }
        }
```

- Wake path: where `screen_state = 3` is set, add `shell.set_aod(false);`.
- Find the old `screen_state == 1` render block (grep `render_aod` in `main.rs`) and delete it; instead, in the shell arm change the render gate from `screen_state >= 2` to:

```rust
                if screen_state >= 2 {
                    shell.render(&mut display);
                } else if screen_state == 1 {
                    // AOD: repaint only when the minute changes (set_time
                    // returns true on a second tick; check minutes).
                    if let Some(dt) = last_dt.as_ref() {
                        if dt.minutes != aod_last_minute {
                            aod_last_minute = dt.minutes;
                            shell.render(&mut display);
                        }
                    }
                }
```

Re-declare `let mut aod_last_minute: u8 = 99;` with the loop locals (it was removed in Task 9 Step 7). Also confirm the tick match gives screen_state 1 its existing 10s cadence (`src/main.rs:581-582` — unchanged, but 10s means up to 10s late on a minute flip; tighten state 1's tick to `Duration::from_secs(5)` so AOD minutes never look stuck).

- [ ] **Step 2: Verify + build + commit**

```bash
PATH="$HOME/.cargo/bin:$PATH" cargo check -q
git add src/main.rs
git commit -m "feat(ui): AOD rendered by the Slint shell (minute-gated repaints)"
```

- [ ] **Step 3: HW GATE** — flash; let the watch idle 15s on the clock → dim AOD scene appears; wait a minute boundary → time updates; touch → full shell returns; idle 180s → panel off; touch → wakes. Else HW-GATE-HELD.

---

### Task 12: Polish — Familiar creature, gyro parallax

**Files:**
- Modify: `ui/slint/clock.slint`, `ui/slint/shell.slint`, `src/ui/slint_shell.rs`, `src/main.rs`

- [ ] **Step 1: Familiar creature on the clock page**

Read `src/ui/watchface.rs:721-803` (`draw_familiar` + `fam_region`) and `src/ui/watchface.rs:146-155` (`FamUi`) for what the creature communicates: `known`, `holding`, `mood: u8`, `hunger: u8`, `stage: u8`. First cut in Slint — a compact status glyph cluster (not sprite art):

Add to `clock.slint`:

```slint
    in property <bool> fam-known;
    in property <bool> fam-holding;
    in property <int> fam-mood;   // 0..255
    in property <int> fam-hunger; // 0..255
    in property <int> fam-stage;  // 0 egg, 1 hatchling, 2 grown (check FamUi docs)

    if root.fam-known: Rectangle {
        x: 24px; y: 300px;
        width: 96px; height: 64px;
        border-radius: 12px;
        background: root.fam-holding ? #10241c : Theme.panel;
        border-width: 1px;
        border-color: root.fam-holding ? Theme.accent : Theme.line;
        Text {
            x: (parent.width - self.width) / 2; y: 6px;
            text: root.fam-stage == 0 ? "\u{25cf}" : (root.fam-stage == 1 ? "\u{25d0}" : "\u{25c9}");
            color: root.fam-holding ? Theme.accent : Theme.soft;
            font-size: 22px;
        }
        // mood/hunger micro-bars
        Rectangle {
            x: 10px; y: 42px; width: 76px; height: 4px; border-radius: 2px;
            background: Theme.track;
            Rectangle { x: 0; y: 0; height: 100%; border-radius: 2px;
                width: parent.width * (root.fam-mood / 255.0);
                background: Theme.accent; }
        }
        Rectangle {
            x: 10px; y: 50px; width: 76px; height: 4px; border-radius: 2px;
            background: Theme.track;
            Rectangle { x: 0; y: 0; height: 100%; border-radius: 2px;
                width: parent.width * (root.fam-hunger / 255.0);
                background: #ffd166; }
        }
    }
```

Thread the five properties through `shell.slint` (same pattern as every page property), add the ShellUi setter:

```rust
    pub fn set_fam(&self, f: &crate::ui::watchface::FamUi) {
        self.ui.set_fam_known(f.known);
        self.ui.set_fam_holding(f.holding);
        self.ui.set_fam_mood(f.mood as i32);
        self.ui.set_fam_hunger(f.hunger as i32);
        self.ui.set_fam_stage(f.stage as i32);
    }
```

and call it from `main.rs` where the old code assigned `watchface.fam` (grep the pre-Task-9 git history: `git show HEAD~N:src/main.rs | grep -n "\.fam"` to find the update site; re-add the push there, gated on value change). Since `FamUi` lives in `watchface.rs` which Task 13 deletes, MOVE the `FamUi` struct into `src/net/familiar.rs` as part of this step and update the import.

- [ ] **Step 2: Gyro parallax**

In the shell arm of `main.rs`, when `gyro_enabled` and on page 0, feed scaled accel into the parallax offsets (matching the old eg behavior in `watchface.rs` — read `update_accel` at `src/ui/watchface.rs:239-248` for the scale factor it used and mirror it):

```rust
                if gyro_enabled && shell.page() == 0 {
                    shell.set_parallax(accel.0, accel.1);
                }
```

ShellUi setter (clamp to ±12px so text never collides with chrome):

```rust
    pub fn set_parallax(&self, ax: f32, ay: f32) {
        self.ui.set_par_x((ax * 12.0).clamp(-12.0, 12.0));
        self.ui.set_par_y((ay * 12.0).clamp(-12.0, 12.0));
    }
```

(`par-x`/`par-y` are `length` in .slint; the generated Rust setter takes `f32` logical pixels.) Tick cadence: extend Task 9 Step 5's page-0 arm to `if gyro_enabled { 33ms } else { 1s }` mirroring `src/main.rs:586-592`.

- [ ] **Step 3: Verify + build + commit**

```bash
PATH="$HOME/.cargo/bin:$PATH" cargo check -q
git add ui/slint/clock.slint ui/slint/shell.slint src/ui/slint_shell.rs src/net/familiar.rs src/main.rs
git commit -m "feat(ui): Familiar status cluster + gyro parallax on the Slint clock"
```

---

### Task 13: Delete the dead eg shell code

**Files:**
- Delete: `src/ui/watchface.rs`, `src/ui/pages.rs`, `src/ui/launcher.rs`, `src/ui/power_page.rs`
- Modify: `src/ui/mod.rs`, `src/main.rs`, `src/ui/slint_shell.rs`, `src/drivers/framebuffer.rs`

- [ ] **Step 1: Untangle survivors**

- `pages::MESH_MAX_ROWS` is still used by the mesh push → move the constant to `src/net/smol_mesh.rs` (`pub const MESH_MAX_ROWS: usize = 7;`) and update the use sites (`main.rs` mesh push, `slint_shell.rs` — it references it since Task 3).
- `power_page.rs` is rendering-only by now (Task 6 moved the mA/total/runtime estimation math into `PowerStats` methods in `src/peripherals/power_stats.rs`) — confirm no estimation logic remains before deleting.
- `FamUi` already moved in Task 12.
- `t9_keyboard` stays (used by `apps/settings.rs` — verify with `grep -rn "t9_keyboard" src/`).
- `Framebuffer::new()` (infallible) now unused → delete it, keep `try_new()`.
- Check nothing else imports the four files: `grep -rn "watchface\|ui::pages\|ui::launcher\|power_page" src/ --include="*.rs"` — chase every hit before deleting.

- [ ] **Step 2: Delete + prune `src/ui/mod.rs`**

```rust
pub mod t9_keyboard;
pub mod slint_platform;
pub mod slint_shell;
```

```bash
git rm src/ui/watchface.rs src/ui/pages.rs src/ui/launcher.rs src/ui/power_page.rs
```

- [ ] **Step 3: Verify (this is the clippy gate too)**

```bash
PATH="$HOME/.cargo/bin:$PATH" cargo check -q
PATH="$HOME/.cargo/bin:$PATH" cargo clippy -q --release 2>&1 | tail -5
```
Expected: no errors; fix any dead-code warnings the deletion exposes.

- [ ] **Step 4: Commit**

```bash
git add -u src/
git commit -m "refactor(ui): drop the embedded-graphics watchface shell (Slint owns it now)"
```

---

### Task 14: Size/heap verification + ship

**Files:**
- None (verification + PR)

- [ ] **Step 1: Binary size vs partition budget**

```bash
PATH="$HOME/.cargo/bin:$PATH" cargo build --release
ls -l target/riscv32imac-unknown-none-elf/release/esp32c6-watch
PATH="$HOME/.cargo/bin:$PATH" espflash save-image --chip esp32c6 \
  target/riscv32imac-unknown-none-elf/release/esp32c6-watch /tmp/watch.bin && ls -l /tmp/watch.bin
```
Expected: image < 4MB (ota_0/ota_1 slots are 0x400000 each per `partitions.csv`). Record the size delta vs `main` in the PR body.

- [ ] **Step 2: HW GATE — full smoke pass**

Flash; walk: boot → clock → all pages → launcher → each of the 7 apps in and out → AOD → deep sleep → wake → WiFi window (weather text appears) → mesh page with a second node if one is powered. Capture the `[HEAP]` boot line.

- [ ] **Step 3: Ship**

Per the ship workflow: push `feat/slint-shell`, open a PR titled `feat(ui): migrate watch shell to Slint (spec 2026-07-19)` with the size + heap numbers and any HW-GATE-HELD notes, then `git checkout main` (CLAUDE.md: always return to main).

---

## Self-review notes (kept for the executor)

- Property/method names between `.slint` and Rust follow Slint's kebab→snake mapping (`time-text` → `set_time_text`); if a setter doesn't exist at compile time, the property name drifted — fix the `.slint` side.
- Anywhere this plan says "check the real source first" (DateTime fields, PowerStats variants, `SmolMesh::peers` age semantics, esp-alloc stats API, `cycle_cpu` ladder, `update_accel` scale, FamUi stage meanings) — that check is part of the task, not optional.
- The demo bin intentionally stays a thin direct-`WatchShell` harness; only the main bin uses `ShellUi`.
- Games/Settings/T9 are untouched by design; if a task seems to require editing `src/apps/*`, stop — that's scope creep (the only sanctioned edits there are the fb `Option` guards in main.rs arms, not app files).
