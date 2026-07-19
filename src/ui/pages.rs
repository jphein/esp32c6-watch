// Multi-page system with swipe transitions
// Pages: Clock | Sensors | System Info

use embedded_graphics::mono_font::ascii::FONT_10X20;
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{PrimitiveStyle, Rectangle};
use embedded_graphics::text::{Alignment, Text};

use crate::board;
use crate::drivers::co5300::DisplayError;

const W: u16 = board::LCD_WIDTH;
const H: u16 = board::LCD_HEIGHT;
const ANIM_STEPS: u16 = 8; // Number of animation frames

#[derive(Clone, Copy, PartialEq)]
pub enum Page {
    Clock = 0,
    Sensors = 1,
    System = 2,
    Power = 3,
}

impl Page {
    pub fn count() -> usize { 4 }

    pub fn next(self) -> Self {
        match self {
            Page::Clock => Page::Sensors,
            Page::Sensors => Page::System,
            Page::System => Page::Power,
            Page::Power => Page::Clock,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            Page::Clock => Page::Power,
            Page::Sensors => Page::Clock,
            Page::System => Page::Sensors,
            Page::Power => Page::System,
        }
    }

    pub fn color(self) -> Rgb565 {
        // All pages use pure black AMOLED background for battery savings
        Rgb565::BLACK
    }

    pub fn name(self) -> &'static str {
        match self {
            Page::Clock => "CLOCK",
            Page::Sensors => "SENSORS",
            Page::System => "SYSTEM",
            Page::Power => "POWER",
        }
    }
}

/// Draw the sensors page content.
pub fn draw_sensors_page<D: DrawTarget<Color = Rgb565>>(
    display: &mut D,
    ax: i16, ay: i16, az: i16,
    gx: i16, gy: i16, gz: i16,
    temp: i16,
) -> Result<(), D::Error> {
    let cx = W as i32 / 2;
    let cyan = MonoTextStyle::new(&FONT_10X20, Rgb565::CYAN);
    let white = MonoTextStyle::new(&FONT_10X20, Rgb565::WHITE);
    let green = MonoTextStyle::new(&FONT_10X20, Rgb565::GREEN);
    let yellow = MonoTextStyle::new(&FONT_10X20, Rgb565::YELLOW);
    let dim = MonoTextStyle::new(&FONT_10X20, Rgb565::CSS_GRAY);

    Text::with_alignment("SENSORS", Point::new(cx, 40), cyan, Alignment::Center).draw(display)?;

    Text::with_alignment("Accelerometer", Point::new(cx, 90), dim, Alignment::Center).draw(display)?;

    let mut buf = [0u8; 16];
    fmt_axis(&mut buf, b'X', ax);
    Text::with_alignment(core::str::from_utf8(&buf[..8]).unwrap_or(""), Point::new(cx, 120), green, Alignment::Center).draw(display)?;
    fmt_axis(&mut buf, b'Y', ay);
    Text::with_alignment(core::str::from_utf8(&buf[..8]).unwrap_or(""), Point::new(cx, 150), green, Alignment::Center).draw(display)?;
    fmt_axis(&mut buf, b'Z', az);
    Text::with_alignment(core::str::from_utf8(&buf[..8]).unwrap_or(""), Point::new(cx, 180), green, Alignment::Center).draw(display)?;

    Text::with_alignment("Gyroscope", Point::new(cx, 230), dim, Alignment::Center).draw(display)?;

    fmt_axis(&mut buf, b'X', gx);
    Text::with_alignment(core::str::from_utf8(&buf[..8]).unwrap_or(""), Point::new(cx, 260), yellow, Alignment::Center).draw(display)?;
    fmt_axis(&mut buf, b'Y', gy);
    Text::with_alignment(core::str::from_utf8(&buf[..8]).unwrap_or(""), Point::new(cx, 290), yellow, Alignment::Center).draw(display)?;
    fmt_axis(&mut buf, b'Z', gz);
    Text::with_alignment(core::str::from_utf8(&buf[..8]).unwrap_or(""), Point::new(cx, 320), yellow, Alignment::Center).draw(display)?;

    let mut tbuf = [0u8; 10];
    let ts = fmt_temp(&mut tbuf, temp);
    Text::with_alignment(ts, Point::new(cx, 380), white, Alignment::Center).draw(display)?;

    Ok(())
}

