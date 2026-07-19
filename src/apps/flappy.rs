// Flappy Bird - framebuffer render, throttled 30fps, instant touch via GPIO

use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{Circle, PrimitiveStyle, Rectangle, RoundedRectangle};
use embedded_graphics::mono_font::ascii::FONT_10X20;
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::text::{Alignment, Text};
use embedded_graphics::geometry::Point as EgPoint;

use crate::apps::{App, AppInput, AppResult};

const W: i32 = 410;
const H: i32 = 502;
const BIRD_X: i32 = 90;
const BIRD_R: i32 = 14;
const PIPE_W: i32 = 55;
const PIPE_GAP: i32 = 140;
const GRAVITY: f32 = 0.45;
const JUMP_VEL: f32 = -6.5;
const PIPE_SPEED: f32 = 2.5;
const GROUND_H: i32 = 30;
const MARGIN: i32 = 6; // Safe margin for rounded screen

pub struct FlappyGame {
    bird_y: f32,
    bird_vel: f32,
    pipes: [(f32, i32); 3],
    score: u32,
    game_over: bool,
    rng: u32,
    was_touching: bool,
    jump_cooldown: u32,
}

impl FlappyGame {
    pub fn new() -> Self {
        let mut g = Self {
            bird_y: 200.0, bird_vel: 0.0,
            pipes: [(0.0, 0); 3],
            score: 0, game_over: false, rng: 99999,
            was_touching: false, jump_cooldown: 0,
        };
        g.init_pipes();
        g
    }

    fn random(&mut self, min: i32, max: i32) -> i32 {
        self.rng ^= self.rng << 13; self.rng ^= self.rng >> 17; self.rng ^= self.rng << 5;
        min + ((self.rng & 0x7FFFFFFF) % (max - min) as u32) as i32
    }

    fn init_pipes(&mut self) {
        for i in 0..3 {
            self.pipes[i].0 = W as f32 + 60.0 + i as f32 * 180.0;
            self.pipes[i].1 = self.random(120, H - 120);
        }
    }
}

impl App for FlappyGame {
    fn name(&self) -> &str { "Flappy" }

    fn setup(&mut self) {
        self.bird_y = 200.0;
        self.bird_vel = 0.0;
        self.score = 0;
        self.game_over = false;
        self.was_touching = false;
        self.jump_cooldown = 200;
        self.init_pipes();
    }

    fn update(&mut self, input: &AppInput) -> AppResult {
        let touching = input.touch.is_some();
        self.jump_cooldown = self.jump_cooldown.saturating_sub(input.dt_ms);

        if self.game_over {
            if (touching && !self.was_touching) || input.tap { self.setup(); }
            self.was_touching = touching;
            return AppResult::Continue;
        }

        if touching && self.jump_cooldown == 0 {
            self.bird_vel = JUMP_VEL;
            self.jump_cooldown = 120;
        }
        self.was_touching = touching;

        let dt = (input.dt_ms as f32 / 16.0).clamp(0.3, 3.0);
        self.bird_vel += GRAVITY * dt;
        self.bird_y += self.bird_vel * dt;

        for pipe in &mut self.pipes { pipe.0 -= PIPE_SPEED * dt; }

        let max_x = self.pipes.iter().map(|p| p.0 as i32).max().unwrap_or(W);
        for i in 0..3 {
            if self.pipes[i].0 < -(PIPE_W as f32) {
                self.pipes[i].0 = (max_x + 160) as f32;
                self.pipes[i].1 = self.random(120, H - 120);
                self.score += 1;
            }
        }

        let by = self.bird_y as i32;
        if by < MARGIN + BIRD_R || by > H - GROUND_H - BIRD_R { self.game_over = true; }
        for pipe in &self.pipes {
            let px = pipe.0 as i32;
            if BIRD_X + BIRD_R > px && BIRD_X - BIRD_R < px + PIPE_W {
                if by - BIRD_R < pipe.1 - PIPE_GAP / 2 || by + BIRD_R > pipe.1 + PIPE_GAP / 2 {
                    self.game_over = true;
                }
            }
        }

        AppResult::Continue
    }

