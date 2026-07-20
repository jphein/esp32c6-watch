#![no_std]
#![no_main]

// Slint UI demo for the Waveshare ESP32-C6-Touch-AMOLED-2.06.
//
// A Slint-rendered watchface (time from the PCF85063 RTC) drawn with the
// Slint software renderer directly to the CO5300 AMOLED over QSPI.
//
// RAM strategy: no framebuffer. The software renderer paints line-by-line
// into a 2-line RGB565 strip (410 x 2 x 2 bytes = 1640 B) which is streamed
// to the panel's internal GRAM. Two lines per flush because the CO5300
// requires a minimum 2x2 address window.
//
// Build: cargo build --release --bin slint-demo

extern crate alloc;

// Reuse the existing firmware modules without touching src/main.rs.
// (Crate-root file: `#[path]` is relative to src/bin/, and inside inline
// modules the inline module name is appended to the base directory.)
#[path = "../board.rs"]
#[allow(dead_code)]
mod board;

#[path = "../drivers"]
#[allow(dead_code)] // shared driver modules; the demo uses a subset of their API
mod drivers {
    pub mod co5300;
    pub mod qspi_bus;
}

#[path = "../peripherals"]
#[allow(dead_code)]
mod peripherals {
    pub mod power;
    pub mod rtc;
    pub mod touch;
}

use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};

use embassy_executor::Spawner;
use embassy_time::{Duration, Timer};
use embedded_hal_bus::i2c::RefCellDevice;
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    dma::{DmaRxBuf, DmaTxBuf},
    dma_buffers,
    gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull},
    i2c::master::{Config as I2cConfig, I2c},
    spi::{
        master::{Config as SpiConfig, Spi},
        Mode as SpiMode,
    },
    time::Rate,
    timer::timg::TimerGroup,
};
use esp_println::println;

use slint::platform::software_renderer::{
    LineBufferProvider, MinimalSoftwareWindow, RepaintBufferType, Rgb565Pixel,
};
use slint::platform::{Platform, PointerEventButton, WindowAdapter, WindowEvent};

use crate::drivers::co5300::Co5300Display;
use crate::drivers::qspi_bus::QspiBus;
use crate::peripherals::power::Axp2101Power;
use crate::peripherals::rtc::Pcf85063aRtc;
use crate::peripherals::touch::{Ft3168Touch, SwipeDirection};

// Pulls in the `WatchFace` component compiled by slint-build from
// src/bin/watchface.slint (see build.rs).
slint::include_modules!();

esp_bootloader_esp_idf::esp_app_desc!();

const WIDTH: usize = board::LCD_WIDTH as usize; // 410
const HEIGHT: usize = board::LCD_HEIGHT as usize; // 502

// === Slint platform integration ===================================

struct EspPlatform {
    window: Rc<MinimalSoftwareWindow>,
}

impl Platform for EspPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.window.clone())
    }

    fn duration_since_start(&self) -> core::time::Duration {
        core::time::Duration::from_micros(embassy_time::Instant::now().as_micros())
    }
}

/// LineBufferProvider that batches two rendered lines per panel write,
/// because the CO5300 rejects address windows smaller than 2x2 pixels.
struct TwoLineFlusher<'a, 'd> {
    display: &'a mut Co5300Display<'d>,
    /// 2 x WIDTH pixels: line A in the first half, line B in the second.
    buf: &'a mut [Rgb565Pixel],
    /// Raw u16 staging for the QSPI bus.
    scratch: &'a mut [u16],
    /// y of the line waiting in the first half of `buf`, if any.
    pending: Option<usize>,
}

impl TwoLineFlusher<'_, '_> {
    /// Send `buf` (two lines) to rows `y` and `y + 1`.
    fn flush_two(&mut self, y: usize) {
        for (dst, src) in self.scratch.iter_mut().zip(self.buf.iter()) {
            *dst = src.0;
        }
        self.display
            .set_addr_window(0, y as u16, WIDTH as u16, 2);
        self.display.bus_mut().write_pixels(self.scratch);
    }

    /// Flush a leftover single line by duplicating it into a 2-row window.
    /// (Never hit in practice: with a full-frame repaint all 502 lines come
    /// in consecutively and 502 is even.)
    fn flush_pending(&mut self) {
        if let Some(y) = self.pending.take() {
            let (first, second) = self.buf.split_at_mut(WIDTH);
            second.copy_from_slice(first);
            let y = y.min(HEIGHT - 2); // keep the 2-row window on the panel
            self.flush_two(y);
        }
    }
}

impl LineBufferProvider for &mut TwoLineFlusher<'_, '_> {
    type TargetPixel = Rgb565Pixel;