/// Draw the system info page.
pub fn draw_system_page<D: DrawTarget<Color = Rgb565>>(
    display: &mut D,
    batt_mv: u16, batt_pct: u8, charging: bool,
) -> Result<(), D::Error> {
    let cx = W as i32 / 2;
    let cyan = MonoTextStyle::new(&FONT_10X20, Rgb565::CYAN);
    let white = MonoTextStyle::new(&FONT_10X20, Rgb565::WHITE);
    let dim = MonoTextStyle::new(&FONT_10X20, Rgb565::CSS_GRAY);
    let green = MonoTextStyle::new(&FONT_10X20, Rgb565::GREEN);

    Text::with_alignment("SYSTEM", Point::new(cx, 40), cyan, Alignment::Center).draw(display)?;

    Text::with_alignment("ESP32-S3 160MHz", Point::new(cx, 90), white, Alignment::Center).draw(display)?;
    Text::with_alignment("8MB PSRAM", Point::new(cx, 120), dim, Alignment::Center).draw(display)?;
    Text::with_alignment("32MB Flash", Point::new(cx, 150), dim, Alignment::Center).draw(display)?;
    Text::with_alignment("QSPI 80MHz DMA", Point::new(cx, 180), dim, Alignment::Center).draw(display)?;

    Text::with_alignment("Firmware", Point::new(cx, 230), cyan, Alignment::Center).draw(display)?;
    Text::with_alignment("waveshare-watch", Point::new(cx, 260), white, Alignment::Center).draw(display)?;
    Text::with_alignment("v0.3 Rust", Point::new(cx, 290), green, Alignment::Center).draw(display)?;
    Text::with_alignment("~110KB binary", Point::new(cx, 320), dim, Alignment::Center).draw(display)?;

    let chg_str = if charging { "USB: Connected" } else { "USB: Battery" };
    Text::with_alignment(chg_str, Point::new(cx, 370), white, Alignment::Center).draw(display)?;

    let mut buf = [0u8; 12];
    let vs = fmt_mv(&mut buf, batt_mv);
    Text::with_alignment(vs, Point::new(cx, 400), dim, Alignment::Center).draw(display)?;

    Ok(())
}

fn fmt_axis(buf: &mut [u8; 16], label: u8, val: i16) {
    buf[0] = label;
    buf[1] = b':';
    buf[2] = b' ';
    if val < 0 { buf[3] = b'-'; } else { buf[3] = b'+'; }
    let v = val.unsigned_abs();
    buf[4] = b'0' + (v / 100) as u8;
    buf[5] = b'.';
    buf[6] = b'0' + ((v / 10) % 10) as u8;
    buf[7] = b'0' + (v % 10) as u8;
}

fn fmt_temp<'a>(buf: &'a mut [u8; 10], temp_c10: i16) -> &'a str {
    let mut p = 0;
    if temp_c10 < 0 { buf[p] = b'-'; p += 1; }
    let v = temp_c10.unsigned_abs();
    buf[p] = b'0' + (v / 100) as u8; p += 1;
    buf[p] = b'0' + ((v / 10) % 10) as u8; p += 1;
    buf[p] = b'.'; p += 1;
    buf[p] = b'0' + (v % 10) as u8; p += 1;
    buf[p] = b'C'; p += 1;
    core::str::from_utf8(&buf[..p]).unwrap_or("??C")
}

fn fmt_mv<'a>(buf: &'a mut [u8; 12], mv: u16) -> &'a str {
    let mut p = 0;
    if mv >= 1000 { buf[p] = b'0' + (mv/1000) as u8; p += 1; }
    buf[p] = b'0' + ((mv/100)%10) as u8; p += 1;
    buf[p] = b'0' + ((mv/10)%10) as u8; p += 1;
    buf[p] = b'0' + (mv%10) as u8; p += 1;
    for &c in b"mV" { buf[p] = c; p += 1; }
    core::str::from_utf8(&buf[..p]).unwrap_or("????mV")
}
