// Board pin definitions for the NM-CYD-C5 (ESP32-C5 "Cheap Yellow Display").
// Source: cyd-c5 bring-up session 2026-08-24 (ESP-Claw board json; vendor demos).
// Node id 176 is smol's ALLOCATION — note the watch fleet derives node ids from
// the efuse MAC fold (sigil-id), so the allocated and derived ids must be
// reconciled before this board joins the mesh. See the branch notes.
//
// ⚠️ SCAFFOLD. Pins below are from the bring-up session's recon, not yet proven
// by a driver on this branch. The display/touch drivers land against
// `src/drivers/panel.rs`'s contract (owned by the cyd driver workstream).

// === SPI Display (ST7789, 320x240 RGB565, classic SPI — NOT quad) ===
// SCK=GPIO6, MOSI=GPIO7, MISO=GPIO2, CS=GPIO23, DC=GPIO24. Landscape-native.
// Vendor-confirmed oddities (vesper-drivers, 2026-08-24): NO reset GPIO — the
// panel is tied to SoC RESET, so init is SWRESET + 150 ms; inversion OFF
// (unusual for ST7789); BGR order; zero GRAM offsets in all rotations.
// Bus: display at 20 MHz, touch at 2.5 MHz on ONE shared SPI with per-device
// apply_config.
pub const LCD_WIDTH: u16 = 320;
pub const LCD_HEIGHT: u16 = 240;
pub const LCD_COL_OFFSET: u16 = 0;
pub const LCD_ROW_OFFSET: u16 = 0;

/// The ST7789 die's native PORTRAIT geometry (240x320) — `_DISPLAY_240x320` in
/// `Demos/MicroPython/rotations/st7789py.py:154-158`. [`LCD_WIDTH`]/
/// [`LCD_HEIGHT`] above are the *rotated* logical size the firmware draws into;
/// these two are what [`crate::drivers::Rotation::size`] rotates.
pub const PANEL_NATIVE_W: u16 = 240;
/// See [`PANEL_NATIVE_W`].
pub const PANEL_NATIVE_H: u16 = 320;

// --- SPI2 (FSPI): display + touch + SD slot, one bus -----------------------
// `Demos/Platformio/pinouts/nm-cyd-c5.h:32-35` (SS=10, MOSI=7, MISO=2, SCK=6),
// corroborated by `Demos/Arduino/libraries/TFT_eSPI/User_Setup-NM-CYD-C5.h:214-217`.
/// SPI clock. `pinouts/nm-cyd-c5.h:35`; `User_Setup-NM-CYD-C5.h:217`.
pub const PIN_SCK: u8 = 6;
/// SPI MOSI. `pinouts/nm-cyd-c5.h:34`; `User_Setup-NM-CYD-C5.h:215`.
pub const PIN_MOSI: u8 = 7;
/// SPI MISO — the XPT2046 needs it; the panel is write-only.
/// `pinouts/nm-cyd-c5.h:33`; `User_Setup-NM-CYD-C5.h:214`.
pub const PIN_MISO: u8 = 2;
/// Display chip select. `User_Setup-NM-CYD-C5.h:216`; `connections.md:9`.
pub const PIN_LCD_CS: u8 = 23;
/// Display data/command select. `User_Setup-NM-CYD-C5.h:218`;
/// `pins_arduino.h:93`; `platformio.ini:39`; `connections.md:9`.
pub const PIN_LCD_DC: u8 = 24;
/// Touch chip select. `User_Setup-NM-CYD-C5.h:232`; `pins_arduino.h:98`;
/// `platformio.ini:44`; `connections.md:20`.
pub const PIN_TOUCH_CS: u8 = 1;

/// SD-card chip select — **PARK HIGH, always.**
///
/// `pinouts/nm-cyd-c5.h:32`. The SD slot shares the SPI bus, and a floating SD
/// CS makes the card answer every clock edge a display transaction generates,
/// corrupting it. [`crate::drivers::spi_bus::SharedSpiBus::new`] takes this pin
/// for exactly that reason and drives it high before the first display byte —
/// whether or not a card is ever fitted, because "no card" does not mean "not
/// floating".
///
/// This is a hazard the C6 does not have: its panel owns a dedicated QSPI bus.
pub const PIN_SD_CS: u8 = 10;

