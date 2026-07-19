//! Live power-consumption diagnostic page.
//!
//! This is a swipeable page on the watchface (Clock → Sensors → System →
//! Power → Clock). It shows, in real time, where the current draw is going:
//! which subsystems are on, the estimated mA each contributes, the total,
//! and a rough runtime-left estimate at the current load.
//!
//! It's styled as a terminal read-out so it reads like a hardware debug tool,
//! and it's drawn from cached PowerStats without doing any extra I²C /
//! peripheral polling of its own — the numbers come from state the firmware
//! already tracks in the main loop. That way, "looking at the diagnostic"
//! doesn't meaningfully skew what's being diagnosed.
//!
//! A companion serial-dump path (one line / second on the UART) is enabled
//! by `PowerStats::serial_dump_enabled` flag for logging to a laptop.

use embedded_graphics::mono_font::ascii::{FONT_8X13, FONT_10X20};
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{PrimitiveStyle, Rectangle, RoundedRectangle};
use embedded_graphics::text::{Alignment, Text};

use crate::board;
use crate::peripherals::power_stats::{DisplayState, PowerStats, WifiMode};

const W: i32 = board::LCD_WIDTH as i32;
const H: i32 = board::LCD_HEIGHT as i32;

pub fn draw_power_page<D: DrawTarget<Color = Rgb565>>(
    d: &mut D,
    stats: &PowerStats,
) -> Result<(), D::Error> {
    // Fill black — AMOLED sub-pixels are physically off on black, so the
    // diagnostic page itself is one of the cheapest pages to display.
    Rectangle::new(Point::zero(), Size::new(W as u32, H as u32))
        .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK))
        .draw(d)?;

    let cx = W / 2;
    let title = MonoTextStyle::new(&FONT_10X20, Rgb565::CYAN);
    let label = MonoTextStyle::new(&FONT_8X13, Rgb565::CSS_GRAY);
    let value = MonoTextStyle::new(&FONT_8X13, Rgb565::WHITE);
    let green = MonoTextStyle::new(&FONT_8X13, Rgb565::GREEN);
    let red = MonoTextStyle::new(&FONT_8X13, Rgb565::RED);

    Text::with_alignment("POWER MONITOR", Point::new(cx, 38), title, Alignment::Center).draw(d)?;
    Text::with_alignment("-- live draw --", Point::new(cx, 58), label, Alignment::Center).draw(d)?;

    // Prompt-style two-column layout:
    //   left:  label       right: state / mA
    let mut y = 100;
    let left_x = 30;
    let right_x = W - 30;
    let row_h = 22;

    // --- CPU ---
    Text::new("CPU:", Point::new(left_x, y), label).draw(d)?;
    let mut cpu_buf = [0u8; 16];
    let cpu_s = fmt_mhz(&mut cpu_buf, stats.cpu_mhz, stats.base_ma());
    Text::with_alignment(cpu_s, Point::new(right_x, y), value, Alignment::Right).draw(d)?;
    y += row_h;

    // --- Display ---
    Text::new("LCD:", Point::new(left_x, y), label).draw(d)?;
    let disp_text = match stats.display {
        None | Some(DisplayState::Off) => "OFF    0mA",
        Some(DisplayState::Aod)        => "AOD    8mA",
        Some(DisplayState::Dim)        => "DIM   25mA",
        Some(DisplayState::Bright)     => "ON    70mA",
    };
    let disp_style = if matches!(stats.display, Some(DisplayState::Bright)) { yellow_inline() } else { value };
    Text::with_alignment(disp_text, Point::new(right_x, y), disp_style, Alignment::Right).draw(d)?;
    y += row_h;

    // --- WiFi ---
    Text::new("WIFI:", Point::new(left_x, y), label).draw(d)?;
    let (wifi_text, wifi_style) = match stats.wifi {
        None | Some(WifiMode::Off)        => ("OFF    0mA", green),
        Some(WifiMode::PowerSave)         => ("PS    20mA", value),
        Some(WifiMode::Active)            => ("ACT   90mA", red),
    };
    Text::with_alignment(wifi_text, Point::new(right_x, y), wifi_style, Alignment::Right).draw(d)?;
    y += row_h;

    // --- BLE ---
    Text::new("BLE:", Point::new(left_x, y), label).draw(d)?;
    let (ble_text, ble_style) = if stats.ble_on {
        ("ON    15mA", value)
    } else {
        ("OFF    0mA", green)
    };
    Text::with_alignment(ble_text, Point::new(right_x, y), ble_style, Alignment::Right).draw(d)?;
    y += row_h;

    // --- IMU ---
    Text::new("IMU:", Point::new(left_x, y), label).draw(d)?;
    let (imu_text, imu_style) = if stats.imu_on {
        ("ON     2mA", value)
    } else {
        ("OFF    0mA", green)
    };
    Text::with_alignment(imu_text, Point::new(right_x, y), imu_style, Alignment::Right).draw(d)?;
    y += row_h;

    // --- Audio ---
    Text::new("AUDIO:", Point::new(left_x, y), label).draw(d)?;
    let (au_text, au_style) = if stats.audio_on {
        ("ON    25mA", value)
    } else {
        ("OFF    0mA", green)
    };
    Text::with_alignment(au_text, Point::new(right_x, y), au_style, Alignment::Right).draw(d)?;
    y += row_h;

    // --- SD ---
    Text::new("SDCARD:", Point::new(left_x, y), label).draw(d)?;
    let (sd_text, sd_style) = if stats.sd_on {
        ("ON    30mA", value)
    } else {
        ("OFF    0mA", green)
    };
    Text::with_alignment(sd_text, Point::new(right_x, y), sd_style, Alignment::Right).draw(d)?;
    y += row_h + 8;

    // Divider
    Rectangle::new(Point::new(20, y), Size::new((W - 40) as u32, 1))
        .into_styled(PrimitiveStyle::with_fill(Rgb565::CSS_DARK_GRAY))
        .draw(d)?;
    y += 12;

    // --- Total ---
    let mut t_buf = [0u8; 16];
    let t_s = fmt_total(&mut t_buf, stats.total_ma());
    Text::new("TOTAL:", Point::new(left_x, y), label).draw(d)?;
    let total_style = if stats.total_ma() < 50 {
        MonoTextStyle::new(&FONT_10X20, Rgb565::GREEN)
    } else if stats.total_ma() < 120 {
        MonoTextStyle::new(&FONT_10X20, Rgb565::YELLOW)
    } else {
        MonoTextStyle::new(&FONT_10X20, Rgb565::RED)
    };
    Text::with_alignment(t_s, Point::new(right_x, y + 4), total_style, Alignment::Right).draw(d)?;
    y += row_h + 6;

    // --- Full-charge runtime (theoretical 100%→0%) ---
    let full_hours = stats.full_runtime_hours(300);
    let mut fh_buf = [0u8; 20];
    let fh_s = fmt_runtime_full(&mut fh_buf, full_hours);
    Text::new("100%:", Point::new(left_x, y), label).draw(d)?;
    Text::with_alignment(fh_s, Point::new(right_x, y), value, Alignment::Right).draw(d)?;
    y += row_h;

    // --- Remaining autonomy based on actual battery % ---
    let remain_hours = stats.estimated_hours(300);
    let mut rh_buf = [0u8; 20];
    let rh_s = fmt_remaining(&mut rh_buf, remain_hours, stats.battery_pct);
    Text::new("LEFT:", Point::new(left_x, y), label).draw(d)?;
    let left_style = if remain_hours < 2 {
        MonoTextStyle::new(&FONT_10X20, Rgb565::RED)
    } else if remain_hours < 6 {
        MonoTextStyle::new(&FONT_10X20, Rgb565::YELLOW)
    } else {
        MonoTextStyle::new(&FONT_10X20, Rgb565::GREEN)
    };
    Text::with_alignment(rh_s, Point::new(right_x, y + 4), left_style, Alignment::Right).draw(d)?;
    y += row_h + 4;

    // --- Battery raw ---
    Text::new("BATT:", Point::new(left_x, y), label).draw(d)?;
    let mut b_buf = [0u8; 20];
    let b_s = fmt_batt(&mut b_buf, stats.battery_mv, stats.battery_pct, stats.charging);
    Text::with_alignment(b_s, Point::new(right_x, y), value, Alignment::Right).draw(d)?;

    // Reboot button
    let rbt_x: i32 = cx - 50;
    let rbt_y: i32 = H - 60;
    let rbt_w: i32 = 100;
    let rbt_h: i32 = 32;
    RoundedRectangle::with_equal_corners(
        Rectangle::new(Point::new(rbt_x, rbt_y), Size::new(rbt_w as u32, rbt_h as u32)),
        Size::new(10, 10),
    ).into_styled(PrimitiveStyle::with_fill(Rgb565::new(20, 4, 0))).draw(d)?;
    let rbt_ts = MonoTextStyle::new(&FONT_10X20, Rgb565::WHITE);
    Text::with_alignment(
        "REBOOT",
        Point::new(cx, rbt_y + 22),
        rbt_ts,
        Alignment::Center,
    ).draw(d)?;

    // Footer hint
    let hint = MonoTextStyle::new(&FONT_8X13, Rgb565::CSS_DARK_GRAY);
    Text::with_alignment(
        "[swipe] next page",
        Point::new(cx, H - 18),
        hint,
        Alignment::Center,
    ).draw(d)?;

    Ok(())
}

