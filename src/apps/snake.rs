// Snake Game - ported from C++ SnakeGame.cpp
// Grid: 20x21 cells, 20px per cell, 8-direction movement, wall wrapping

use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::geometry::Point as EgPoint;
use embedded_graphics::primitives::{Circle, PrimitiveStyle, Rectangle, RoundedRectangle};
use embedded_graphics::mono_font::ascii::FONT_10X20;
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::text::{Alignment, Text};

use crate::apps::{App, AppInput, AppResult};
use crate::peripherals::touch::SwipeDirection;

const GRID_SIZE: i32 = 20;
const GRID_W: i32 = 20;
const GRID_H: i32 = 21;
const OFFSET_X: i32 = 5;
const OFFSET_Y: i32 = 60;
const MAX_SNAKE_LEN: usize = 100;
const GAME_SPEED_MS: u32 = 130;

#[derive(Clone, Copy, PartialEq)]
enum Direction {
    Up, Down, Left, Right,
    UpLeft, UpRight, DownLeft, DownRight,
}

#[derive(Clone, Copy)]
struct Point { x: i32, y: i32 }

pub struct SnakeGame {
    snake: [Point; MAX_SNAKE_LEN],
    len: usize,
    food: Point,
    dir: Direction,
    next_dir: Direction,
    score: u32,
    time_accum: u32, // ms since last step
    rng_state: u32,
    did_step: bool,
}

impl SnakeGame {
    pub fn new() -> Self {
        let mut game = Self {
            snake: [Point { x: 0, y: 0 }; MAX_SNAKE_LEN],
            len: 3,
            food: Point { x: 5, y: 5 },
            dir: Direction::Up,
            next_dir: Direction::Up,
            score: 0,
            time_accum: 0,
            rng_state: 12345,
            did_step: false,
        };
        game.setup();
        game
    }

    fn random(&mut self, max: i32) -> i32 {
        // Simple xorshift PRNG
        self.rng_state ^= self.rng_state << 13;
        self.rng_state ^= self.rng_state >> 17;
        self.rng_state ^= self.rng_state << 5;
        ((self.rng_state & 0x7FFFFFFF) % max as u32) as i32
    }

    fn spawn_food(&mut self) {
        self.food.x = self.random(GRID_W);
        self.food.y = self.random(GRID_H);
    }

    pub fn score(&self) -> u32 { self.score }
    pub fn stepped(&self) -> bool { self.did_step }

    fn handle_swipe(&mut self, dir: SwipeDirection) {
        self.next_dir = match dir {
            SwipeDirection::Up => Direction::Up,
            SwipeDirection::Down => Direction::Down,
            SwipeDirection::Left => Direction::Left,
            SwipeDirection::Right => Direction::Right,
            _ => self.next_dir,
        };
    }
}

impl App for SnakeGame {
    fn name(&self) -> &str { "Snake" }

    fn setup(&mut self) {
        self.len = 3;
        self.snake[0] = Point { x: GRID_W / 2, y: GRID_H / 2 };
        self.snake[1] = Point { x: GRID_W / 2, y: GRID_H / 2 + 1 };
        self.snake[2] = Point { x: GRID_W / 2, y: GRID_H / 2 + 2 };
        self.dir = Direction::Up;
        self.next_dir = Direction::Up;
        self.score = 0;
        self.time_accum = 0;
        self.spawn_food();
    }

