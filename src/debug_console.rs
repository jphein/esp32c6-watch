//! Debug console — **UI test automator** (cargo feature `debug-console`).
//!
//! Lets a HOST script drive + measure the watch UI over the USB-Serial-JTAG,
//! replacing the manual flash-and-glass-test loop. The host sends newline-
//! delimited text commands; this task injects them as *synthetic input on the
//! exact same path as real touch* and reports UI state + per-frame render
//! timing. See `tools/ui_test.py`.
//!
//! ## Sharing the USB-Serial-JTAG with `println!` (esp-println)
//!
//! esp-println is the firmware's stdout. On the C6 it writes the USB-Serial-JTAG
//! **TX FIFO** directly via raw MMIO (`0x6000_F000`/`_F004`) under its own
//! critical section — it never owns the esp-hal peripheral, never uses
//! interrupts, and never touches the RX side. So we take the esp-hal
//! [`UsbSerialJtag`] peripheral here in **async, RX-only** mode: we `.read()`
//! incoming bytes (RX FIFO + the SERIAL_OUT_RECV_PKT interrupt) and we NEVER
//! call the HAL's TX — all our output goes back through `println!`. The two
//! coexist because their concerns are disjoint (esp-println = TX FIFO + wr_done
//! trigger, polled; console = RX FIFO + RX-packet IRQ); the only shared register
//! is `ep1_conf`, where esp-println writes a trigger bit and we read a status
//! bit — both single 32-bit accesses, non-destructive. `UsbSerialJtag::new`
//! deliberately does NOT reset the peripheral (that would drop the USB link).
//!
//! ## Commands (each reply is one line prefixed `[DBGCON] ` so the host can
//! parse deterministically):
//!   - `tap <x> <y>`      — synthesise a press+release click at (x,y)
//!   - `swipe up|down|left|right` — a navigation swipe
//!   - `launch <idx>`     — raise the app at registry index <idx>
//!   - `home`             — return to the watchface
//!   - `state`            — print AppState + key UI flags
//!   - `perf`             — print the last-N render-frame durations (µs)
//!   - `beep`             — play the 800 Hz/50 ms test tone on the shared TX
//!                          ring (#23) — validates playback AND that the mic
//!                          still captures afterwards (run `launch` Sound next)
//!   - `ping` / `help`

use core::cell::RefCell;
use core::fmt::Write as _;

use critical_section::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use embedded_io_async::Read as _;
use esp_hal::usb_serial_jtag::UsbSerialJtag;
use esp_hal::Async;
use esp_println::println;

use crate::apps::registry;
use crate::apps::AppState;
use crate::peripherals::touch::{SwipeDirection, TouchPoint};

// ============================================================================
// Synthetic input queue (console task -> main loop)
// ============================================================================

/// One synthetic input event. Drained one-per-tick by the main loop and merged
/// into the SAME `touch_point` / `swipe_event` variables that real touch feeds,
/// so `shell.handle_touch(...)` (and framebuffer `AppInput`) cannot tell the
/// difference. Zero overhead when the queue is empty.
#[derive(Clone, Copy)]
pub enum Inject {
    /// A raw touch frame. A click is two frames: a press (`point: Some`,
    /// `tap: false`) then a release (`point: None`, `swipe: Some(Tap)`,
    /// `tap: true`) — matching how real touch synthesises press→release across
    /// ticks. A navigation swipe is a single frame (`point: None`, `swipe:
    /// Some(dir)`); `handle_touch` drives paging off the swipe arg alone.
    Touch {
        point: Option<TouchPoint>,
        swipe: Option<SwipeDirection>,
        start_y: u16,
        tap: bool,
    },
    /// Raise the app at this registry index (same cell the launcher tile tap
    /// sets: `shell.req.launch`).
    Launch(usize),
    /// Return to the watchface (drop any framebuffer app, close overlays).
    Home,
}

const Q_DEPTH: usize = 16;
static INJECT_Q: Channel<CriticalSectionRawMutex, Inject, Q_DEPTH> = Channel::new();
/// Wakes the main-loop select the instant a command is queued (so the loop does
/// not wait out its idle tick). Payload is `()`; the data rides `INJECT_Q`.
static INJECT_WAKE: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// Awaited by the main-loop `select` so a queued command wakes the loop at once.
pub async fn wait_inject() {
    INJECT_WAKE.wait().await;
}

/// Pop one pending synthetic input (main loop, once per tick). Re-arms the wake
/// signal if more remain, so a multi-frame gesture (tap = press+release)
/// advances on the very next tick.
pub fn take_inject() -> Option<Inject> {
    match INJECT_Q.try_receive() {
        Ok(inj) => {
            if !INJECT_Q.is_empty() {
                INJECT_WAKE.signal(());
            }
            Some(inj)
        }
        Err(_) => None,
    }
}