// === Backlight (plain GPIO, no PWM requirement) ===
/// `User_Setup-NM-CYD-C5.h:134`; `pins_arduino.h:91`; `platformio.ini:37`.
///
/// ⚠️ Plain on/off. The driver's `set_brightness` is 0 = off, non-zero = on, so
/// the firmware's intermediate steps (`0x18` for the AOD dim at main.rs:2982)
/// land as FULL brightness on this board. The off path is correct, so the
/// degradation is in the harmless direction. Real dimming needs an LEDC channel
/// here — the vendor's `interface.cpp:85` uses `analogWrite`, i.e. a soft LEDC
/// channel — which the display driver deliberately does not claim so the
/// firmware keeps the timer peripheral. TODO: LEDC PWM, then delete this note.
pub const PIN_BACKLIGHT: u8 = 25;

// ---------------------------------------------------------------------------
// THERE IS NO DISPLAY RESET GPIO.
// ---------------------------------------------------------------------------
// `User_Setup-NM-CYD-C5.h:220`, `pins_arduino.h:92`, `platformio.ini:38` all say
// `TFT_RST = -1`, and `connections.md:9`'s TFT_RST column for the Display row
// reads literally "C5 RST": the panel's reset is tied to the SoC's own.
// St7789Display therefore takes `Option<Output>` and is passed `None`, issuing a
// software SWRESET + 150 ms instead.
//
// ⚠️ FOOTGUN: `Demos/MicroPython/rotations/tft_config.py` constructs the driver
// with `reset=Pin(0, Pin.OUT)`. That is NOT the panel reset — `st7789py.ST7789`
// merely *requires* a `reset=` argument, and GPIO0 is documented FREE in the same
// repo (`connections.md:15`). Porting that line drives an unconnected pin and
// yields a panel that is never reset, which presents exactly like a wiring fault.

// === Touch (XPT2046 resistive, SHARED SPI bus with the display, own CS) ===
// Resistive: needs calibration + debounce; pressure threshold replaces the
// FT3168's finger count.
//
// ⚠️ CORRECTED 2026-08-24 (glass). This block used to read "POLL-ONLY — no IRQ
// line is wired, so there is no touch interrupt to arm". That was a wrong
// inference from a true observation: every vendor software config really does
// say `-DCYD28_TouchR_IRQ=-1` and `connections.md`'s touch table really does
// leave the IRQ column empty — but the schematic routes XPT2046 pin 11 PENIRQ#
// → net TP_IRQ → **GPIO3** with a 10k pull-up, and `smoke.rs` stage 6 observed
// that pin LOW on every pressed sample and HIGH idle. The vendor simply chose
// to poll. See PIN_TOUCH_IRQ below.
//
// The general shape, since it recurs: a vendor's *configuration* tells you what
// they did, never what the board can do. Only the schematic and the glass do.
pub const PIN_TOUCH_IRQ: u8 = 3;

// === BOOT / wake button — ⚠️ GPIO9 IS WRONG ON THIS CHIP ===
//
// The C6 firmware reads its BOOT button on GPIO9. That is the C3/C6 download-boot
// convention and it does NOT hold on the C5 — and it fails in the worst way,
// because **GPIO9 exists on the C5, so the wrong pin compiles silently.**
//
// Two independent reasons it is wrong (nebula, 2026-08-25):
//   * ESP32-C5 datasheet §3 Table 3-1 puts chip boot mode on GPIO26/27/28, not 9.
//   * On THIS board GPIO9 is already occupied several times over —
//     `pins_arduino.h` has `RXLED 9` and `CC1101_SS_PIN 9`; `connections.md`
//     lists I2C SDA = 9, IR RX = 9, 433 RX = 9. Reading it yields bus traffic,
//     not a button.
//
// The vendor names a different pin outright, `pins_arduino.h:115-116`:
//     #define BTN_ACT LOW
//     #define DEEPSLEEP_WAKEUP_PIN 0        // GPIO0, active LOW
//
// And GPIO0 is the defensible choice for a further reason the vendor may have
// intended: **LP-capable pins on the C5 are GPIO0-GPIO6 only** (esp-metadata
// `esp32c5/gpio.toml` — pins 7..28 carry no `lp` mapping at all). So GPIO0 can
// wake from deep sleep and GPIO9/GPIO28 provably cannot, whatever else is true.
//
// ⚠️ NOT YET SETTLED PHYSICALLY: whether a tactile BOOT button is even fitted.
// The schematic has five switch designators (S1-S5) but its text layer ties none
// of them to BOOT0/IO0, and `BOOT0` also appears in the CH340C auto-download
// circuit — so it may be that circuit ONLY. `nm-cyd-c5.ini` has every navigation
// button define commented out with `HAS_TOUCH=1`, i.e. this board looks
// touch-only by design. Settle it with one minute of hardware (input-pull-up on
// GPIO0, print the level, press everything) before any wake path depends on it.
//
// Not defined as a constant yet on purpose: nothing on this board should claim a
// BOOT pin until that press test says which pin, if any, is real. Douse is soft
// (screen + radios off, tap to wake) so nothing needs it today.

