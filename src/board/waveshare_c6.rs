// Board pin definitions for Waveshare ESP32-C6-Touch-AMOLED-2.06
// Source: waveshare/esp32_c6_touch_amoled_2_06 BSP component v2.0.0

// === QSPI Display (CO5300 AMOLED, 410x502 RGB565) ===
// SCLK=GPIO0, SDIO0..3=GPIO1..4, CS=GPIO5, RST=GPIO11 — wired in main.rs.
pub const LCD_WIDTH: u16 = 410;
pub const LCD_HEIGHT: u16 = 502;
pub const LCD_COL_OFFSET: u16 = 22;
pub const LCD_ROW_OFFSET: u16 = 0;

// === I2C Bus (SDA=GPIO8, SCL=GPIO7) ===
pub const I2C_FREQ_HZ: u32 = 400_000;

// === Touch (FT3168, INT=GPIO15, RST=GPIO10) ===
pub const TP_I2C_ADDR: u8 = 0x38;

// === IMU (QMI8658) ===
pub const IMU_I2C_ADDR: u8 = 0x6B;

// === RTC (PCF85063) ===
pub const RTC_I2C_ADDR: u8 = 0x51;

// === Audio (ES8311 codec over I2S) ===
// MCLK=GPIO19, SCLK=GPIO20, LRCK=GPIO22,
// ES8311 ASDOUT (ADC/mic data out, codec→SoC) = GPIO21  → SoC I2S RX DIN
// ES8311 DSDIN  (DAC data in,  SoC→codec)      = GPIO23  → SoC I2S TX DOUT
// (Per the V1.0 schematic page-1 pin table: I2S_ASDOUT=GPIO21, I2S_DSDIN=GPIO23.
// The old "DAC in=21/ADC out=23" was SWAPPED — reading GPIO23 for the mic got the
// playback line, hence exact-zero capture.)
// speaker amp enable=GPIO6 (keep LOW unless playing audio).

/// UI hit-geometry for THIS board's layout set (`ui/slint/`, 410x502 portrait).
///
/// These exist because Slint's event dispatch is dead while `play_chapter`
/// parks the main loop — the mid-playback touch path hit-tests raw panel
/// coordinates in Rust (main.rs), and hardcoding them there is how the numbers
/// silently diverge from the .slint the moment a layout moves. Every rect here
/// MUST mirror its `ui/slint/story.slint` tile exactly; the layout set and this
/// module change together or not at all. The CYD board carries its own values
/// for its own layout.
/// Lines the renderer stages before a flush — **2 on this board**.
///
/// The CO5300's CASET/RASET windows must be even-aligned on both axes
/// (datasheet §7.5.21/§7.5.22), so the flusher stages an even/odd row PAIR and
/// writes it as one `[x0, y_even, w, 2]` window. That constraint is this panel's
/// alone; see the CYD board module for the counterpart.
pub const FLUSH_STRIP_LINES: usize = 2;

pub mod ui {
    /// story READ page, PAUSE tile: x0, x1, y0, y1 (inclusive band).
    pub const STORY_PAUSE_RECT: (u16, u16, u16, u16) = (22, 198, 378, 438);

    /// Switcher card stack (#31) — MUST match `ui/slint/switcher.slint`
    /// (slot i spans y `TOP + i*PITCH .. + H`).
    pub const SWITCHER_CARD_TOP: u16 = 110;
    pub const SWITCHER_CARD_H: u16 = 84;
    pub const SWITCHER_CARD_PITCH: u16 = 96;
    /// Visible card slots (the suspension list may be longer; overlay shows "+N").
    pub const SWITCHER_CARDS: usize = 4;

    /// Shade card stack (#32) — MUST match `ui/slint/shade.slint`.
    pub const SHADE_CARD_TOP: u16 = 76;
    pub const SHADE_CARD_H: u16 = 84;
    pub const SHADE_CARD_PITCH: u16 = 92;
    /// Visible shade cards (the ring holds up to 8; overlay shows "+N").
    pub const SHADE_CARDS: usize = 4;
    /// Bottom edge-swipe band: a touch starting at y >= this is an edge gesture
    /// (swipe-up = launcher, hold = switcher). 85 % of the 502 px panel.
    pub const EDGE_BOTTOM_Y: u16 = 427;
    /// Top edge-swipe band: a touch starting at y <= this is an edge gesture
    /// (swipe-down = shade).
    pub const EDGE_TOP_Y: u16 = 75;
    /// Max travel still counted as a hold rather than a drag.
    ///
    /// Invariant, and it is the invariant rather than the number that ports:
    /// this must stay UNDER the swipe threshold so a **cancelled hold can still
    /// classify as the edge-swipe**. Here 24 < 36.
    pub const HOLD_SLOP_PX: u16 = 24;
    /// Minimum dominant-axis travel for a lift-off to count as a swipe.
    /// One value suffices on a near-square portrait panel: ~9 % of 410 wide and
    /// ~7 % of 502 tall. See the CYD module for why landscape needs two.
    pub const SWIPE_MIN_X: u32 = 36;
    /// See [`SWIPE_MIN_X`].
    pub const SWIPE_MIN_Y: u32 = 36;

    /// Slots per launcher page — a fixed 3x3 grid on this portrait panel. MUST
    /// match the `for slot in 9` grid + geometry in `ui/slint/launcher.slint`.
    pub const LAUNCHER_PAGE_SLOTS: usize = 9;

    /// Settings-hub section pages (`ui/slint/settings.slint` `titles` order).
    pub const SETTINGS_PAGE_COUNT: i32 = 6;

    /// y-band of the power page's brightness slider — swipes starting here are
    /// slider drags, not page switches.
    pub const SLIDER_BAND: core::ops::RangeInclusive<u16> = 330..=430;

    /// y-band of the Settings hub's DISPLAY-page brightness slider.
    ///
    /// This is the slider geometry (`settings.slint` DISPLAY page, absolute
    /// y 180..220) PLUS deliberate finger slop — 10 px above, 20 px below,
    /// because thumbs drift downward mid-drag. So `170..=240` and "the slider is
    /// 180..220" are the same fact, not a disagreement. (An earlier note here —
    /// mine — flagged them as contradicting; they do not.)
    pub const HUB_SLIDER_BAND: core::ops::RangeInclusive<u16> = 170..=240;
}

/// Short board name for the boot banner — the first line of every console
/// capture, and therefore the label every log gets filed under.
/// Waveshare ESP32-C6-Touch-AMOLED-2.06 (CO5300 410x502).
pub const BANNER: &str = "C6 AMOLED";