fn queue(inj: Inject) -> bool {
    let ok = INJECT_Q.try_send(inj).is_ok();
    if ok {
        INJECT_WAKE.signal(());
    }
    ok
}

// ============================================================================
// UI state snapshot (main loop -> console `state`)
// ============================================================================

/// Snapshot of the UI the main loop publishes each awake tick.
#[derive(Clone, Copy)]
pub struct UiState {
    pub app: AppState,
    pub page: i32,
    pub screen_state: u8,
    pub wifi: bool,
    pub ble: bool,
    pub mesh_peers: u8,
}

impl UiState {
    const fn boot() -> Self {
        UiState {
            app: AppState::Watchface,
            page: 0,
            screen_state: 3,
            wifi: false,
            ble: false,
            mesh_peers: 0,
        }
    }
}

static UI_STATE: Mutex<RefCell<UiState>> = Mutex::new(RefCell::new(UiState::boot()));

/// Publish the current UI state (main loop, once per tick).
pub fn publish_state(s: UiState) {
    critical_section::with(|cs| *UI_STATE.borrow(cs).borrow_mut() = s);
}

// ============================================================================
// Render-frame perf ring (main loop -> console `perf`)
// ============================================================================

const PERF_N: usize = 32;

struct PerfRing {
    us: [u32; PERF_N],
    head: usize,
    /// Total frames recorded (saturating); `min(count, PERF_N)` are valid.
    count: u32,
}

static PERF: Mutex<RefCell<PerfRing>> = Mutex::new(RefCell::new(PerfRing {
    us: [0; PERF_N],
    head: 0,
    count: 0,
}));

/// Record one `shell.render(&mut display)` duration in microseconds (main loop).
/// This is the responsiveness metric that would have caught the theme-slow +
/// launcher-scroll regressions.
pub fn record_frame(micros: u32) {
    critical_section::with(|cs| {
        let mut r = PERF.borrow(cs).borrow_mut();
        let h = r.head;
        r.us[h] = micros;
        r.head = (h + 1) % PERF_N;
        r.count = r.count.saturating_add(1);
    });
}

// ============================================================================
// Console task
// ============================================================================

/// The debug-console task: read newline-delimited commands from the
/// USB-Serial-JTAG RX, echo each command's result on the SAME serial (via
/// `println!`), and inject synthetic input / report state.
#[embassy_executor::task]
pub async fn debug_console_task(mut usb: UsbSerialJtag<'static, Async>) -> ! {
    println!("[DBGCON] ready");
    let mut line = [0u8; 96];
    let mut len = 0usize;
    let mut buf = [0u8; 32];
    loop {
        let n = usb.read(&mut buf).await.unwrap_or(0);
        for &b in &buf[..n] {
            match b {
                b'\n' | b'\r' => {
                    if len > 0 {
                        handle_line(&line[..len]);
                        len = 0;
                    }
                }
                _ => {
                    if len < line.len() {
                        line[len] = b;
                        len += 1;
                    } else {
                        // Overflow: drop the line so parsing stays deterministic.
                        len = 0;
                        println!("[DBGCON] err line-too-long");
                    }
                }
            }
        }
    }
}