// === WS2812 status LED ===
pub const WS2812_GPIO: u8 = 27;

// === Flash / PSRAM ===
// 16 MB flash, 8 MB PSRAM. The C6 watch is ALSO 16 MB flash (partitions.csv's
// last partition ends at 0xC20000 — an earlier draft of this comment said 4 MB,
// which was the C6's ROM-REGION ceiling before widen_rom_region, not its flash
// size). So the OTA layout ports UNCHANGED: same partitions.csv, same 6 MB A/B
// slots, no board variant needed. What does NOT port is the C6's heap story —
// PSRAM changes it entirely, and the reclaimed-pool scarcity and the
// 256-SceneTexture ceiling are C6 MEASUREMENTS that must not be inherited
// (the same measured-never-inherited rule as the stack floor).

// === Constants with LIVE call sites in main.rs (compile-time requirement) ===
// The CYD has no I2C peripherals in use — touch is SPI, there is no PMU, IMU or
// RTC chip. These exist because main.rs's bring-up references them
// unconditionally until the capability-gating pass lands; the first-boot plan
// (mapper §5) satisfies the I2C drivers with a fake-bus shim, so these values
// configure a bus that talks to NOTHING. They must never be read as hardware
// facts about this board.
pub const I2C_FREQ_HZ: u32 = 400_000;
pub const TP_I2C_ADDR: u8 = 0x38;
pub const IMU_I2C_ADDR: u8 = 0x6B;
pub const RTC_I2C_ADDR: u8 = 0x51;

// ===========================================================================
// Panel electrical facts. Every one of these is GLASS-VERIFIED (2026-08-24) or
// cited to the vendor repo; full derivations in
// ~/Projects/cyd-c5/watch-port/src/board.rs, which is where they were settled.
// ===========================================================================

/// Default orientation: **inverted landscape**, MADCTL `0xA0` (MV|MY), plus the
/// BGR bit → **`0xA8` on the wire**.
///
/// The vendor ships rotation 3 (`platformio.ini:29`, `nm-cyd-c5.ini:25`), and
/// both vendor stacks agree on what "3" means: TFT_eSPI's upstream
/// `ST7789_Rotation.h` case 3 emits `MV|MY` (`0x20|0x80`), identical to the
/// MicroPython table's fourth entry.
///
/// ⚠️ If the image reads upside down that does NOT make this constant wrong —
/// `0xA0` is provably the vendor's orientation. It would mean the board is being
/// *viewed* 180° from the vendor's intended mounting, and switching to
/// `Landscape` is then a product decision about which way up JP wants it, not a
/// bug fix. Worth naming, because the one-line change looks identical either way.
pub const DEFAULT_ROTATION: crate::drivers::Rotation =
    crate::drivers::Rotation::LandscapeInverted;

/// MADCTL colour-order bit: **BGR (0x08) — CONFIRMED ON GLASS 2026-08-24** (JP:
/// colours normal, no red/blue swap).
///
/// Primary source: `st7789py.py:271` defaults to `color_order=BGR` and
/// `tft_config.py` does not override it.
///
/// ⚠️ The two vendor stacks DISAGREE here. TFT_eSPI's `ST7789_Defines.h:82-94`
/// defaults to BGR only when `CGRAM_OFFSET` is defined — which is for the offset
/// panel variants, not a full 240x320 die like this one — so it would most
/// likely have selected RGB. The conclusion stands because glass beats both, but
/// anyone re-deriving this from TFT_eSPI alone would find the opposite and
/// reasonably conclude this constant is a bug. If red and blue ever appear
/// swapped, set `0x00`.
pub const MADCTL_COLOR_ORDER: u8 = 0x08;

