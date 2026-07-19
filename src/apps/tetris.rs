// Tetris - full screen, gyro + touch controls
// 12x16 grid filling the screen, tap=rotate, touch drag=move, swipe down=drop
// Gyroscope tilt left/right moves piece each tick
// Tap to restart on game over

use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{PrimitiveStyle, Rectangle, RoundedRectangle};
use embedded_graphics::mono_font::ascii::FONT_10X20;
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::text::{Alignment, Text};
use embedded_graphics::geometry::Point as EgPoint;

use crate::apps::{App, AppInput, AppResult};
use crate::peripherals::touch::SwipeDirection;

const GW: usize = 12;
const GH: usize = 16;
const BLOCK: i32 = 30;
const SCREEN_W: i32 = 410;
const SCREEN_H: i32 = 502;
const OX: i32 = (SCREEN_W - GW as i32 * BLOCK) / 2; // 25px each side
const OY: i32 = 20;
const DROP_MS: u32 = 700;
const GYRO_MOVE_MS: u32 = 150; // gyro moves piece every 150ms
const GYRO_THRESHOLD: f32 = 0.3; // tilt threshold in g

static SHAPES: [[[u8; 4]; 4]; 7] = [
    [[0,1,0,0],[0,1,0,0],[0,1,0,0],[0,1,0,0]], // I
    [[0,0,0,0],[0,1,1,0],[0,1,1,0],[0,0,0,0]], // O
    [[0,0,0,0],[0,1,1,0],[1,1,0,0],[0,0,0,0]], // S
    [[0,0,0,0],[1,1,0,0],[0,1,1,0],[0,0,0,0]], // Z
    [[0,0,0,0],[0,1,0,0],[1,1,1,0],[0,0,0,0]], // T
    [[0,0,0,0],[1,0,0,0],[1,1,1,0],[0,0,0,0]], // L
    [[0,0,0,0],[0,0,1,0],[1,1,1,0],[0,0,0,0]], // J
];

static COLORS: [Rgb565; 7] = [
    Rgb565::CYAN, Rgb565::YELLOW, Rgb565::GREEN, Rgb565::RED,
    Rgb565::new(20, 0, 31), Rgb565::new(31, 20, 0), Rgb565::BLUE,
];

pub struct TetrisGame {
    grid: [[u8; GW]; GH],
    piece: usize, rot: usize, px: i32, py: i32,
    score: u32, lines: u32, game_over: bool,
    drop_timer: u32,
    gyro_timer: u32,
    rng: u32,
    did_step: bool,
}

impl TetrisGame {
    pub fn new() -> Self {
        let mut g = Self {
            grid: [[0; GW]; GH], piece: 0, rot: 0, px: 4, py: -1,
            score: 0, lines: 0, game_over: false,
            drop_timer: 0, gyro_timer: 0, rng: 77777, did_step: false,
        };
        g.spawn_piece();
        g
    }

    fn random(&mut self, max: u32) -> u32 {
        self.rng ^= self.rng << 13; self.rng ^= self.rng >> 17; self.rng ^= self.rng << 5;
        (self.rng & 0x7FFFFFFF) % max
    }

    fn spawn_piece(&mut self) {
        self.piece = self.random(7) as usize;
        self.rot = 0;
        self.px = (GW as i32 - 4) / 2;
        self.py = -2; // Start above visible grid
        // Find first valid Y
        while self.py < 0 && self.collides(self.px, self.py, self.rot) {
            self.py += 1;
        }
        if self.collides(self.px, self.py, self.rot) { self.game_over = true; }
    }

    fn rotated(&self, rot: usize) -> [[u8; 4]; 4] {
        let s = &SHAPES[self.piece];
        let mut out = [[0u8; 4]; 4];
        for r in 0..4 { for c in 0..4 {
            match rot % 4 {
                0 => out[r][c] = s[r][c],
                1 => out[r][c] = s[3-c][r],
                2 => out[r][c] = s[3-r][3-c],
                3 => out[r][c] = s[c][3-r],
                _ => {}
            }
        }}
        out
    }

    fn collides(&self, px: i32, py: i32, rot: usize) -> bool {
        let shape = self.rotated(rot);
        for r in 0..4 { for c in 0..4 {
            if shape[r][c] != 0 {
                let gx = px + c as i32;
                let gy = py + r as i32;
                if gx < 0 || gx >= GW as i32 || gy >= GH as i32 { return true; }
                if gy >= 0 && self.grid[gy as usize][gx as usize] != 0 { return true; }
            }
        }}
        false
    }

