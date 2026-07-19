// 2048 Game - ported from C++ Game2048.cpp
// 4x4 grid, swipe to merge tiles

use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{PrimitiveStyle, Rectangle, RoundedRectangle};
use embedded_graphics::mono_font::ascii::FONT_10X20;
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::text::{Alignment, Text};
use embedded_graphics::geometry::Point as EgPoint;

use crate::apps::{App, AppInput, AppResult};
use crate::peripherals::touch::SwipeDirection;

const GRID: usize = 4;
const CELL_SIZE: i32 = 90;
const GAP: i32 = 8;
const BOARD_X: i32 = (410 - GRID as i32 * CELL_SIZE - (GRID as i32 - 1) * GAP) / 2;
const BOARD_Y: i32 = 70;

pub struct Game2048 {
    tiles: [[u16; GRID]; GRID],
    score: u32,
    game_over: bool,
    rng: u32,
    moved: bool,
}

impl Game2048 {
    pub fn new() -> Self {
        let mut g = Self { tiles: [[0; GRID]; GRID], score: 0, game_over: false, rng: 54321, moved: false };
        g.spawn_tile();
        g.spawn_tile();
        g
    }

    fn random(&mut self, max: u32) -> u32 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 17;
        self.rng ^= self.rng << 5;
        (self.rng & 0x7FFFFFFF) % max
    }

    fn spawn_tile(&mut self) {
        let mut empty = [(0usize, 0usize); 16];
        let mut count = 0;
        for r in 0..GRID {
            for c in 0..GRID {
                if self.tiles[r][c] == 0 {
                    empty[count] = (r, c);
                    count += 1;
                }
            }
        }
        if count > 0 {
            let idx = self.random(count as u32) as usize;
            let (r, c) = empty[idx];
            self.tiles[r][c] = if self.random(10) < 1 { 4 } else { 2 };
        }
    }

    fn slide_row(row: &mut [u16; GRID]) -> (bool, u32) {
        let mut moved = false;
        let mut pts = 0u32;
        // Compact left
        let mut j = 0;
        for i in 0..GRID {
            if row[i] != 0 {
                if j != i { row[j] = row[i]; row[i] = 0; moved = true; }
                j += 1;
            }
        }
        // Merge
        for i in 0..GRID - 1 {
            if row[i] != 0 && row[i] == row[i + 1] {
                row[i] *= 2;
                pts += row[i] as u32;
                row[i + 1] = 0;
                moved = true;
            }
        }
        // Compact again
        j = 0;
        for i in 0..GRID {
            if row[i] != 0 {
                if j != i { row[j] = row[i]; row[i] = 0; }
                j += 1;
            }
        }
        (moved, pts)
    }

    fn do_move(&mut self, dir: SwipeDirection) {
        self.moved = false;
        match dir {
            SwipeDirection::Left => {
                for r in 0..GRID {
                    let mut row = self.tiles[r];
                    let (m, p) = Self::slide_row(&mut row);
                    self.tiles[r] = row;
                    if m { self.moved = true; }
                    self.score += p;
                }
            }
            SwipeDirection::Right => {
                for r in 0..GRID {
                    let mut row = self.tiles[r];
                    row.reverse();
                    let (m, p) = Self::slide_row(&mut row);
                    row.reverse();
                    self.tiles[r] = row;
                    if m { self.moved = true; }
                    self.score += p;
                }
            }
            SwipeDirection::Up => {
                for c in 0..GRID {
                    let mut col = [0u16; GRID];
                    for r in 0..GRID { col[r] = self.tiles[r][c]; }
                    let (m, p) = Self::slide_row(&mut col);
                    for r in 0..GRID { self.tiles[r][c] = col[r]; }
                    if m { self.moved = true; }
                    self.score += p;
                }
            }
            SwipeDirection::Down => {
                for c in 0..GRID {
                    let mut col = [0u16; GRID];
                    for r in 0..GRID { col[r] = self.tiles[GRID - 1 - r][c]; }
                    let (m, p) = Self::slide_row(&mut col);
                    for r in 0..GRID { self.tiles[GRID - 1 - r][c] = col[r]; }
                    if m { self.moved = true; }
                    self.score += p;
                }
            }
            _ => {}
        }
        if self.moved { self.spawn_tile(); }
    }

    fn tile_color(val: u16) -> Rgb565 {
        match val {
            2 => Rgb565::new(29, 58, 28),
            4 => Rgb565::new(29, 56, 24),
            8 => Rgb565::new(31, 44, 8),
            16 => Rgb565::new(31, 38, 4),
            32 => Rgb565::new(31, 30, 4),
            64 => Rgb565::new(31, 24, 2),
            128 => Rgb565::new(29, 56, 12),
            256 => Rgb565::new(29, 54, 8),
            512 => Rgb565::new(29, 52, 4),
            1024 => Rgb565::new(29, 50, 0),
            2048 => Rgb565::new(29, 48, 0),
            _ => Rgb565::new(10, 20, 10),
        }
    }
}