/// Display inversion: **OFF**.
///
/// Stated loudly because most ST7789 IPS modules need `INVON`, so this is a
/// genuine board fact rather than an inherited default — four sources agree:
/// `User_Setup-NM-CYD-C5.h:119` (`TFT_INVERSION_OFF`), `tft_config.py`'s init
/// table (`0x20` = disable inversion), `platformio.ini:45` (`-DTFT_IPS=0`), and
/// glass. **Do not send `INVON` (0x21).**
pub const INVERT_COLORS: bool = false;

/// Display SPI clock. `User_Setup-NM-CYD-C5.h:368` → `SPI_FREQUENCY 20000000`.
///
/// ⚠️ **20 MHz is already an overclock, not a conservative default** — the
/// opposite of the ecosystem folklore that "CYD boards commonly run 40-80 MHz,
/// so raising this is a free later optimisation". ST7789V datasheet v1.6 §7.4.3
/// (4-line serial, which is this board) gives `TSCYCW >= 66 ns` → **15.15 MHz**
/// for writes, while `TSHW`/`TSLW >= 15 ns` implies 33.3 MHz at 50 % duty. The
/// datasheet contradicts itself, and that gap IS the whole budget:
///
///   * **<= 15 MHz** — fully in spec.
///   * **15-33 MHz** — violates cycle time only; pulse widths still have margin.
///     The vendor's 20 MHz lives here. Low risk.
///   * **> 33 MHz** — the half-cycle drops under the 15 ns pulse-width floor. A
///     harder class of violation: pulse width governs the controller's internal
///     sampling, and it fails with temperature and unit spread rather than
///     reproducibly — i.e. it passes a smoke test and dies in a warm room.
///
/// Aggravated here by the bus being SHARED (the ST7789, XPT2046 and SD slot all
/// load MISO, against datasheet timings that assume CL <= 30 pF).
///
/// ★ And clock cannot rescue a full repaint at ANY legal rate: a frame is
/// `320*240*2` = 153,600 B, so against a 30 ms budget 20 MHz needs 61.4 ms,
/// 33 MHz needs 37.2 ms, and even an out-of-bounds 40 MHz needs 30.7 ms. **There
/// is no legal clock at which a full-screen repaint fits.** Dirty-rectangle
/// partial repaint is therefore mandatory rather than an optimisation — which is
/// exactly what the Slint renderer + [`crate::ui::slint_platform`]'s flusher do.
/// **26 MHz as of image 10b** (was 20). Still inside the documented 15-33 MHz
/// band above — cycle time violated, pulse widths with margin — so this is the
/// same risk class the vendor's 20 MHz already occupied, not a new one.
///
/// ⚠️ 26, NOT 33, and the reason is the DIVIDER not the datasheet. The SPI clock
/// is an integer division of the 80 MHz source, so the achievable rates near the
/// ceiling are:
///
/// ```text
///   80/2 = 40.00 MHz   OUT OF BAND — half-cycle under the 15 ns pulse floor
///   80/3 = 26.67 MHz   highest IN-BAND rate that exists
///   80/4 = 20.00 MHz   the old value
/// ```
///
/// There is no 33 MHz. Asking for 33 either rounds down to 26.67 (harmless but
/// misleading in the source) or rounds up to 40 (out of spec, silently). 26 can
/// only resolve to 26.67 or below, so it cannot overshoot.
///
/// Full frame 320*240*2 = 153,600 B: 20 MHz = 61.4 ms, **26.67 MHz = 46.1 ms**
/// (1.33x). Separate commit from the repaint fixes so it reverts alone if JP sees
/// tearing or corruption on a page turn.
pub const SPI_DISPLAY_HZ: u32 = 26_000_000;

/// Max SPI clock for *reading* ST7789 registers — 6.67 MHz (`TSCYCR >= 150 ns`).
///
/// Unused: this driver is write-only to the panel. Kept as a tripwire, because
/// the vendor config sets `SPI_READ_FREQUENCY 20000000` (3x over the read spec).
/// If anyone adds register readback and gets garbage, drop to this rate before
/// suspecting wiring — and do NOT assume the write-path overclock argument
/// transfers: the read path's cycle-vs-pulse-width slack is 1.25x, not 2.20x.
pub const SPI_DISPLAY_READ_MAX_HZ: u32 = 6_670_000;

/// Touch SPI clock. The vendor's 2.5 MHz (`User_Setup-NM-CYD-C5.h:374`) is
/// already at the XPT2046's ~2 MHz settling edge, so we sit just under it.
///
/// [`crate::drivers::spi_bus::SharedSpiBus`] re-tunes per device: running the
/// touch chip at the display's 20 MHz does not error, it returns *plausible*
/// garbage — the worst failure mode available.
pub const SPI_TOUCH_HZ: u32 = 2_000_000;