fn handle_line(bytes: &[u8]) {
    let s = match core::str::from_utf8(bytes) {
        Ok(s) => s.trim(),
        Err(_) => {
            println!("[DBGCON] err non-utf8");
            return;
        }
    };
    if s.is_empty() {
        return;
    }
    let mut it = s.split_ascii_whitespace();
    let cmd = it.next().unwrap_or("");
    match cmd {
        "tap" => {
            let x = it.next().and_then(|v| v.parse::<u16>().ok());
            let y = it.next().and_then(|v| v.parse::<u16>().ok());
            match (x, y) {
                (Some(x), Some(y)) => {
                    // Press then release -> a click at (x,y). handle_touch turns
                    // the press into PointerPressed and the release (at the same
                    // last_pos, direction Tap so it is not treated as a nav
                    // swipe) into PointerReleased == a click.
                    let press = Inject::Touch {
                        point: Some(TouchPoint { x, y, fingers: 1 }),
                        swipe: None,
                        start_y: y,
                        tap: false,
                    };
                    let release = Inject::Touch {
                        point: None,
                        swipe: Some(SwipeDirection::Tap),
                        start_y: y,
                        tap: true,
                    };
                    if queue(press) && queue(release) {
                        println!("[DBGCON] ok tap {} {}", x, y);
                    } else {
                        println!("[DBGCON] err queue-full");
                    }
                }
                _ => println!("[DBGCON] err usage: tap <x> <y>"),
            }
        }
        "swipe" => {
            let dir = match it.next() {
                Some("up") => Some(SwipeDirection::Up),
                Some("down") => Some(SwipeDirection::Down),
                Some("left") => Some(SwipeDirection::Left),
                Some("right") => Some(SwipeDirection::Right),
                _ => None,
            };
            match dir {
                Some(d) => {
                    // Navigation is driven by the swipe arg alone (no press
                    // frame needed). start_y at screen centre avoids the
                    // power-page brightness-slider band.
                    let f = Inject::Touch {
                        point: None,
                        swipe: Some(d),
                        start_y: 206,
                        tap: false,
                    };
                    if queue(f) {
                        println!("[DBGCON] ok swipe {}", dir_name(d));
                    } else {
                        println!("[DBGCON] err queue-full");
                    }
                }
                None => println!("[DBGCON] err usage: swipe up|down|left|right"),
            }
        }
        "launch" => match it.next().and_then(|v| v.parse::<usize>().ok()) {
            Some(idx) => match registry::launch_state(idx) {
                Some(st) => {
                    if queue(Inject::Launch(idx)) {
                        println!("[DBGCON] ok launch {} ({:?})", idx, st);
                    } else {
                        println!("[DBGCON] err queue-full");
                    }
                }
                None => println!("[DBGCON] err launch: bad index {}", idx),
            },
            None => println!("[DBGCON] err usage: launch <idx>"),
        },
        "home" => {
            if queue(Inject::Home) {
                println!("[DBGCON] ok home");
            } else {
                println!("[DBGCON] err queue-full");
            }
        }
        "state" => {
            let s = critical_section::with(|cs| *UI_STATE.borrow(cs).borrow());
            println!(
                "[DBGCON] state app={:?} page={} launcher={} screen={} wifi={} ble={} mesh={}",
                s.app,
                s.page,
                (s.app == AppState::Launcher) as u8,
                s.screen_state,
                s.wifi as u8,
                s.ble as u8,
                s.mesh_peers
            );
        }
        "perf" => report_perf(),
        "beep" => {
            // On-glass playback probe (#23): synthesize the Snake beep on the
            // stack and queue it. The amp rises via the main loop's per-tick
            // service_amp — no shared peripherals needed from this task. A
            // no-op input frame is queued too: it wakes the (possibly 1 Hz /
            // parked) main loop immediately, so the amp raise is prompt.
            let mut buf = [0u8; 1600];
            let n = mic_dsp::fill_tone_mono_s16le(&mut buf, 16_000, 800, 50, 12_000, 2);
            let queued = crate::peripherals::audio_out::play_pcm(&buf[..n]);
            let _ = queue(Inject::Touch { point: None, swipe: None, start_y: 0, tap: false });
            println!("[DBGCON] ok beep ({} of {} B queued)", queued, n);
        }
        "ping" => println!("[DBGCON] ok pong"),
        "help" => println!(
            "[DBGCON] cmds: tap <x> <y> | swipe up|down|left|right | launch <idx> | home | state | perf | beep | ping"
        ),
        _ => println!("[DBGCON] err unknown: {}", cmd),
    }
}

/// Copy the ring out under a short critical section, then format + emit one line
/// OUTSIDE the CS (never hold interrupts off across a whole TX FIFO drain).
fn report_perf() {
    let (samples, count) = critical_section::with(|cs| {
        let r = PERF.borrow(cs).borrow();
        let n = core::cmp::min(r.count as usize, PERF_N);
        let mut out = [0u32; PERF_N];
        // Reorder oldest -> newest into a local copy.
        for (i, slot) in out.iter_mut().enumerate().take(n) {
            *slot = r.us[(r.head + PERF_N - n + i) % PERF_N];
        }
        (out, r.count)
    });
    let n = core::cmp::min(count as usize, PERF_N);
    let mut max = 0u32;
    let mut sum: u64 = 0;
    let mut out: heapless::String<384> = heapless::String::new();
    let _ = write!(out, "[DBGCON] perf count={} n={} frames_us=[", count, n);
    for (i, &v) in samples.iter().take(n).enumerate() {
        max = max.max(v);
        sum += v as u64;
        if i > 0 {
            let _ = out.push(',');
        }
        let _ = write!(out, "{}", v);
    }
    let avg = if n > 0 { (sum / n as u64) as u32 } else { 0 };
    let _ = write!(out, "] max_us={} avg_us={}", max, avg);
    println!("{}", out);
}

fn dir_name(d: SwipeDirection) -> &'static str {
    match d {
        SwipeDirection::Up => "up",
        SwipeDirection::Down => "down",
        SwipeDirection::Left => "left",
        SwipeDirection::Right => "right",
        SwipeDirection::Tap => "tap",
    }
}