    fn update(&mut self, input: &AppInput) -> AppResult {
        // Handle swipe input
        if let Some(swipe) = input.swipe {
            self.handle_swipe(swipe);
        }

        // Accumulate time
        self.time_accum += input.dt_ms;
        self.did_step = false;

        // Step game at fixed interval
        if self.time_accum >= GAME_SPEED_MS {
            self.did_step = true;
            self.time_accum -= GAME_SPEED_MS;
            self.dir = self.next_dir;

            // Compute next head position
            let mut head = self.snake[0];
            match self.dir {
                Direction::Up => head.y -= 1,
                Direction::Down => head.y += 1,
                Direction::Left => head.x -= 1,
                Direction::Right => head.x += 1,
                Direction::UpLeft => { head.x -= 1; head.y -= 1; }
                Direction::UpRight => { head.x += 1; head.y -= 1; }
                Direction::DownLeft => { head.x -= 1; head.y += 1; }
                Direction::DownRight => { head.x += 1; head.y += 1; }
            }

            // Wall wrapping
            if head.x < 0 { head.x = GRID_W - 1; }
            else if head.x >= GRID_W { head.x = 0; }
            if head.y < 0 { head.y = GRID_H - 1; }
            else if head.y >= GRID_H { head.y = 0; }

            // Eat food? (1-cell tolerance for easier gameplay on small screen)
            let dx = (head.x - self.food.x).abs();
            let dy = (head.y - self.food.y).abs();
            if dx <= 1 && dy <= 1 {
                if self.len < MAX_SNAKE_LEN {
                    self.len += 1;
                }
                self.score += 10;
                self.spawn_food();
            }

            // Shift body
            for i in (1..self.len).rev() {
                self.snake[i] = self.snake[i - 1];
            }
            self.snake[0] = head;
        }

        AppResult::Continue
    }

    fn render<D: DrawTarget<Color = Rgb565>>(&self, d: &mut D) {
        // Clear
        let _ = Rectangle::new(EgPoint::zero(), Size::new(410, 502))
            .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK))
            .draw(d);

        // Food (red circle)
        let fx = OFFSET_X + self.food.x * GRID_SIZE + GRID_SIZE / 2;
        let fy = OFFSET_Y + self.food.y * GRID_SIZE + GRID_SIZE / 2;
        let _ = Circle::new(
            EgPoint::new(fx - GRID_SIZE / 2 + 2, fy - GRID_SIZE / 2 + 2),
            (GRID_SIZE - 4) as u32,
        )
        .into_styled(PrimitiveStyle::with_fill(Rgb565::RED))
        .draw(d);

        // Snake body first (darker green), then head on top (bright green)
        for i in (1..self.len).rev() {
            let sx = OFFSET_X + self.snake[i].x * GRID_SIZE + 1;
            let sy = OFFSET_Y + self.snake[i].y * GRID_SIZE + 1;
            let _ = RoundedRectangle::with_equal_corners(
                Rectangle::new(EgPoint::new(sx, sy), Size::new((GRID_SIZE - 2) as u32, (GRID_SIZE - 2) as u32)),
                Size::new(4, 4),
            ).into_styled(PrimitiveStyle::with_fill(Rgb565::new(0, 23, 0))).draw(d);
        }
        // Head drawn last = always on top
        {
            let sx = OFFSET_X + self.snake[0].x * GRID_SIZE + 1;
            let sy = OFFSET_Y + self.snake[0].y * GRID_SIZE + 1;
            let _ = RoundedRectangle::with_equal_corners(
                Rectangle::new(EgPoint::new(sx, sy), Size::new((GRID_SIZE - 2) as u32, (GRID_SIZE - 2) as u32)),
                Size::new(4, 4),
            ).into_styled(PrimitiveStyle::with_fill(Rgb565::GREEN)).draw(d);
        }

        // Score
        let mut buf = [0u8; 16];
        let s = format_score(&mut buf, self.score);
        let style = MonoTextStyle::new(&FONT_10X20, Rgb565::CYAN);
        let _ = Text::with_alignment(s, EgPoint::new(205, 30), style, Alignment::Center).draw(d);
    }
}

fn format_score<'a>(buf: &'a mut [u8; 16], score: u32) -> &'a str {
    let mut p = 0;
    for &c in b"SCORE: " { buf[p] = c; p += 1; }
    if score >= 1000 { buf[p] = b'0' + (score / 1000 % 10) as u8; p += 1; }
    if score >= 100 { buf[p] = b'0' + (score / 100 % 10) as u8; p += 1; }
    if score >= 10 { buf[p] = b'0' + (score / 10 % 10) as u8; p += 1; }
    buf[p] = b'0' + (score % 10) as u8; p += 1;
    core::str::from_utf8(&buf[..p]).unwrap_or("SCORE: ?")
}