    fn lock_piece(&mut self) {
        let shape = self.rotated(self.rot);
        for r in 0..4 { for c in 0..4 {
            if shape[r][c] != 0 {
                let gy = self.py + r as i32;
                let gx = self.px + c as i32;
                if gy >= 0 && gy < GH as i32 && gx >= 0 && gx < GW as i32 {
                    self.grid[gy as usize][gx as usize] = self.piece as u8 + 1;
                }
            }
        }}
        self.clear_lines();
        self.spawn_piece();
    }

    fn clear_lines(&mut self) {
        let mut cleared = 0u32;
        let mut r = GH - 1;
        loop {
            if self.grid[r].iter().all(|&c| c != 0) {
                // Shift everything down
                for rr in (1..=r).rev() { self.grid[rr] = self.grid[rr - 1]; }
                self.grid[0] = [0; GW];
                cleared += 1;
                // Don't decrement r, check same row again
            } else {
                if r == 0 { break; }
                r -= 1;
            }
        }
        self.lines += cleared;
        self.score += match cleared {
            1 => 100,
            2 => 300,
            3 => 500,
            4 => 800, // Tetris!
            _ => 0,
        };
    }

    fn try_move(&mut self, dx: i32) -> bool {
        if !self.collides(self.px + dx, self.py, self.rot) {
            self.px += dx;
            self.did_step = true;
            true
        } else { false }
    }

    pub fn stepped(&self) -> bool { self.did_step }
}

impl App for TetrisGame {
    fn name(&self) -> &str { "Tetris" }

    fn setup(&mut self) {
        self.grid = [[0; GW]; GH];
        self.score = 0; self.lines = 0; self.game_over = false;
        self.drop_timer = 0; self.gyro_timer = 0;
        self.spawn_piece();
        self.did_step = true; // Force initial render
    }

    fn update(&mut self, input: &AppInput) -> AppResult {
        self.did_step = false;

        // Game over: tap to restart
        if self.game_over {
            if input.tap { self.setup(); }
            return AppResult::Continue;
        }

        // Swipe input
        if let Some(swipe) = input.swipe {
            match swipe {
                SwipeDirection::Left => { self.try_move(-1); }
                SwipeDirection::Right => { self.try_move(1); }
                SwipeDirection::Down => {
                    while !self.collides(self.px, self.py + 1, self.rot) { self.py += 1; }
                    self.lock_piece();
                    self.did_step = true;
                }
                SwipeDirection::Tap | SwipeDirection::Up => {
                    let nr = (self.rot + 1) % 4;
                    if !self.collides(self.px, self.py, nr) {
                        self.rot = nr; self.did_step = true;
                    }
                }
            }
        }

        // Gyroscope tilt: move piece left/right
        let (ax, ay, _) = input.accel;
        self.gyro_timer += input.dt_ms;
        if self.gyro_timer >= GYRO_MOVE_MS {
            self.gyro_timer -= GYRO_MOVE_MS;
            if ay < -GYRO_THRESHOLD { self.try_move(1); }  // tilt right = move right
            else if ay > GYRO_THRESHOLD { self.try_move(-1); } // tilt left = move left
        }

        // Auto drop (speed increases with lines)
        let speed = (DROP_MS as i32 - self.lines as i32 * 30).max(150) as u32;
        self.drop_timer += input.dt_ms;
        if self.drop_timer >= speed {
            self.drop_timer -= speed;
            if !self.collides(self.px, self.py + 1, self.rot) {
                self.py += 1;
            } else {
                self.lock_piece();
            }
            self.did_step = true;
        }

        AppResult::Continue
    }

