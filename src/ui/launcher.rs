// App Launcher - scrollable list of games
// Tap to select, swipe up/down to scroll smoothly, swipe right to go back

use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{PrimitiveStyle, Rectangle, RoundedRectangle};
use embedded_graphics::mono_font::ascii::FONT_10X20;
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::text::{Alignment, Text};

use crate::apps::AppState;
use crate::peripherals::touch::SwipeDirection;

const ITEM_H: i32 = 65;
const ITEM_GAP: i32 = 6;
const MARGIN_X: i32 = 20;
const START_Y: i32 = 55;
const SCREEN_W: i32 = 410;

struct MenuItem {
    name: &'static str,
    state: AppState,
    bg_color: Rgb565,
    text_color: Rgb565, // Contrast-aware text color
}

const MENU_ITEMS: &[MenuItem] = &[
    MenuItem { name: "Snake", state: AppState::Snake, bg_color: Rgb565::new(2, 20, 2), text_color: Rgb565::GREEN },
    MenuItem { name: "2048", state: AppState::Game2048, bg_color: Rgb565::new(15, 10, 0), text_color: Rgb565::YELLOW },
    MenuItem { name: "Tetris", state: AppState::Tetris, bg_color: Rgb565::new(0, 10, 15), text_color: Rgb565::CYAN },
    MenuItem { name: "Flappy Bird", state: AppState::Flappy, bg_color: Rgb565::new(15, 12, 0), text_color: Rgb565::WHITE },
    MenuItem { name: "Maze (Tilt)", state: AppState::Maze, bg_color: Rgb565::new(2, 4, 15), text_color: Rgb565::WHITE },
    MenuItem { name: "Settings", state: AppState::Settings, bg_color: Rgb565::new(6, 12, 6), text_color: Rgb565::WHITE },
];

pub struct Launcher {
    scroll_offset: i32,
    target_scroll: i32, // smooth scroll target
}

impl Launcher {
    pub fn new() -> Self {
        Self { scroll_offset: 0, target_scroll: 0 }
    }

    pub fn update(&mut self, swipe: Option<SwipeDirection>, tap: bool, tap_y: u16) -> Option<AppState> {
        let max_scroll = ((MENU_ITEMS.len() as i32) * (ITEM_H + ITEM_GAP) - 400).max(0);

        match swipe {
            Some(SwipeDirection::Up) => {
                self.target_scroll = (self.target_scroll + 120).min(max_scroll);
            }
            Some(SwipeDirection::Down) => {
                self.target_scroll = (self.target_scroll - 120).max(0);
            }
            Some(SwipeDirection::Right) => {
                return Some(AppState::Watchface);
            }
            _ => {}
        }

        // Smooth scroll interpolation
        let diff = self.target_scroll - self.scroll_offset;
        if diff.abs() > 2 {
            self.scroll_offset += diff / 3;
        } else {
            self.scroll_offset = self.target_scroll;
        }

        // Tap detection
        if tap {
            let y = tap_y as i32 + self.scroll_offset;
            for (i, item) in MENU_ITEMS.iter().enumerate() {
                let item_y = START_Y + i as i32 * (ITEM_H + ITEM_GAP);
                if y >= item_y && y < item_y + ITEM_H {
                    return Some(item.state);
                }
            }
        }
        None
    }

    pub fn render<D: DrawTarget<Color = Rgb565>>(&self, d: &mut D) {
        let _ = Rectangle::new(Point::zero(), Size::new(410, 502))
            .into_styled(PrimitiveStyle::with_fill(Rgb565::new(1, 2, 2)))
            .draw(d);

        // Title
        let title = MonoTextStyle::new(&FONT_10X20, Rgb565::WHITE);
        let _ = Text::with_alignment("APPS", Point::new(205, 35), title, Alignment::Center).draw(d);

        // Menu items
        for (i, item) in MENU_ITEMS.iter().enumerate() {
            let y = START_Y + i as i32 * (ITEM_H + ITEM_GAP) - self.scroll_offset;
            if y + ITEM_H < 0 || y > 502 { continue; }

            // Dark background with colored accent
            let _ = RoundedRectangle::with_equal_corners(
                Rectangle::new(Point::new(MARGIN_X, y), Size::new((SCREEN_W - 2 * MARGIN_X) as u32, ITEM_H as u32)),
                Size::new(12, 12),
            ).into_styled(PrimitiveStyle::with_fill(item.bg_color)).draw(d);

            // Colored left accent bar
            let _ = Rectangle::new(Point::new(MARGIN_X, y + 8), Size::new(4, (ITEM_H - 16) as u32))
                .into_styled(PrimitiveStyle::with_fill(item.text_color)).draw(d);

            // Item name with contrast-aware color
            let text_style = MonoTextStyle::new(&FONT_10X20, item.text_color);
            let _ = Text::with_alignment(
                item.name,
                Point::new(SCREEN_W / 2, y + ITEM_H / 2 + 5),
                text_style,
                Alignment::Center,
            ).draw(d);
        }
    }
}