    fn process_line(
        &mut self,
        line: usize,
        range: core::ops::Range<usize>,
        render_fn: impl FnOnce(&mut [Self::TargetPixel]),
    ) {
        // Decide which half of the strip this line goes into.
        let second_half = match self.pending {
            Some(p) if line == p + 1 => true,
            Some(_) => {
                // Non-consecutive line: emit the stragglers first.
                self.flush_pending();
                false
            }
            None => false,
        };

        let offset = if second_half { WIDTH } else { 0 };
        let dst = &mut self.buf[offset..offset + WIDTH];
        if range.start != 0 || range.end != WIDTH {
            // Partial dirty range: blank the rest of the strip line so we
            // never push stale pixels (full repaints make this a no-op).
            dst.fill(Rgb565Pixel(0));
        }
        render_fn(&mut dst[range]);

        if second_half {
            let y = self.pending.take().unwrap();
            self.flush_two(y);
        } else {
            self.pending = Some(line);
        }
    }
}

// === Helpers =======================================================

const WEEKDAYS: [&str; 7] = ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];
const MONTHS: [&str; 12] = [
    "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
];

fn date_string(dt: &crate::peripherals::rtc::DateTime) -> slint::SharedString {
    let weekday = WEEKDAYS[(dt.weekday % 7) as usize];
    let month = MONTHS[(dt.month.clamp(1, 12) - 1) as usize];
    slint::format!("{} {:02} {} 20{:02}", weekday, dt.day, month, dt.year)
}