    fn render<D: DrawTarget<Color = Rgb565>>(&self, d: &mut D) {
        let _ = Rectangle::new(EgPoint::zero(), Size::new(SCREEN_W as u32, SCREEN_H as u32))
            .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK)).draw(d);

        // Play field border
        let _ = Rectangle::new(
            EgPoint::new(OX - 2, OY - 2),
            Size::new((GW as i32 * BLOCK + 4) as u32, (GH as i32 * BLOCK + 4) as u32),
        ).into_styled(PrimitiveStyle::with_stroke(Rgb565::new(4, 8, 4), 2)).draw(d);

        // Grid cells
        for r in 0..GH { for c in 0..GW {
            let val = self.grid[r][c];
            if val != 0 {
                let color = COLORS[(val - 1) as usize % 7];
                let _ = RoundedRectangle::with_equal_corners(
                    Rectangle::new(
                        EgPoint::new(OX + c as i32 * BLOCK + 1, OY + r as i32 * BLOCK + 1),
                        Size::new(BLOCK as u32 - 2, BLOCK as u32 - 2),
                    ), Size::new(3, 3),
                ).into_styled(PrimitiveStyle::with_fill(color)).draw(d);
            }
        }}

        // Ghost piece (shadow showing where piece will land)
        if !self.game_over {
            let mut ghost_y = self.py;
            while !self.collides(self.px, ghost_y + 1, self.rot) { ghost_y += 1; }
            if ghost_y != self.py {
                let shape = self.rotated(self.rot);
                for r in 0..4 { for c in 0..4 {
                    if shape[r][c] != 0 {
                        let gy = ghost_y + r as i32;
                        let gx = self.px + c as i32;
                        if gy >= 0 && gy < GH as i32 {
                            let _ = Rectangle::new(
                                EgPoint::new(OX + gx * BLOCK + 1, OY + gy * BLOCK + 1),
                                Size::new(BLOCK as u32 - 2, BLOCK as u32 - 2),
                            ).into_styled(PrimitiveStyle::with_stroke(Rgb565::new(6, 12, 6), 1)).draw(d);
                        }
                    }
                }}
            }

            // Current piece
            let shape = self.rotated(self.rot);
            let color = COLORS[self.piece];
            for r in 0..4 { for c in 0..4 {
                if shape[r][c] != 0 {
                    let gy = self.py + r as i32;
                    let gx = self.px + c as i32;
                    if gy >= 0 && gy < GH as i32 {
                        let _ = RoundedRectangle::with_equal_corners(
                            Rectangle::new(
                                EgPoint::new(OX + gx * BLOCK + 1, OY + gy * BLOCK + 1),
                                Size::new(BLOCK as u32 - 2, BLOCK as u32 - 2),
                            ), Size::new(3, 3),
                        ).into_styled(PrimitiveStyle::with_fill(color)).draw(d);
                    }
                }
            }}
        }

        // Score + Lines (in side margins)
        let white = MonoTextStyle::new(&FONT_10X20, Rgb565::WHITE);
        let dim = MonoTextStyle::new(&FONT_10X20, Rgb565::CSS_GRAY);

        let mut buf = [0u8; 8];
        let s = fmt_num(&mut buf, self.score);
        let _ = Text::with_alignment(s, EgPoint::new(SCREEN_W / 2, SCREEN_H - 10), white, Alignment::Center).draw(d);

        let mut lbuf = [0u8; 8];
        let ls = fmt_num(&mut lbuf, self.lines);
        let _ = Text::new(ls, EgPoint::new(4, 20), dim).draw(d);

        if self.game_over {
            // Dark overlay
            let _ = Rectangle::new(EgPoint::new(40, 200), Size::new(330, 100))
                .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK)).draw(d);
            let _ = Text::with_alignment("GAME OVER", EgPoint::new(SCREEN_W / 2, 240),
                MonoTextStyle::new(&FONT_10X20, Rgb565::RED), Alignment::Center).draw(d);
            let _ = Text::with_alignment("TAP TO RETRY", EgPoint::new(SCREEN_W / 2, 275),
                dim, Alignment::Center).draw(d);
        }
    }
}

fn fmt_num<'a>(buf: &'a mut [u8; 8], val: u32) -> &'a str {
    let mut p = 0;
    if val >= 10000 { buf[p] = b'0' + (val / 10000 % 10) as u8; p += 1; }
    if val >= 1000 { buf[p] = b'0' + (val / 1000 % 10) as u8; p += 1; }
    if val >= 100 { buf[p] = b'0' + (val / 100 % 10) as u8; p += 1; }
    if val >= 10 { buf[p] = b'0' + (val / 10 % 10) as u8; p += 1; }
    buf[p] = b'0' + (val % 10) as u8; p += 1;
    core::str::from_utf8(&buf[..p]).unwrap_or("0")
}