/// Hit-test for the reboot button on the power page.
pub fn is_reboot_zone(x: u16, y: u16) -> bool {
    let cx = W / 2;
    let rbt_x = cx - 50;
    let rbt_y = H - 60;
    let xi = x as i32;
    let yi = y as i32;
    xi >= rbt_x - 8 && xi <= rbt_x + 100 + 8
        && yi >= rbt_y - 8 && yi <= rbt_y + 32 + 8
}

// Dummy helper so we can inline a yellow MonoTextStyle above without borrowing
// the one defined at the top of the function.
fn yellow_inline() -> MonoTextStyle<'static, Rgb565> {
    MonoTextStyle::new(&FONT_8X13, Rgb565::YELLOW)
}

fn fmt_u16(buf: &mut [u8], pos: &mut usize, mut v: u16) {
    if v == 0 {
        buf[*pos] = b'0';
        *pos += 1;
        return;
    }
    let mut digits = [0u8; 5];
    let mut n = 0;
    while v > 0 && n < 5 {
        digits[n] = b'0' + (v % 10) as u8;
        v /= 10;
        n += 1;
    }
    while n > 0 {
        n -= 1;
        buf[*pos] = digits[n];
        *pos += 1;
    }
}

fn fmt_mhz<'a>(buf: &'a mut [u8; 16], mhz: u16, ma: u16) -> &'a str {
    let mut p = 0;
    fmt_u16(buf, &mut p, mhz);
    for &c in b"MHz " { buf[p] = c; p += 1; }
    fmt_u16(buf, &mut p, ma);
    for &c in b"mA" { buf[p] = c; p += 1; }
    core::str::from_utf8(&buf[..p]).unwrap_or("?")
}