fn uptime_string() -> slint::SharedString {
    let s = embassy_time::Instant::now().as_secs();
    slint::format!("{}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
}

/// Map the UI slider fraction (0.0..1.0) onto the CO5300 brightness range,
/// keeping a floor so the slider can never black the panel out completely.
const BRIGHTNESS_MIN: u8 = 0x10;
fn brightness_raw(frac: f32) -> u8 {
    let frac = frac.clamp(0.0, 1.0);
    BRIGHTNESS_MIN + (frac * (0xFF - BRIGHTNESS_MIN) as f32) as u8
}

/// y-band of the brightness slider on the stats page (see watchface.slint):
/// horizontal swipes starting here are slider drags, not page switches.
const SLIDER_BAND: core::ops::RangeInclusive<u16> = 330..=430;

// === Entry point ===================================================

#[esp_rtos::main]
async fn main(_spawner: Spawner) -> ! {
    esp_println::logger::init_logger_from_env();

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    // Same heap layout as the main firmware; Slint allocates its scene and
    // strings here. No framebuffer, so there is plenty of headroom.
    esp_alloc::heap_allocator!(size: 240 * 1024);
    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 56 * 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_interrupt =
        esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_interrupt.software_interrupt0);

    println!("=== slint-demo (C6 AMOLED, Slint software renderer) ===");

    // === I2C bus (PCF85063 RTC at 0x51, SDA=GPIO8 SCL=GPIO7) ===
    let i2c = I2c::new(
        peripherals.I2C0,
        I2cConfig::default().with_frequency(Rate::from_khz(board::I2C_FREQ_HZ / 1000)),
    )
    .expect("I2C failed")
    .with_sda(peripherals.GPIO8)
    .with_scl(peripherals.GPIO7);
    let i2c_ref = RefCell::new(i2c);

    let mut rtc = Pcf85063aRtc::new(RefCellDevice::new(&i2c_ref));
    let _ = rtc.init();
    println!("[RTC] OK");

    // === Power (AXP2101 at 0x34, same bus; read-mostly: rails untouched) ===
    let mut power = Axp2101Power::new(RefCellDevice::new(&i2c_ref));
    let _ = power.enable_adc();
    println!("[POWER] OK");

    // === Display: CO5300 over QSPI DMA at 80MHz ===
    let spi_config = SpiConfig::default()
        .with_frequency(Rate::from_mhz(80))
        .with_mode(SpiMode::_0);
    let (rx_buf, rx_desc, tx_buf, tx_desc) = dma_buffers!(8000);
    let dma_rx = DmaRxBuf::new(rx_desc, rx_buf).unwrap();
    let dma_tx = DmaTxBuf::new(tx_desc, tx_buf).unwrap();
    let spi = Spi::new(peripherals.SPI2, spi_config)
        .expect("SPI failed")
        .with_sck(peripherals.GPIO0)
        .with_sio0(peripherals.GPIO1)
        .with_sio1(peripherals.GPIO2)
        .with_sio2(peripherals.GPIO3)
        .with_sio3(peripherals.GPIO4)
        .with_dma(peripherals.DMA_CH0)
        .with_buffers(dma_rx, dma_tx);
    let cs = Output::new(peripherals.GPIO5, Level::High, OutputConfig::default());
    let reset = Output::new(peripherals.GPIO11, Level::High, OutputConfig::default());
    let mut display = Co5300Display::new(QspiBus::new(spi, cs), reset);
    display.init();
    println!("[DISPLAY] OK");

    // === Touch (FT3168 at 0x38, same bus; INT=GPIO15, RST=GPIO10) ===
    let mut touch_rst = Output::new(peripherals.GPIO10, Level::High, OutputConfig::default());
    let touch_int = Input::new(
        peripherals.GPIO15,
        InputConfig::default().with_pull(Pull::Up),
    );
    touch_rst.set_low();
    Timer::after(Duration::from_millis(10)).await;
    touch_rst.set_high();
    Timer::after(Duration::from_millis(50)).await;
    let mut touch = Ft3168Touch::new(RefCellDevice::new(&i2c_ref));
    let _ = touch.init();
    println!("[TOUCH] OK");

    // === Slint platform ===
    // NewBuffer: our 2-line strip never holds the previous frame, so ask the
    // renderer to repaint everything each time it draws.
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    window.set_size(slint::PhysicalSize::new(WIDTH as u32, HEIGHT as u32));
    slint::platform::set_platform(Box::new(EspPlatform {
        window: window.clone(),
    }))
    .expect("set_platform failed");

    let ui = WatchFace::new().expect("failed to create WatchFace");

    // Radio glyphs: static for now, the demo carries no radio stack.
    ui.set_wifi_on(false);
    ui.set_ble_on(false);
    ui.set_mesh_peers(0);
    ui.set_steps(0); // placeholder: no pedometer in the demo

    // Brightness requests flow out of the Slint callback through this cell
    // and are applied in the loop, where `display` is mutably borrowable.
    let brightness_req: Rc<Cell<Option<u8>>> = Rc::new(Cell::new(None));
    {
        let req = brightness_req.clone();
        ui.on_brightness_changed(move |frac| req.set(Some(brightness_raw(frac))));
    }

    ui.show().expect("show failed");
    println!("[SLINT] UI up, entering render loop");

    // 2-line render strip + u16 staging buffer (1640 B each half's worth).
    let mut line_buf: Vec<Rgb565Pixel> = alloc::vec![Rgb565Pixel(0); WIDTH * 2];
    let mut scratch: Vec<u16> = alloc::vec![0u16; WIDTH * 2];

    let mut last_second: u8 = 0xFF;
    let mut batt_countdown: u8 = 0; // battery poll every 5 second-ticks
    let mut touch_down = false;
    let mut last_pos = slint::LogicalPosition::new(0.0, 0.0);

    loop {
        // === Touch -> Slint pointer events + page swipes ===
        // The FT3168 INT line is low while a finger is down; keep polling
        // one extra round after it releases so the up-edge is delivered.
        if touch_int.is_low() || touch_down {
            if let Ok((point, swipe)) = touch.poll() {
                if let Some(tp) = point {
                    let pos = slint::LogicalPosition::new(tp.x as f32, tp.y as f32);
                    let event = if touch_down {
                        WindowEvent::PointerMoved { position: pos }
                    } else {
                        WindowEvent::PointerPressed {
                            position: pos,
                            button: PointerEventButton::Left,
                        }
                    };
                    touch_down = true;
                    last_pos = pos;
                    let _ = window.window().try_dispatch_event(event);
                } else if touch_down {
                    touch_down = false;
                    let _ = window.window().try_dispatch_event(WindowEvent::PointerReleased {
                        position: last_pos,
                        button: PointerEventButton::Left,
                    });
                }
                if let Some(sw) = swipe {
                    // A horizontal drag that started on the brightness slider
                    // is a slider adjustment, not a page switch.
                    let on_slider =
                        ui.get_current_page() == 1 && SLIDER_BAND.contains(&sw.start_y);
                    if !on_slider {
                        match sw.direction {
                            SwipeDirection::Left => ui.set_current_page(1),
                            SwipeDirection::Right => ui.set_current_page(0),
                            _ => {}
                        }
                    }
                }
            }
        }

        // Apply any brightness change requested by the UI callback.
        if let Some(raw) = brightness_req.take() {
            display.set_brightness(raw);
        }

        slint::platform::update_timers_and_animations();

        // Poll the RTC; push new values into the UI when the second ticks.
        if let Ok(dt) = rtc.get_time() {
            if dt.seconds != last_second {
                last_second = dt.seconds;
                ui.set_time_text(slint::format!("{:02}:{:02}", dt.hours, dt.minutes));
                ui.set_seconds_text(slint::format!("{:02}", dt.seconds));
                ui.set_date_text(date_string(&dt));
                ui.set_minute_progress(dt.seconds as f32 / 59.0);
                ui.set_uptime_text(uptime_string());

                // Battery telemetry is slow-moving: poll every 5 ticks.
                if batt_countdown == 0 {
                    batt_countdown = 5;
                    if let Ok(pct) = power.get_battery_percent() {
                        ui.set_battery_percent(pct.min(100) as i32);
                    }
                    ui.set_battery_mv(power.get_battery_voltage().unwrap_or(0) as i32);
                    ui.set_charging(power.is_charging().unwrap_or(false));
                }
                batt_countdown -= 1;
            }
        }

        window.draw_if_needed(|renderer| {
            let mut flusher = TwoLineFlusher {
                display: &mut display,
                buf: &mut line_buf,
                scratch: &mut scratch,
                pending: None,
            };
            renderer.render_by_line(&mut flusher);
            flusher.flush_pending();
        });

        if touch_down || touch_int.is_low() {
            // Tight poll while a finger is down so drags feel live.
            Timer::after(Duration::from_millis(20)).await;
        } else if window.has_active_animations() {
            Timer::after(Duration::from_millis(33)).await;
        } else {
            Timer::after(Duration::from_millis(100)).await;
        }
    }
}