/// Touch calibration — vendor span, **axis settled on glass 2026-08-24**.
///
/// Span from `platformio.ini:50-53`. The vendor repo contains a second,
/// conflicting calibration (`interface.cpp:55`), and neither is authoritative —
/// resistive panels vary unit to unit, which is why two of the vendor's own
/// builds disagree. The wider span is used deliberately: clamping is lossless,
/// extrapolating is not.
///
/// # The frame these are expressed in
///
/// This describes raw → the panel's **mechanical** frame: the short (240 px)
/// axis and the long (320 px) axis of the bare die. It is a wiring fact and
/// nothing else — it does not change when the display is rotated, and
/// `Xpt2046::map` applies rotation separately from the same MADCTL semantics the
/// display driver uses.
///
/// ⚠️ An earlier shape carried `swap_xy` + two `invert_raw_*` flags whose
/// documented frame disagreed with their arithmetic. That hid a real bug:
/// `swap_xy` was silently performing the *rotation's* transpose, so
/// `set_rotation(Portrait)` had nothing to supply one and produced garbage —
/// which matters here because the watch firmware does call `set_rotation`.
/// Splitting "how is the film wired" from "how is the panel rotated" fixes both.
///
/// ★ **`invert_long: true` — exactly one inversion, solved not assumed.**
/// The decisive datum was a single press with finger and dot visible in the SAME
/// view at the same moment: physical top-right read `raw(3272, 459)`. Under the
/// LandscapeInverted transform `(x,y) = (g, MAX-s)` that requires `g = 3854`,
/// `s = 3598`; `norm(rawX=3272) = 3596` → short uninverted, `norm(rawY=459) =
/// 241` and `4095-241 = 3854` → long inverted.
///
/// An earlier corner set (`'TL' raw(464,363)`, `'TR' raw(485,3685)`,
/// `'BR' raw(3539,3570)`) is NOT wrong data — it is correct data whose LABELS
/// were 180° rotated; under these settings each lands on the corner diagonally
/// opposite its label, and both datasets then agree exactly.
///
/// ⚠️ The lesson, because it will recur: remembered corner LABELS carry a frame
/// that can silently rotate between sessions, while a finger-and-dot-in-one-
/// glance observation carries no frame at all. **When two touch datasets
/// disagree by exactly 180°, suspect the labels before the hardware or the
/// arithmetic.** The boot self-test in `main.rs` asserts the anchor above, so a
/// future edit to these flags reports itself on the console rather than in
/// someone's fingers.
pub const TOUCH_CAL: crate::drivers::xpt2046::Calibration =
    crate::drivers::xpt2046::Calibration {
        x_min: 185,
        x_max: 3700,
        y_min: 250,
        y_max: 3800,
        // The film's channel X runs along the die's SHORT (240 px) axis — the
        // NATURAL pairing, no transpose in the calibration at all.
        short_axis: crate::drivers::xpt2046::RawAxis::X,
        invert_short: false,
        invert_long: true,
    };

/// Minimum XPT2046 pressure (`z1 + 4095 - z2`) to count as a touch.
///
/// **100 — a measurement, not a preference** (vesper, driver owner, image 9).
///
/// ```text
///   noise ceiling      z <= 30           (idle, many samples, spike-free)
///   ---- 100 ----                        3.3x over noise
///   rejected REALS     213 274 360 365 372
///   confirmed contact  z_min 478..534    (truncated gestures)
/// ```
///
/// The old 400 sat above every one of those rejected reals. It looked
/// exonerated by the first capture — which showed rejects at z<=30 and contact
/// at 482..2341, a wide empty band — but that capture could only contain
/// gestures that REGISTERED. JP's controlled count settled it: ~20 swipes
/// produced 8 lifts. Twelve gestures were entirely invisible, and a threshold
/// too high is invisible **by construction** — it yields no samples, so it
/// appears in no gesture record. Survivorship bias, and it hid this for an
/// entire image cycle.
///
/// ★ Why the LOW end of the 100-150 range: the two failure directions are not
/// symmetric in observability. Too low self-reports — phantom taps show up in
/// telemetry as short-travel events with z in 30..100. Too high reports
/// nothing at all. Prefer the error you can see. If phantoms appear, 150 is a
/// one-line retreat *with data*; the reverse mistake costs another cycle of
/// invisibility.
///
/// Note the rejected reals are all NONZERO — they cleared the `z1 == 0`
/// bridge-open early-return and were killed by this gate, not by the floor. So
/// brush-weight contact provably produces sub-400 z on this panel, and lowering
/// the gate necessarily recovers it. (If a future capture shows high `open=`
/// with low `rej=`, the remaining losses are open-bridge reads instead — a
/// different failure, fixed in the driver's PD bits, not here.)
pub const TOUCH_PRESSURE_THRESHOLD: u16 = 100;

