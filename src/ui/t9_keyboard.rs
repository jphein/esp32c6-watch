// T9 Keyboard - French multi-tap input
// Ported from C++ T9Keyboard.cpp
// 12 buttons in 4x3 grid, tap to cycle characters, 800ms auto-commit

use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{PrimitiveStyle, Rectangle, RoundedRectangle};
use embedded_graphics::mono_font::ascii::FONT_10X20;
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::text::{Alignment, Text};

const KEYS_COLS: usize = 3;
const KEYS_ROWS: usize = 4;
const KEY_W: i32 = 120;
const KEY_H: i32 = 60;
const KEY_GAP: i32 = 4;
const KB_X: i32 = (410 - KEYS_COLS as i32 * KEY_W - (KEYS_COLS as i32 - 1) * KEY_GAP) / 2;
const KB_Y: i32 = 250;
const COMMIT_MS: u32 = 800;

struct KeyDef {
    label: &'static str,
    chars_lower: &'static [&'static str],
    chars_upper: &'static [&'static str],
}

static KEYS: [KeyDef; 12] = [
    KeyDef { label: "1 .,?!", chars_lower: &[".", ",", "?", "!", "1"], chars_upper: &[".", ",", "?", "!", "1"] },
    KeyDef { label: "2 abc", chars_lower: &["a","b","c","a","a","c","2"], chars_upper: &["A","B","C","2"] },
    KeyDef { label: "3 def", chars_lower: &["d","e","f","e","e","e","3"], chars_upper: &["D","E","F","3"] },
    KeyDef { label: "4 ghi", chars_lower: &["g","h","i","i","i","4"], chars_upper: &["G","H","I","4"] },
    KeyDef { label: "5 jkl", chars_lower: &["j","k","l","5"], chars_upper: &["J","K","L","5"] },
    KeyDef { label: "6 mno", chars_lower: &["m","n","o","o","6"], chars_upper: &["M","N","O","6"] },
    KeyDef { label: "7 pqrs", chars_lower: &["p","q","r","s","7"], chars_upper: &["P","Q","R","S","7"] },
    KeyDef { label: "8 tuv", chars_lower: &["t","u","v","u","u","8"], chars_upper: &["T","U","V","8"] },
    KeyDef { label: "9 wxyz", chars_lower: &["w","x","y","z","9"], chars_upper: &["W","X","Y","Z","9"] },
    KeyDef { label: "*SHIFT", chars_lower: &[], chars_upper: &[] },
    KeyDef { label: "0 SPC", chars_lower: &[" ", "0"], chars_upper: &[" ", "0"] },
    KeyDef { label: "<-DEL", chars_lower: &[], chars_upper: &[] },
];

#[derive(Clone, Copy, PartialEq)]
enum Mode { Lower, Upper, Numeric }

pub struct T9Keyboard {
    text: [u8; 128],
    text_len: usize,
    mode: Mode,
    last_key: i8,
    char_index: usize,
    commit_timer: u32,
    active: bool,
    pending_char: bool, // char not yet committed
}

impl T9Keyboard {
    pub fn new() -> Self {
        Self {
            text: [0; 128], text_len: 0, mode: Mode::Lower,
            last_key: -1, char_index: 0, commit_timer: 0,
            active: false, pending_char: false,
        }
    }

    pub fn show(&mut self) { self.active = true; }
    pub fn hide(&mut self) { self.active = false; self.commit(); }
    pub fn is_active(&self) -> bool { self.active }

    pub fn get_text(&self) -> &str {
        core::str::from_utf8(&self.text[..self.text_len]).unwrap_or("")
    }

    pub fn clear_text(&mut self) { self.text_len = 0; }

    fn commit(&mut self) {
        self.last_key = -1;
        self.char_index = 0;
        self.commit_timer = 0;
        self.pending_char = false;
    }

    fn add_char(&mut self, ch: &str) {
        for &b in ch.as_bytes() {
            if self.text_len < self.text.len() - 1 {
                self.text[self.text_len] = b;
                self.text_len += 1;
            }
        }
    }

    fn delete_last(&mut self) {
        if self.text_len > 0 { self.text_len -= 1; }
    }

    /// Call every frame with dt_ms. Returns true if display needs update.
    pub fn update(&mut self, dt_ms: u32) -> bool {
        if !self.active { return false; }
        if self.pending_char {
            self.commit_timer += dt_ms;
            if self.commit_timer >= COMMIT_MS {
                self.commit();
                return true;
            }
        }
        false
    }

