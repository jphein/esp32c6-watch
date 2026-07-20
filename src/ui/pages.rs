// Multi-page system with swipe transitions
// Pages: Clock | Sensors | System Info | Power | Mesh

use embedded_graphics::mono_font::ascii::FONT_10X20;
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{PrimitiveStyle, Rectangle};
use embedded_graphics::text::{Alignment, Text};

use crate::board;
use crate::drivers::co5300::DisplayError;
use crate::net::names;
use crate::net::smol_mesh::PeerView;

const W: u16 = board::LCD_WIDTH;
const H: u16 = board::LCD_HEIGHT;
const ANIM_STEPS: u16 = 8; // Number of animation frames

#[derive(Clone, Copy, PartialEq)]
pub enum Page {
    Clock = 0,
    Sensors = 1,
    System = 2,
    Power = 3,
    Mesh = 4,
}

impl Page {
    pub fn count() -> usize { 5 }

    pub fn next(self) -> Self {
        match self {
            Page::Clock => Page::Sensors,
            Page::Sensors => Page::System,
            Page::System => Page::Power,
            Page::Power => Page::Mesh,
            Page::Mesh => Page::Clock,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            Page::Clock => Page::Mesh,
            Page::Sensors => Page::Clock,
            Page::System => Page::Sensors,
            Page::Power => Page::System,
            Page::Mesh => Page::Power,
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
            Page::Mesh => "MESH",
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

// === Marauder's Watch: the mesh roster page ===
// One row per known SMOLv1 peer: realm name + id, a near/far bar driven by
// the per-peer RSSI EWMA, staleness dimming. Ported from smol #58.

/// RSSI mapped to the bar: `FAR_DBM` = empty (far), `NEAR_DBM` = full (near).
const NEAR_DBM: i32 = -40;
const FAR_DBM: i32 = -90;
/// Fresh below this age; dimmed above it (matches the mesh PEER_STALE_MS).
const ROW_STALE_MS: u64 = 3_000;
/// Very stale above this: dimmest tier, empty-ish presence.
const ROW_GONE_MS: u64 = 30_000;
/// Rows that fit under the header on the 410x502 panel.
pub const MESH_MAX_ROWS: usize = 7;

fn fmt_id3(buf: &mut [u8; 5], id: u8) -> &str {
    buf[0] = b'i';
    buf[1] = b'd';
    buf[2] = b'0' + (id / 100) % 10;
    buf[3] = b'0' + (id / 10) % 10;
    buf[4] = b'0' + id % 10;
    core::str::from_utf8(&buf[..]).unwrap_or("id???")
}

/// Draw the Marauder's Watch mesh roster.
pub fn draw_mesh_page<D: DrawTarget<Color = Rgb565>>(
    display: &mut D,
    my_id: u8,
    rows: &[PeerView],
) -> Result<(), D::Error> {
    let cx = W as i32 / 2;
    let cyan = MonoTextStyle::new(&FONT_10X20, Rgb565::CYAN);
    let white = MonoTextStyle::new(&FONT_10X20, Rgb565::WHITE);
    let dim = MonoTextStyle::new(&FONT_10X20, Rgb565::CSS_GRAY);
    let dimmer = MonoTextStyle::new(&FONT_10X20, Rgb565::CSS_DIM_GRAY);

    Text::with_alignment("MESH", Point::new(cx, 40), cyan, Alignment::Center).draw(display)?;

    // Our own banner: "id042 Celestial Herald".
    let (adj, noun) = names::name_for_id(my_id);
    let mut hdr = [0u8; 32];
    let mut n = 0;
    let mut idbuf = [0u8; 5];
    for &b in fmt_id3(&mut idbuf, my_id).as_bytes() {
        hdr[n] = b;
        n += 1;
    }
    hdr[n] = b' ';
    n += 1;
    for &b in adj.as_bytes() {
        hdr[n] = b;
        n += 1;
    }
    hdr[n] = b' ';
    n += 1;
    for &b in noun.as_bytes() {
        hdr[n] = b;
        n += 1;
    }
    Text::with_alignment(
        core::str::from_utf8(&hdr[..n]).unwrap_or("id???"),
        Point::new(cx, 75),
        white,
        Alignment::Center,
    )
    .draw(display)?;

    if rows.is_empty() {
        Text::with_alignment("No fleet heard", Point::new(cx, 250), dim, Alignment::Center)
            .draw(display)?;
        Text::with_alignment("listening ch6", Point::new(cx, 285), dimmer, Alignment::Center)
            .draw(display)?;
        return Ok(());
    }

    let mut y: i32 = 130;
    for row in rows.iter().take(MESH_MAX_ROWS) {
        let text_style = if row.age_ms < ROW_STALE_MS {
            white
        } else if row.age_ms < ROW_GONE_MS {
            dim
        } else {
            dimmer
        };

        // Realm name (derived from the id, exactly like the fleet does) + id.
        let mut namebuf = [0u8; 24];
        let name: &str = match row.id {
            Some(id) => {
                let (adj, noun) = names::name_for_id(id);
                let mut n = 0;
                for &b in adj.as_bytes() {
                    namebuf[n] = b;
                    n += 1;
                }
                namebuf[n] = b' ';
                n += 1;
                for &b in noun.as_bytes() {
                    namebuf[n] = b;
                    n += 1;
                }
                core::str::from_utf8(&namebuf[..n]).unwrap_or("?")
            }
            None => "Unnamed",
        };
        Text::new(name, Point::new(20, y), text_style).draw(display)?;

        let idtext: &str = match row.id {
            Some(id) => fmt_id3(&mut idbuf, id),
            None => "id???",
        };
        Text::with_alignment(idtext, Point::new(390, y), dimmer, Alignment::Right)
            .draw(display)?;

        // Near/far bar from the RSSI EWMA: full = near, empty = far.
        let track = Rectangle::new(Point::new(20, y + 8), Size::new(300, 12));
        track
            .into_styled(PrimitiveStyle::with_stroke(Rgb565::CSS_DIM_GRAY, 1))
            .draw(display)?;
        if let Some(dbm) = row.rssi_dbm {
            let frac = ((dbm as i32 - FAR_DBM) * 1000 / (NEAR_DBM - FAR_DBM)).clamp(0, 1000);
            let fill_w = (296 * frac / 1000).max(2) as u32;
            let fill_color = if row.age_ms >= ROW_STALE_MS {
                Rgb565::CSS_DIM_GRAY // stale: presence remembered, not live
            } else if frac >= 666 {
                Rgb565::GREEN // near
            } else if frac >= 333 {
                Rgb565::YELLOW
            } else {
                Rgb565::RED // far
            };
            Rectangle::new(Point::new(22, y + 10), Size::new(fill_w, 8))
                .into_styled(PrimitiveStyle::with_fill(fill_color))
                .draw(display)?;

            // "-62dB" right of the bar.
            let mut rbuf = [0u8; 5];
            let v = (-(dbm as i32)).clamp(0, 99) as u8;
            rbuf[0] = b'-';
            rbuf[1] = b'0' + v / 10;
            rbuf[2] = b'0' + v % 10;
            rbuf[3] = b'd';
            rbuf[4] = b'B';
            Text::with_alignment(
                core::str::from_utf8(&rbuf).unwrap_or("--dB"),
                Point::new(390, y + 20),
                dimmer,
                Alignment::Right,
            )
            .draw(display)?;
        } else {
            Text::with_alignment("--dB", Point::new(390, y + 20), dimmer, Alignment::Right)
                .draw(display)?;
        }

        y += 46;
    }

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