fn fmt_total<'a>(buf: &'a mut [u8; 16], ma: u16) -> &'a str {
    let mut p = 0;
    fmt_u16(buf, &mut p, ma);
    for &c in b"mA" { buf[p] = c; p += 1; }
    core::str::from_utf8(&buf[..p]).unwrap_or("?")
}

fn fmt_runtime_full<'a>(buf: &'a mut [u8; 20], hours: u16) -> &'a str {
    let mut p = 0;
    if hours >= 999 {
        for &c in b"--" { buf[p] = c; p += 1; }
    } else {
        fmt_u16(buf, &mut p, hours);
        for &c in b"h (300mAh)" { buf[p] = c; p += 1; }
    }
    core::str::from_utf8(&buf[..p]).unwrap_or("?h")
}

fn fmt_remaining<'a>(buf: &'a mut [u8; 20], hours: u16, pct: u8) -> &'a str {
    let mut p = 0;
    if hours >= 999 {
        for &c in b"--" { buf[p] = c; p += 1; }
    } else {
        buf[p] = b'~'; p += 1;
        fmt_u16(buf, &mut p, hours);
        for &c in b"h @" { buf[p] = c; p += 1; }
        fmt_u16(buf, &mut p, pct as u16);
        buf[p] = b'%'; p += 1;
    }
    core::str::from_utf8(&buf[..p]).unwrap_or("?h")
}

fn fmt_batt<'a>(buf: &'a mut [u8; 20], mv: u16, pct: u8, chg: bool) -> &'a str {
    let mut p = 0;
    fmt_u16(buf, &mut p, pct as u16);
    for &c in b"% " { buf[p] = c; p += 1; }
    fmt_u16(buf, &mut p, mv);
    for &c in b"mV" { buf[p] = c; p += 1; }
    if chg {
        for &c in b" CHG" { buf[p] = c; p += 1; }
    }
    core::str::from_utf8(&buf[..p]).unwrap_or("?")
}