    /// Handle tap at screen coordinate. Returns true if consumed.
    pub fn handle_tap(&mut self, x: u16, y: u16) -> bool {
        if !self.active { return false; }

        // Find which key was tapped
        let kx = (x as i32 - KB_X) / (KEY_W + KEY_GAP);
        let ky = (y as i32 - KB_Y) / (KEY_H + KEY_GAP);
        if kx < 0 || kx >= KEYS_COLS as i32 || ky < 0 || ky >= KEYS_ROWS as i32 { return false; }
        let idx = (ky * KEYS_COLS as i32 + kx) as usize;
        if idx >= 12 { return false; }

        // Shift key
        if idx == 9 {
            self.commit();
            self.mode = match self.mode {
                Mode::Lower => Mode::Upper,
                Mode::Upper => Mode::Numeric,
                Mode::Numeric => Mode::Lower,
            };
            return true;
        }

        // Delete key
        if idx == 11 {
            self.commit();
            self.delete_last();
            return true;
        }

        // Character key
        let key = &KEYS[idx];
        let chars = match self.mode {
            Mode::Lower => key.chars_lower,
            Mode::Upper | Mode::Numeric => key.chars_upper,
        };
        if chars.is_empty() { return true; }

        if self.mode == Mode::Numeric {
            self.commit();
            // Last char is the digit
            self.add_char(chars[chars.len() - 1]);
        } else if idx as i8 == self.last_key && self.pending_char {
            // Same key: cycle to next character
            self.char_index = (self.char_index + 1) % chars.len();
            self.delete_last(); // remove previous cycling char
            self.add_char(chars[self.char_index]);
            self.commit_timer = 0;
        } else {
            // New key
            self.commit();
            self.last_key = idx as i8;
            self.char_index = 0;
            self.add_char(chars[0]);
            self.pending_char = true;
            self.commit_timer = 0;
        }
        true
    }

    pub fn render<D: DrawTarget<Color = Rgb565>>(&self, d: &mut D) {
        if !self.active { return; }

        // Text area background
        let _ = Rectangle::new(Point::new(10, 200), Size::new(390, 40))
            .into_styled(PrimitiveStyle::with_fill(Rgb565::new(2, 4, 2)))
            .draw(d);
        // Text
        let txt = self.get_text();
        let style = MonoTextStyle::new(&FONT_10X20, Rgb565::WHITE);
        let _ = Text::new(txt, Point::new(20, 225), style).draw(d);
        // Cursor blink
        let cursor_x = 20 + txt.len() as i32 * 10;
        let _ = Rectangle::new(Point::new(cursor_x, 210), Size::new(2, 24))
            .into_styled(PrimitiveStyle::with_fill(Rgb565::WHITE))
            .draw(d);

        // Mode indicator
        let mode_str = match self.mode {
            Mode::Lower => "abc",
            Mode::Upper => "ABC",
            Mode::Numeric => "123",
        };
        let dim = MonoTextStyle::new(&FONT_10X20, Rgb565::CSS_GRAY);
        let _ = Text::with_alignment(mode_str, Point::new(370, 225), dim, Alignment::Center).draw(d);

        // Keyboard buttons
        let normal = MonoTextStyle::new(&FONT_10X20, Rgb565::WHITE);
        for row in 0..KEYS_ROWS {
            for col in 0..KEYS_COLS {
                let idx = row * KEYS_COLS + col;
                let x = KB_X + col as i32 * (KEY_W + KEY_GAP);
                let y = KB_Y + row as i32 * (KEY_H + KEY_GAP);

                let bg = if self.pending_char && idx as i8 == self.last_key {
                    Rgb565::new(4, 10, 4) // highlight active key
                } else {
                    Rgb565::new(3, 6, 3)
                };

                let _ = RoundedRectangle::with_equal_corners(
                    Rectangle::new(Point::new(x, y), Size::new(KEY_W as u32, KEY_H as u32)),
                    Size::new(6, 6),
                ).into_styled(PrimitiveStyle::with_fill(bg)).draw(d);

                let _ = Text::with_alignment(
                    KEYS[idx].label,
                    Point::new(x + KEY_W / 2, y + KEY_H / 2 + 5),
                    normal,
                    Alignment::Center,
                ).draw(d);
            }
        }
    }
}