/// Lines the renderer stages before a flush — **1 on this board**.
///
/// The C6's CO5300 requires even-aligned 2x2 windows, so it stages row PAIRS.
/// The ST7789 accepts a 1-pixel-tall window, which is the single assumption the
/// C6's flusher could not make, so the CYD runs
/// [`crate::ui::slint_platform::SingleLineFlusher`]: one window per line, no
/// staging, no pairing contract, and therefore no violation class. It also
/// halves the strip buffers, which this constant sizes.
pub const FLUSH_STRIP_LINES: usize = 1;

/// UI hit-geometry for the CYD layout set (`ui/cyd/`, 320x240 landscape).
/// PLACEHOLDER values pending the layout work — they mirror nothing yet, and
/// the C5 arm's story playback is gated off until they do.
pub mod ui {
    /// PLACEHOLDER — `(0,0,0,0)` makes the hit test `x>=0 && x<=0 && y>=0 &&
    /// y<=0`, i.e. only pixel (0,0), which effectively gates story playback off
    /// until the story page is laid out. A safe default by construction.
    pub const STORY_PAUSE_RECT: (u16, u16, u16, u16) = (0, 0, 0, 0);

    /// Switcher card stack (#31) — MUST match `ui/cyd/switcher.slint`
    /// (landscape 320x240; geometry from that file's port header, landed at
    /// 42dd687). Slot i spans y `TOP + i*PITCH .. + H`.
    pub const SWITCHER_CARD_TOP: u16 = 40;
    pub const SWITCHER_CARD_H: u16 = 52;
    pub const SWITCHER_CARD_PITCH: u16 = 58;
    pub const SWITCHER_CARDS: usize = 3;

    /// Shade card stack (#32) — MUST match `ui/cyd/shade.slint`.
    pub const SHADE_CARD_TOP: u16 = 38;
    pub const SHADE_CARD_H: u16 = 60;
    pub const SHADE_CARD_PITCH: u16 = 66;
    pub const SHADE_CARDS: usize = 3;
    /// Bottom edge-swipe band — bottom 15 % of the panel, matching the C6's
    /// proportion (`427/502 = 85 %`, `204/240 = 85 %`).
    ///
    /// ⚠️ The C6's 427 is entirely OFF a 240 px panel, which would leave
    /// bottom-edge swipe-up (launcher) and hold-to-switcher **unreachable** —
    /// not degraded, unreachable. Proposed by luna (layout workstream) and
    /// applied here pending watch-session review.
    pub const EDGE_BOTTOM_Y: u16 = 204;
    /// Top edge-swipe band — top 18 %.
    ///
    /// Deliberately not the strict 15 % (36): 44 is exactly the chrome band's
    /// hit height, so "the top 44 px is edge territory" is ONE number instead of
    /// two that nearly agree. The C6 pair had the same relationship and works,
    /// because swipes are classified in Rust from the touch driver rather than
    /// by Slint hit-testing — a tap on a chip in that band stays a tap.
    pub const EDGE_TOP_Y: u16 = 44;
    /// Max travel still counted as a hold. Must stay under the SMALLER of the
    /// two swipe thresholds below — see [`SWIPE_MIN_Y`].
    pub const HOLD_SLOP_PX: u16 = 18;
    /// Minimum horizontal travel for a swipe — 10 % of 320.
    ///
    /// ★ A landscape panel needs TWO thresholds where the C6 needed one. The
    /// C6's single `SWIPE_MIN = 36` is documented as "~10 % of the 410px panel";
    /// carried over unchanged it would be 11 % horizontally but **15 %
    /// vertically** on this panel, making up/down swipes markedly harder to
    /// trigger than left/right. Splitting the constant preserves the *intent*
    /// (10 % of the axis travelled) rather than the number.
    pub const SWIPE_MIN_X: u32 = 32;
    /// Minimum vertical travel for a swipe — 10 % of 240. [`HOLD_SLOP_PX`] (18)
    /// is kept below this, which is the C6's invariant restated for the axis
    /// that binds here.
    pub const SWIPE_MIN_Y: u32 = 24;

