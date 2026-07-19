// Maze Game - ported from C++ MazeGame.cpp
// Tilt the watch (IMU accelerometer) to move ball through maze

use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{Circle, PrimitiveStyle, Rectangle};
use embedded_graphics::mono_font::ascii::FONT_10X20;
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::text::{Alignment, Text};
use embedded_graphics::geometry::Point as EgPoint;

use crate::apps::{App, AppInput, AppResult};

const ROWS: usize = 12;
const COLS: usize = 10;
const CELL: i32 = 40;
const OX: i32 = (410 - COLS as i32 * CELL) / 2;
const OY: i32 = (502 - ROWS as i32 * CELL) / 2;

// 0=empty, 1=wall, 2=start, 3=goal
static MAZE: [[u8; COLS]; ROWS] = [
    [1,1,1,1,1,1,1,1,1,1],
    [1,2,0,0,1,0,0,0,0,1],
    [1,0,1,0,1,0,1,1,0,1],
    [1,0,1,0,0,0,0,1,0,1],
    [1,0,1,1,1,1,0,1,0,1],
    [1,0,0,0,0,1,0,0,0,1],
    [1,1,1,1,0,1,1,1,0,1],
    [1,0,0,0,0,0,0,1,0,1],
    [1,0,1,1,1,1,0,1,0,1],
    [1,0,1,0,0,0,0,0,0,1],
    [1,0,0,0,1,1,1,1,3,1],
    [1,1,1,1,1,1,1,1,1,1],
];

pub struct MazeGame {
    ball_x: f32,
    ball_y: f32,
    vel_x: f32,
    vel_y: f32,
    won: bool,
}

impl MazeGame {
    pub fn new() -> Self {
        // Find start position
        let (sx, sy) = Self::find_cell(2);
        Self {
            ball_x: sx as f32 + 0.5,
            ball_y: sy as f32 + 0.5,
            vel_x: 0.0, vel_y: 0.0, won: false,
        }
    }

    fn find_cell(val: u8) -> (usize, usize) {
        for r in 0..ROWS { for c in 0..COLS {
            if MAZE[r][c] == val { return (c, r); }
        }}
        (1, 1)
    }

    fn is_wall(&self, gx: i32, gy: i32) -> bool {
        if gx < 0 || gy < 0 || gx >= COLS as i32 || gy >= ROWS as i32 { return true; }
        MAZE[gy as usize][gx as usize] == 1
    }
}

impl App for MazeGame {
    fn name(&self) -> &str { "Maze" }

    fn setup(&mut self) {
        let (sx, sy) = Self::find_cell(2);
        self.ball_x = sx as f32 + 0.5;
        self.ball_y = sy as f32 + 0.5;
        self.vel_x = 0.0;
        self.vel_y = 0.0;
        self.won = false;
    }

    fn update(&mut self, input: &AppInput) -> AppResult {
        if self.won { return AppResult::Continue; }

        let (ax, ay, _) = input.accel;
        // Map accelerometer to velocity (inverted for correct watch orientation)
        self.vel_x -= ay * 0.5;  // tilt right = ball right
        self.vel_y += ax * 0.5;  // tilt forward = ball down
        self.vel_x *= 0.9; // friction
        self.vel_y *= 0.9;
        self.vel_x = self.vel_x.clamp(-5.0, 5.0);
        self.vel_y = self.vel_y.clamp(-5.0, 5.0);

        let dt = (input.dt_ms as f32 / 16.0).clamp(0.5, 3.0);

        // Move with collision
        let nx = self.ball_x + self.vel_x * 0.05 * dt;
        let ny = self.ball_y + self.vel_y * 0.05 * dt;

        if !self.is_wall(nx as i32, self.ball_y as i32) {
            self.ball_x = nx;
        } else {
            self.vel_x *= -0.5;
        }
        if !self.is_wall(self.ball_x as i32, ny as i32) {
            self.ball_y = ny;
        } else {
            self.vel_y *= -0.5;
        }

        // Check goal
        let gx = self.ball_x as usize;
        let gy = self.ball_y as usize;
        if gx < COLS && gy < ROWS && MAZE[gy][gx] == 3 {
            self.won = true;
        }

        AppResult::Continue
    }

    fn render<D: DrawTarget<Color = Rgb565>>(&self, d: &mut D) {
        let _ = Rectangle::new(EgPoint::zero(), Size::new(410, 502))
            .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK)).draw(d);

        // Draw maze
        for r in 0..ROWS { for c in 0..COLS {
            let x = OX + c as i32 * CELL;
            let y = OY + r as i32 * CELL;
            let color = match MAZE[r][c] {
                1 => Rgb565::new(4, 8, 16), // dark blue walls
                3 => Rgb565::GREEN,          // goal
                _ => continue,
            };
            let _ = Rectangle::new(EgPoint::new(x, y), Size::new(CELL as u32, CELL as u32))
                .into_styled(PrimitiveStyle::with_fill(color)).draw(d);
        }}

        // Ball
        let bx = OX + (self.ball_x * CELL as f32) as i32;
        let by = OY + (self.ball_y * CELL as f32) as i32;
        let _ = Circle::new(EgPoint::new(bx - 10, by - 10), 20)
            .into_styled(PrimitiveStyle::with_fill(Rgb565::RED)).draw(d);
        // Shine
        let _ = Circle::new(EgPoint::new(bx - 5, by - 5), 6)
            .into_styled(PrimitiveStyle::with_fill(Rgb565::WHITE)).draw(d);

        if self.won {
            let _ = Text::with_alignment("YOU WIN!", EgPoint::new(205, 30),
                MonoTextStyle::new(&FONT_10X20, Rgb565::GREEN), Alignment::Center).draw(d);
        }
    }
}