    fn render<D: DrawTarget<Color = Rgb565>>(&self, d: &mut D) {
        // Sky
        let _ = Rectangle::new(EgPoint::new(0, 0), Size::new(W as u32, (H - GROUND_H) as u32))
            .into_styled(PrimitiveStyle::with_fill(Rgb565::new(1, 4, 10))).draw(d);
        // Ground
        let _ = Rectangle::new(EgPoint::new(0, H - GROUND_H), Size::new(W as u32, GROUND_H as u32))
            .into_styled(PrimitiveStyle::with_fill(Rgb565::new(8, 16, 2))).draw(d);

        // Pipes
        let pipe_color = Rgb565::new(2, 25, 2);
        let lip_color = Rgb565::new(1, 18, 1);
        for pipe in &self.pipes {
            let px = pipe.0 as i32;
            if px > -PIPE_W && px < W {
                let gap_top = pipe.1 - PIPE_GAP / 2;
                let gap_bot = pipe.1 + PIPE_GAP / 2;
                if gap_top > 0 {
                    let _ = Rectangle::new(EgPoint::new(px, 0), Size::new(PIPE_W as u32, gap_top as u32))
                        .into_styled(PrimitiveStyle::with_fill(pipe_color)).draw(d);
                    let _ = Rectangle::new(EgPoint::new(px - 3, gap_top - 16), Size::new((PIPE_W + 6) as u32, 16))
                        .into_styled(PrimitiveStyle::with_fill(lip_color)).draw(d);
                }
                if gap_bot < H - GROUND_H {
                    let _ = Rectangle::new(EgPoint::new(px, gap_bot), Size::new(PIPE_W as u32, (H - GROUND_H - gap_bot) as u32))
                        .into_styled(PrimitiveStyle::with_fill(pipe_color)).draw(d);
                    let _ = Rectangle::new(EgPoint::new(px - 3, gap_bot), Size::new((PIPE_W + 6) as u32, 16))
                        .into_styled(PrimitiveStyle::with_fill(lip_color)).draw(d);
                }
            }
        }

        // Bird
        let by = self.bird_y as i32;
        let _ = Circle::new(EgPoint::new(BIRD_X - BIRD_R, by - BIRD_R), (BIRD_R * 2) as u32)
            .into_styled(PrimitiveStyle::with_fill(Rgb565::YELLOW)).draw(d);
        let _ = Circle::new(EgPoint::new(BIRD_X + 4, by - 5), 5)
            .into_styled(PrimitiveStyle::with_fill(Rgb565::WHITE)).draw(d);
        let _ = Circle::new(EgPoint::new(BIRD_X + 6, by - 4), 2)
            .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK)).draw(d);
        let _ = Rectangle::new(EgPoint::new(BIRD_X + BIRD_R - 2, by - 2), Size::new(8, 5))
            .into_styled(PrimitiveStyle::with_fill(Rgb565::new(31, 20, 0))).draw(d);

        // Score
        let mut buf = [0u8; 8];
        let s = fmt_u32(&mut buf, self.score);
        let _ = Text::with_alignment(s, EgPoint::new(W / 2, 40),
            MonoTextStyle::new(&FONT_10X20, Rgb565::WHITE), Alignment::Center).draw(d);

        if self.game_over {
            let _ = Rectangle::new(EgPoint::new(60, 210), Size::new(290, 80))
                .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK)).draw(d);
            let _ = Text::with_alignment("GAME OVER", EgPoint::new(W / 2, 240),
                MonoTextStyle::new(&FONT_10X20, Rgb565::RED), Alignment::Center).draw(d);
            let _ = Text::with_alignment("TAP TO RETRY", EgPoint::new(W / 2, 270),
                MonoTextStyle::new(&FONT_10X20, Rgb565::CSS_GRAY), Alignment::Center).draw(d);
        }
    }
}

fn fmt_u32<'a>(buf: &'a mut [u8; 8], val: u32) -> &'a str {
    let mut p = 0;
    if val >= 100 { buf[p] = b'0' + (val / 100 % 10) as u8; p += 1; }
    if val >= 10 { buf[p] = b'0' + (val / 10 % 10) as u8; p += 1; }
    buf[p] = b'0' + (val % 10) as u8; p += 1;
    core::str::from_utf8(&buf[..p]).unwrap_or("0")
}