    /// Slots per launcher page — **8**, a 4x2 landscape grid.
    ///
    /// ⚠️ TWO-SIDED with `Geom.launcher-slots` in `ui/cyd/launcher.slint` and
    /// with the `page * slots + slot` indexing in `slint_shell.rs`. Change one
    /// half only and tapping app N launches app M, silently.
    ///
    /// Luna's derivation, kept because the binding constraint is not the obvious
    /// one: with Voice, Sound and Maze dropped the registry sections are
    /// **GAMES 6 · SYSTEM 7 · AUDIO 1**, and 8 slots gives exactly 3 pages with
    /// no section split. The section that binds is **SYSTEM at 7**, not GAMES —
    /// so this breaks if SYSTEM grows past 8, and dropping another game will not
    /// save it. Re-verified after Maze was dropped: still 8.
    pub const LAUNCHER_PAGE_SLOTS: usize = 8;

    /// Settings-hub section pages — **5**, one fewer than the C6: the SOUND page
    /// is dropped with the audio hardware.
    ///
    /// This one is in the VISIBLE-failure class, unlike the slot-inverse
    /// constants: too high and the hub pages to a blank screen, which reports
    /// itself. Safe to change; still worth getting right.
    pub const SETTINGS_PAGE_COUNT: i32 = 5;

    /// **Retired on this board — a deliberately never-matching range.**
    ///
    /// `1..=0` is empty, so `contains()` is always false and every horizontal
    /// swipe on the power page is a page switch rather than a slider drag. That
    /// is correct here because the CYD's backlight is a TOGGLE, not a slider —
    /// there is no drag for the band to protect.
    ///
    /// ⚠️ Retiring it is not the same as leaving the C6 value. At `330..=430` the
    /// band is entirely off a 240 px panel, so it would never match either — but
    /// by accident, and it would silently come alive the moment anyone "fixed"
    /// the number to something on-panel. An empty range states the intent.
    pub const SLIDER_BAND: core::ops::RangeInclusive<u16> = 1..=0;

    /// **Retired on this board**, same reasoning as [`SLIDER_BAND`].
    ///
    /// ⚠️ Here the stale-value hazard is not hypothetical: the C6's upper bound
    /// (240) was that panel's EDGE, but on this panel 240 IS the edge — so
    /// carrying `170..=240` across would swallow **every** horizontal swipe in
    /// the lower third of the Settings hub. An empty range cannot.
    ///
    /// Retiring this also makes `HUB_PAGE_DISPLAY` unreachable on this board,
    /// which is why that constant stays file-scoped rather than gaining a CYD
    /// value it could never be consulted for.
    pub const HUB_SLIDER_BAND: core::ops::RangeInclusive<u16> = 1..=0;
}

// === Soft-douse contract (BINDING — set by the shipped power-menu caption) ===
// The CYD power menu reads: "screen and radios off · tap the glass to wake"
// (with a drawn tap-mark — the one caption a user reads to UNDO something).
// The Rust that implements soft douse (no deep sleep on this board: esp-hal
// 1.1.1 is radio XOR sleep) must therefore:
//   (a) wake on the TOUCH IRQ (XPT2046 /IRQ on GPIO3 — glass-verified), and
//   (b) bring the radios back WITH the screen — no further user action.
// If the relight path cannot restore radios, the caption changes BEFORE this
// firmware ships, not after. Whoever lands douse owns keeping that sentence
// true.

/// Short board name for the boot banner — the first line of every console
/// capture, and therefore the label every log gets filed under.
/// RockBase NM-CYD-C5 (ST7789 320x240 landscape + XPT2046).
pub const BANNER: &str = "C5 CYD";

/// `chip_id` in the esp-idf app-image header (LE u16 at bytes 12..14) for this
/// board's SoC — ESP32-C5 = 0x0017.
///
/// MEASURED from real images built by `espflash save-image`, not taken from a
/// table: both arms' images start with the same 0xE9 magic, so this is the first
/// byte pair that actually distinguishes them. `ota_http` refuses a mismatch
/// before the first flash write.
pub const ESP_IMAGE_CHIP_ID: u16 = 0x0017;