impl App for Game2048 {
    fn name(&self) -> &str { "2048" }
    fn setup(&mut self) {
        self.tiles = [[0; GRID]; GRID];
        self.score = 0;
        self.game_over = false;
        self.spawn_tile();
        self.spawn_tile();
    }

    fn update(&mut self, input: &AppInput) -> AppResult {
        if let Some(swipe) = input.swipe {
            if !self.game_over {
                self.do_move(swipe);
            }
        }
        AppResult::Continue
    }

    fn render<D: DrawTarget<Color = Rgb565>>(&self, d: &mut D) {
        let _ = Rectangle::new(EgPoint::zero(), Size::new(410, 502))
            .into_styled(PrimitiveStyle::with_fill(Rgb565::new(4, 8, 4)))
            .draw(d);

        // Score
        let mut buf = [0u8; 16];
        let s = fmt_num(&mut buf, b"SCORE:", self.score);
        let _ = Text::with_alignment(s, EgPoint::new(205, 35), MonoTextStyle::new(&FONT_10X20, Rgb565::WHITE), Alignment::Center).draw(d);

        // Board
        for r in 0..GRID {
            for c in 0..GRID {
                let x = BOARD_X + c as i32 * (CELL_SIZE + GAP);
                let y = BOARD_Y + r as i32 * (CELL_SIZE + GAP);
                let val = self.tiles[r][c];
                let color = Self::tile_color(val);
                let _ = RoundedRectangle::with_equal_corners(
                    Rectangle::new(EgPoint::new(x, y), Size::new(CELL_SIZE as u32, CELL_SIZE as u32)),
                    Size::new(8, 8),
                ).into_styled(PrimitiveStyle::with_fill(color)).draw(d);

                if val > 0 {
                    let mut nbuf = [0u8; 6];
                    let ns = fmt_tile(&mut nbuf, val);
                    // Dark text on light tiles, white on dark tiles
                    let txt_color = if val <= 4 { Rgb565::BLACK } else { Rgb565::WHITE };
                    let _ = Text::with_alignment(ns, EgPoint::new(x + CELL_SIZE / 2, y + CELL_SIZE / 2 + 5),
                        MonoTextStyle::new(&FONT_10X20, txt_color), Alignment::Center).draw(d);
                }
            }
        }
    }
}

fn fmt_num<'a>(buf: &'a mut [u8; 16], prefix: &[u8], val: u32) -> &'a str {
    let mut p = 0;
    for &c in prefix { buf[p] = c; p += 1; }
    if val >= 10000 { buf[p] = b'0' + (val / 10000 % 10) as u8; p += 1; }
    if val >= 1000 { buf[p] = b'0' + (val / 1000 % 10) as u8; p += 1; }
    if val >= 100 { buf[p] = b'0' + (val / 100 % 10) as u8; p += 1; }
    if val >= 10 { buf[p] = b'0' + (val / 10 % 10) as u8; p += 1; }
    buf[p] = b'0' + (val % 10) as u8; p += 1;
    core::str::from_utf8(&buf[..p]).unwrap_or("?")
}

fn fmt_tile<'a>(buf: &'a mut [u8; 6], val: u16) -> &'a str {
    let mut p = 0;
    if val >= 1000 { buf[p] = b'0' + (val / 1000 % 10) as u8; p += 1; }
    if val >= 100 { buf[p] = b'0' + (val / 100 % 10) as u8; p += 1; }
    if val >= 10 { buf[p] = b'0' + (val / 10 % 10) as u8; p += 1; }
    buf[p] = b'0' + (val % 10) as u8; p += 1;
    core::str::from_utf8(&buf[..p]).unwrap_or("?")
}
