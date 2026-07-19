// 7-segment style digit renderer using fill_solid rectangles
// Generic over DrawTarget - works with framebuffer or direct display

use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{PrimitiveStyle, Rectangle};

const SEGMENTS: [u8; 10] = [
    0b0111111, 0b0000110, 0b1011011, 0b1001111, 0b1100110,
    0b1101101, 0b1111101, 0b0000111, 0b1111111, 0b1101111,
];

pub fn draw_digit<D: DrawTarget<Color = Rgb565>>(
    display: &mut D, x: i32, y: i32, digit: u8,
    w: i32, h: i32, t: i32, color: Rgb565, bg: Rgb565,
) -> Result<(), D::Error> {
    if digit > 9 { return Ok(()); }
    let seg = SEGMENTS[digit as usize];
    let hh = h / 2;
    let on = |bit: u8| -> Rgb565 { if seg & bit != 0 { color } else { bg } };

    // A: top, B: top-right, C: bottom-right, D: bottom, E: bottom-left, F: top-left, G: middle
    seg_h(display, x + t, y, w - 2*t, t, on(0x01))?;
    seg_v(display, x + w - t, y + t, hh - t, t, on(0x02))?;
    seg_v(display, x + w - t, y + hh + t, hh - t, t, on(0x04))?;
    seg_h(display, x + t, y + h - t, w - 2*t, t, on(0x08))?;
    seg_v(display, x, y + hh + t, hh - t, t, on(0x10))?;
    seg_v(display, x, y + t, hh - t, t, on(0x20))?;
    seg_h(display, x + t, y + hh - t/2, w - 2*t, t, on(0x40))
}

fn seg_h<D: DrawTarget<Color = Rgb565>>(d: &mut D, x: i32, y: i32, w: i32, h: i32, c: Rgb565) -> Result<(), D::Error> {
    Rectangle::new(Point::new(x, y), Size::new(w as u32, h as u32))
        .into_styled(PrimitiveStyle::with_fill(c)).draw(d).map(|_| ())
}

fn seg_v<D: DrawTarget<Color = Rgb565>>(d: &mut D, x: i32, y: i32, h: i32, w: i32, c: Rgb565) -> Result<(), D::Error> {
    Rectangle::new(Point::new(x, y), Size::new(w as u32, h as u32))
        .into_styled(PrimitiveStyle::with_fill(c)).draw(d).map(|_| ())
}

pub fn draw_colon<D: DrawTarget<Color = Rgb565>>(d: &mut D, x: i32, y: i32, h: i32, t: i32, color: Rgb565) -> Result<(), D::Error> {
    let s = t as u32;
    Rectangle::new(Point::new(x, y + h/4), Size::new(s, s))
        .into_styled(PrimitiveStyle::with_fill(color)).draw(d).map(|_| ())?;
    Rectangle::new(Point::new(x, y + 3*h/4 - t), Size::new(s, s))
        .into_styled(PrimitiveStyle::with_fill(color)).draw(d).map(|_| ())
}

pub fn draw_time<D: DrawTarget<Color = Rgb565>>(
    d: &mut D, center_x: i32, y: i32,
    h: u8, m: u8, s: u8, color: Rgb565, bg: Rgb565,
) -> Result<(), D::Error> {
    let dw = 36; let dh = 64; let t = 6; let gap = 6; let cw = 14;
    let total = 6*dw + 2*cw + 7*gap;
    let mut x = center_x - total/2;

    draw_digit(d, x, y, h/10, dw, dh, t, color, bg)?; x += dw+gap;
    draw_digit(d, x, y, h%10, dw, dh, t, color, bg)?; x += dw+gap;
    draw_colon(d, x+cw/2-t/2, y, dh, t, color)?; x += cw+gap;
    draw_digit(d, x, y, m/10, dw, dh, t, color, bg)?; x += dw+gap;
    draw_digit(d, x, y, m%10, dw, dh, t, color, bg)?; x += dw+gap;
    draw_colon(d, x+cw/2-t/2, y, dh, t, color)?; x += cw+gap;
    draw_digit(d, x, y, s/10, dw, dh, t, color, bg)?; x += dw+gap;
    draw_digit(d, x, y, s%10, dw, dh, t, color, bg)
}

/// Draw HH:MM only, larger digits, used by the Always-On-Display.
pub fn draw_hhmm<D: DrawTarget<Color = Rgb565>>(
    d: &mut D, center_x: i32, y: i32,
    h: u8, m: u8, color: Rgb565, bg: Rgb565,
) -> Result<(), D::Error> {
    // Bigger than the normal watchface — fewer chars so we have horizontal room.
    let dw = 56; let dh = 96; let t = 8; let gap = 8; let cw = 18;
    let total = 4*dw + cw + 5*gap;
    let mut x = center_x - total/2;

    draw_digit(d, x, y, h/10, dw, dh, t, color, bg)?; x += dw+gap;
    draw_digit(d, x, y, h%10, dw, dh, t, color, bg)?; x += dw+gap;
    draw_colon(d, x+cw/2-t/2, y, dh, t, color)?; x += cw+gap;
    draw_digit(d, x, y, m/10, dw, dh, t, color, bg)?; x += dw+gap;
    draw_digit(d, x, y, m%10, dw, dh, t, color, bg)
}
