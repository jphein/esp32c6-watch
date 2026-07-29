#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]

// Waveshare ESP32-C6-Touch-AMOLED-2.06 firmware, ported from
// infinition/waveshare-watch-rs (ESP32-S3-Touch-AMOLED-2.06).
// C6 differences: no PSRAM (RGB332 framebuffer in SRAM), no SD card slot,
// no TE pin wired in the BSP, different GPIO map (see board.rs).
// Radio: WiFi STA + NTP and BLE advertising, both OFF at boot, toggled from
// the watchface buttons. Set WIFI_SSID / WIFI_PASS env vars at build time.
// Not yet ported: mp3player/smarthome apps, DVFS.

mod board;
mod drivers;
mod guarded_flash;
mod net;
mod notify;
mod peripherals;
mod ui;
mod apps;
// UI test automator (feature `debug-console`, on by default): drive + measure
// the UI over the USB-Serial-JTAG RX. See src/debug_console.rs.
#[cfg(feature = "debug-console")]
mod debug_console;

use core::cell::RefCell;

use embassy_executor::Spawner;
use embassy_futures::join::join;
// Both build variants use fully-qualified embassy_futures::select at the main
// wake point (the debug-console build nests select for the synthetic-input wake).
use embassy_time::{Duration, Instant, Timer};
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::RgbColor;
use embedded_hal_bus::i2c::RefCellDevice;
use esp_backtrace as _;

include!("panic_reboot.rs");
use esp_hal::{
    clock::CpuClock,
    delay::Delay,
    dma::DmaDescriptor,
    gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull, WakeEvent},
    i2c::master::{Config as I2cConfig, I2c},
    i2s::master::{Config as I2sConfig, DataFormat, I2s},
    rtc_cntl::{
        sleep::{GpioWakeupSource, TimerWakeupSource},
        wakeup_cause, Rtc,
    },
    spi::{
        master::{Config as SpiConfig, Spi},
        Mode as SpiMode,
    },
    time::Rate,
    timer::timg::TimerGroup,
};
use esp_println::println;
use esp_radio::ble::controller::BleConnector;
use static_cell::StaticCell;

use crate::apps::flappy::FlappyGame;
use crate::apps::game2048::Game2048;
use crate::apps::maze::MazeGame;
use crate::apps::snake::SnakeGame;
use crate::apps::tetris::TetrisGame;
use crate::apps::world_snake::WorldSnakeApp;
use crate::apps::{App, AppInput, AppResult, AppState, Sfx};
use crate::drivers::co5300::Co5300Display;
use crate::net::familiar::FamUi;
use crate::net::smol_mesh::{MeshEvent, MESH_MAX_ROWS, PeerView, SmolMesh};
use crate::net::voice_stt;
#[cfg(feature = "tts")]
use crate::net::voice_tts;
use crate::drivers::framebuffer::Framebuffer;
use crate::drivers::qspi_bus::QspiBus;
use crate::peripherals::audio::Es8311;
use crate::peripherals::audio_out;
use crate::peripherals::es7210::Es7210;
use crate::peripherals::die_temp::DieTemp;
use crate::peripherals::imu::Qmi8658Imu;
use crate::peripherals::mic_capture;
use crate::peripherals::power::{Axp2101Power, PowerKey};
use crate::peripherals::power_stats::{DisplayState, PowerStats, WifiMode};
use crate::peripherals::rtc::{DateTime, Pcf85063aRtc};
use crate::peripherals::touch::{Ft3168Touch, SwipeDirection};
use crate::ui::slint_shell::{self, ShellUi};

extern crate alloc;

esp_bootloader_esp_idf::esp_app_desc!();

/// The embassy-net stack runner (smoltcp poll loop). Distinct from
/// `net::net_task::net_task`, the #53 network OWNER that drives the WiFi
/// controller/scan/burst/OTA — this one just pumps packets.
#[embassy_executor::task]
async fn net_stack_task(
    mut runner: embassy_net::Runner<'static, esp_radio::wifi::Interface<'static>>,
) -> ! {
    runner.run().await
}

// #58 climate: the real `climate-model` crate (oracle-t9 CONFIRMED-CLEAN @5c0d04c;
// stub swapped out). Provides HvacMode / ClimateState.
use climate_model;

/// Map the UI's hvac-mode int (0..5, from the ClimateOverlay segmented control)
/// back to the model enum for a `SetMode` command.
fn hvac_from_ui(m: i32) -> climate_model::HvacMode {
    use climate_model::HvacMode as H;
    match m {
        1 => H::Heat,
        2 => H::Cool,
        3 => H::Auto,
        4 => H::FanOnly,
        5 => H::Dry,
        _ => H::Off,
    }
}

/// #58: holds the long-lived HA climate MQTT session while the Climate screen is
/// up. Waits for `open`, runs the session until `close` (or an error) ends it,
/// then signals `done` — on BOTH the Ok and Err paths — so main.rs releases the
/// WiFi hold + returns to mesh unconditionally (oracle-t10 invariant b: an error
/// return must never leave WiFi held).
#[embassy_executor::task]
async fn climate_task(
    stack: embassy_net::Stack<'static>,
    state: &'static crate::net::mqtt_climate::ClimateStateMutex,
    energy: &'static crate::net::mqtt_climate::EnergyStateMutex,
    lights: &'static crate::net::mqtt_climate::LightsStateMutex,
    cmd_rx: crate::net::mqtt_climate::ClimateCmdReceiver,
    open: &'static crate::net::mqtt_climate::CloseSignal,
    close: &'static crate::net::mqtt_climate::CloseSignal,
    done: &'static crate::net::mqtt_climate::CloseSignal,
) {
    // Consecutive-error counter for progressive backoff: the FIRST failure of a
    // screen visit retries fast (a cold open races DHCP/route settling — a 10s
    // flat pause here was most of the "Finding your room… forever" feel), while
    // repeat failures back off to the storm-safe 10s.
    let mut consec_errs: u32 = 0;
    loop {
        open.wait().await;
        // One session feeds the Climate + Energy + Lights screens (shared CONNECT).
        let res = crate::net::mqtt_climate::run_climate_session(
            stack, state, energy, lights, cmd_rx, close,
        )
        .await;
        // Phase is owned here (the only caller): DOWN on every exit path —
        // BEFORE the backoff sleep — so a press during the backoff window is
        // rejected with feedback instead of queueing a stale replay
        // (see SESSION_PHASE docs).
        crate::net::mqtt_climate::SESSION_PHASE.store(
            crate::net::mqtt_climate::PHASE_DOWN,
            core::sync::atomic::Ordering::Relaxed,
        );
        match res {
            Ok(()) => consec_errs = 0,
            Err(e) => {
                consec_errs = consec_errs.saturating_add(1);
                // Reconnect backoff. main.rs re-signals `open` as soon as `done` clears
                // `climate_running` (session_want && wifi_connected && !running). With a
                // broker the watch can't reach (e.g. VLAN-6 mosquitto firewalled off the
                // roam VLAN-11), the session fails `tcp connect` in ~2s — so without
                // a pause here `open` would re-fire every ~2s: a tight reconnect storm
                // that keeps WiFi held (mesh starved) and pins the radio. Progressive:
                // 2s on the first failure (transient boot/DHCP races recover fast),
                // 10s from the second on (real outages stay storm-safe). Only on the
                // Err path — a clean close signals `done` at once.
                let backoff = if consec_errs == 1 { 2 } else { 10 };
                println!("[CLIM] session ended: {e} (retry in {backoff}s)");
                embassy_time::Timer::after(embassy_time::Duration::from_secs(backoff)).await;
            }
        }
        done.signal(()); // fires on Ok AND Err → main restores mesh unconditionally
    }
}

fn days_to_date(days_since_epoch: i32) -> (u32, u32, u32) {
    let mut y = 1970i32;
    let mut remaining = days_since_epoch;
    loop {
        let days_in_year = if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) {
            366
        } else {
            365
        };
        if remaining < days_in_year {
            break;
        }
        remaining -= days_in_year;
        y += 1;
    }
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let month_days = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut m = 0;
    while m < 12 && remaining >= month_days[m] {
        remaining -= month_days[m];
        m += 1;
    }
    (y as u32, (m + 1) as u32, (remaining + 1) as u32)
}

/// US Pacific offset in seconds for a given UTC day (PST8PDT).
/// DST runs from the second Sunday of March to the first Sunday of November.
/// The 2:00-local transition hour is ignored — a watch clock being an hour
/// off for part of one night twice a year is acceptable for v1.
fn us_pacific_offset_secs(days_since_epoch: i32) -> i64 {
    let (_y, m, d) = days_to_date(days_since_epoch);
    let wd = ((days_since_epoch % 7) + 4) % 7; // 0=Sunday (1970-01-01 was a Thursday)
    let d = d as i32;
    let dst = match m {
        4..=10 => true,
        3 => d - wd >= 8,  // on/after the second Sunday
        11 => d - wd < 1,  // before the first Sunday
        _ => false,
    };
    if dst {
        -7 * 3600
    } else {
        -8 * 3600
    }
}

/// #43: make the RTC sleep-clock calibration work on ESP32-C6 rev >= v0.1, and
/// probe whether light sleep is safe to enter at all.
///
/// esp-hal 1.1.1 panics `attempt to divide by zero` at
/// `rtc_cntl/sleep/esp32c6.rs:665` (`us_to_fastclk`) when entering light sleep
/// on newer C6 silicon. Every `sleep_light()` re-runs an RC_FAST calibration
/// (`RtcClock::calibrate(RcFastDivClk, 2048)`) on the TIMG0 one-shot counter.
/// On rev >= v0.1 the calibration mux taps RC_FAST through the REF_TICK /32
/// divider, which only ticks when `PCR.ctrl_tick_conf.{fosc_tick_num,
/// tick_enable}` are programmed — esp-idf sets both (`clk_ll_rc_fast_tick_conf`
/// + `rtc_clk_cal`), esp-hal never does on C6 (its C6 metadata lacks
/// `tick_enable`; the H2 flavor was fixed after esp-rs/esp-hal#5321, C6 was
/// left out — still missing on esp-hal main as of 2026-07). The counter never
/// advances, the hardware timeout fires, `calibrate()` returns 0, and the
/// sleep math divides by it. Rev v0.0 parts tap RC_FAST directly — which is
/// why watch #1 sleeps fine while factory-fresh watch #2 (newer silicon)
/// panics deterministically. NOT the LP_AON STORE1/slowclk read: that divisor
/// is exercised first (esp32c6.rs:560 -> :657) and would report line 657.
///
/// Mirrors esp-idf's calibration prep via the PAC (esp-hal `unstable` feature
/// re-exports register blocks as `esp_hal::peripherals::<P>::regs()`):
///   1. assert the RC_FAST gates (POR-default on, but they live in the
///      battery-held LP domain, so previous firmware can leave them off);
///   2. program + enable the REF_TICK divider and LEAVE it on — esp-hal's
///      sleep-entry calibration never touches these registers on C6;
///   3. sanity-check LP_AON.STORE1 (RTC slow-clock period, Q19 µs-per-cycle —
///      the *other* sleep-math divisor; esp-hal's boot calibration writes it
///      but stores 0 on failure) and write a nominal RC_SLOW default if bad;
///   4. dry-run the exact TIMG0 one-shot calibration esp-hal will run at sleep
///      entry; only a non-zero result enables light sleep (else the AOD path
///      stays on the select-tick loop, like debug-console builds).
///
/// Returns `true` when light sleep is safe (calibration hardware works).
fn rtc_sleep_cal_init(delay: &Delay) -> bool {
    use esp_hal::peripherals::{LP_AON, LP_CLKRST, PCR, PMU, TIMG0};

    // (1) RC_FAST (FOSC, ~17.5 MHz) analog enable + digital gate — the same
    // two registers esp-hal's `enable_rc_fast_clk_impl` drives; the rc_fast
    // clock-tree node is never requested on C6, so nothing else asserts them.
    PMU::regs()
        .hp_sleep_lp_ck_power()
        .modify(|_, w| w.hp_sleep_xpd_fosc_clk().set_bit());
    LP_CLKRST::regs()
        .clk_to_hp()
        .modify(|_, w| w.icg_hp_fosc().set_bit());
    delay.delay_micros(5); // oscillator settle (esp-hal/esp-idf use the same 5 µs)

    // (2) REF_TICK /32 divider feeding the calibration mux on rev >= v0.1.
    // fosc_tick_num=255 == esp-idf `clk_ll_rc_fast_tick_conf()`; tick_enable ==
    // esp-idf `SET_PERI_REG_MASK(PCR_CTRL_TICK_CONF_REG, PCR_TICK_ENABLE)`.
    // Harmless on rev v0.0 (the divider is bypassed there).
    PCR::regs().ctrl_tick_conf().modify(|_, w| unsafe {
        w.fosc_tick_num().bits(255);
        w.tick_enable().set_bit()
    });

    // (3) STORE1 holds the RTC slow-clock period as Q19 µs-per-cycle
    // (us_to_slowclk: `(us << 19) / period`; slowclk_to_us: `(cyc * period) >>
    // 19`). Plausibility band 0.19 µs..38 µs covers 32 kHz XTAL (16.0M) and
    // RC_SLOW (3.86M). Nominal default: 1e6/136_000 µs << 19 = 3_855_059
    // (C6 RC_SLOW ~= 136 kHz). RC drift (±20%) only skews sleep *duration*;
    // wall time is re-read from the external PCF85063 after every wake.
    const STORE1_MIN: u32 = 100_000;
    const STORE1_MAX: u32 = 20_000_000;
    const RC_SLOW_PERIOD_Q19: u32 = 3_855_059;
    let store1 = LP_AON::regs().store1().read().data().bits();
    if !(STORE1_MIN..=STORE1_MAX).contains(&store1) {
        LP_AON::regs()
            .store1()
            .write(|w| unsafe { w.bits(RC_SLOW_PERIOD_Q19) });
        println!(
            "[RTC] slowclk cal absent — wrote default (STORE1={} -> {})",
            store1, RC_SLOW_PERIOD_Q19
        );
    } else {
        println!("[RTC] slowclk cal present: {}", store1);
    }

    // (4) Dry-run the RC_FAST_DIV one-shot calibration exactly as esp-hal's
    // `measure_rtc_clock` does inside `sleep_light()`: TIMG0 rtccalicfg,
    // cali_clk_sel=1 (RC_FAST_DIV). 64 cycles ~= 117 µs of fosc/32 on
    // rev >= v0.1 (or ~4 µs of direct fosc on v0.0). The rtccalicfg2 hardware
    // timeout bounds the wait; a software spin cap backs it up.
    let timg0 = TIMG0::regs();
    if timg0.rtccalicfg().read().rtc_cali_start_cycling().bit_is_set() {
        // A cycling calibration is mid-flight (POR default) — drain it first,
        // same dance as esp-hal/esp-idf.
        timg0
            .rtccalicfg2()
            .modify(|_, w| unsafe { w.rtc_cali_timeout_thres().bits(1) });
        while !timg0.rtccalicfg().read().rtc_cali_rdy().bit_is_set()
            && !timg0.rtccalicfg2().read().rtc_cali_timeout().bit_is_set()
        {}
    }
    timg0.rtccalicfg2().reset();
    // Expected completion ~4.7k XTAL(40 MHz) cycles; allow 400k (10 ms).
    timg0
        .rtccalicfg2()
        .modify(|_, w| unsafe { w.rtc_cali_timeout_thres().bits(400_000) });
    timg0.rtccalicfg().modify(|_, w| unsafe {
        w.rtc_cali_start_cycling().clear_bit();
        w.rtc_cali_clk_sel().bits(1); // 1 = RC_FAST_DIV, the sleep path's fastclk
        w.rtc_cali_max().bits(64);
        w.rtc_cali_start().clear_bit()
    });
    timg0
        .rtccalicfg()
        .modify(|_, w| w.rtc_cali_start().set_bit());
    let mut cal_value = 0u32;
    let mut spins = 0u32;
    loop {
        if timg0.rtccalicfg().read().rtc_cali_rdy().bit_is_set() {
            cal_value = timg0.rtccalicfg1().read().rtc_cali_value().bits();
            break;
        }
        if timg0.rtccalicfg2().read().rtc_cali_timeout().bit_is_set() {
            break; // counter never ticked — RC_FAST calibration dead on this unit
        }
        spins += 1;
        if spins > 40_000_000 {
            break; // paranoia cap (~seconds); treat as failed
        }
    }
    timg0
        .rtccalicfg()
        .modify(|_, w| w.rtc_cali_start().clear_bit());

    if cal_value != 0 {
        println!(
            "[RTC] fastclk cal probe OK ({} xtal cyc / 64) — AOD light sleep enabled",
            cal_value
        );
        true
    } else {
        println!(
            "[RTC] fastclk cal probe TIMEOUT — AOD light sleep DISABLED (div-by-zero guard, #43)"
        );
        false
    }
}

/// Convert a Unix timestamp to Pacific local time and write it to the RTC.
fn set_rtc_from_unix(
    rtc: &mut crate::peripherals::rtc::Pcf85063aRtc<impl embedded_hal::i2c::I2c>,
    unix_secs: u32,
) -> (u8, u8, u8) {
    let utc_days = (unix_secs / 86400) as i32;
    let local_secs = unix_secs as i64 + us_pacific_offset_secs(utc_days);
    let time_of_day = (local_secs.rem_euclid(86400)) as u32;
    let hours = (time_of_day / 3600) as u8;
    let minutes = ((time_of_day % 3600) / 60) as u8;
    let seconds = (time_of_day % 60) as u8;
    let (year, month, day) = days_to_date(local_secs.div_euclid(86400) as i32);
    let dt = crate::peripherals::rtc::DateTime::new(
        (year - 2000) as u8,
        month as u8,
        day as u8,
        hours,
        minutes,
        seconds,
    );
    let _ = rtc.set_time(&dt);
    (hours, minutes, seconds)
}

/// Page label for the WATCH telemetry `scr` field, keyed by the Slint shell's
/// current-page index (shares the page order with ui/slint/shell.slint).
/// Resolve a mesh node to its per-device sigil (#35): the known fleet by id
/// ([`sigil_id::sigil_for_node`] — authoritative even if a frame is ever
/// relayed), else derived from the frame's source MAC (any watch, #34
/// derivation). Bounded copy into the roster-name buffer.
fn ping_sigil(from_id: u8, mac: [u8; 6]) -> heapless::String<{ sigil_id::SIGIL_MAX }> {
    let mut s = heapless::String::new();
    match crate::net::names::sigil_for_node(from_id) {
        Some(name) => {
            let _ = s.push_str(name);
        }
        None => {
            let _ = s.push_str(crate::net::names::sigil_for_mac(mac).as_str());
        }
    }
    s
}

fn page_scr_name(page: i32) -> &'static str {
    match page {
        1 => "SENSORS",
        2 => "SYSTEM",
        3 => "POWER",
        4 => "MESH",
        _ => "CLOCK",
    }
}

/// Optimistic "sent" tracking for the Lights hero (#39): stamped when a command
/// publish is queued; cleared when the retained state's `seq` moves past
/// `seq_at_send` (HA republishes after acting) or after a 5s no-reply timeout.
struct LightsPending {
    sent_at: Instant,
    seq_at_send: u32,
}

/// Optimistic-setpoint tracking for the Climate detail (oracle-t9 C4/C5/E2).
/// Holds the user's pending absolute target so the UI can reflect a ±tap
/// instantly (C4), the MQTT publish can be debounced ~400ms after the last tap
/// (C5), and the optimistic value can revert to authoritative state if HA does
/// not confirm within 5s (E2). Lives as a main-loop stack local — no .bss, so
/// it does not move the stack-floor guardrail.
struct ClimatePending {
    id: i32,
    temp: f32,
    last_tap: Instant,
    /// Set once the debounced SetTemp is published; also starts the 5s revert.
    sent_at: Option<Instant>,
}

/// Log per-region free heap at boot / app-enter. The framebuffer must come from
/// ONE region, so total-free (HEAP.free()) can read fine while the main region
/// alone is short. region_stats[0] = main (240KB pool), [1] = reclaimed (56KB).
/// `need` is the half-res fb footprint (205*251) so a launch log shows the
/// margin at a glance.
fn log_heap(tag: &str) {
    let stats = esp_alloc::HEAP.stats();
    let region_free = |i: usize| {
        stats.region_stats[i]
            .as_ref()
            .map(|r| r.free)
            .unwrap_or(0)
    };
    println!(
        "[HEAP] {}: main_free={} recl_free={} total_free={} need={}",
        tag,
        region_free(0),
        region_free(1),
        esp_alloc::HEAP.free(),
        (410usize / 2) * (502usize / 2),
    );
}

/// CFG key `R` boot debounce (reference main.rs REBOOT_DEBOUNCE_MS): within
/// this window a retained/re-armed reboot command is consumed but ignored,
/// so a stale `R` can never reboot-loop the watch.
const REBOOT_DEBOUNCE_MS: u64 = 10_000;

/// The one flash writer, shared between the main loop (config saves, OTA
/// mark-valid) and the OTA download (#53: moving into `net_task`). An async
/// mutex locked **per operation** — one config save, one 4 KB OTA chunk write —
/// never across a whole download, so a config save during an OTA waits at most
/// one sector program, and an OTA never waits on more than one save.
///
/// #55: the storage inside is a [`guarded_flash::GuardedFlash`], not a raw
/// `FlashStorage` — every write is range-checked against the booted app slot
/// and the bootloader/partition-table region, so no caller (present or
/// future) can corrupt the running image through this mutex.
pub type FlashMutex = embassy_sync::mutex::Mutex<
    embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex,
    guarded_flash::GuardedFlash,
>;

/// Persist the config record through the shared flash mutex. Returns whether
/// the save happened (offset known + write OK); callers own their log lines so
/// the per-site messages stay grep-identical to the pre-mutex code.
async fn cfg_save(
    flash: &'static FlashMutex,
    offset: Option<u32>,
    cfg: &peripherals::config::WatchConfig,
) -> bool {
    match offset {
        Some(off) => peripherals::config::save(&mut *flash.lock().await, off, cfg).is_ok(),
        None => false,
    }
}

// The one-shot SNTP query moved into `net::net_task` (#53) — the socket half
// runs there during the boot burst; the RTC/mesh-authority application stays
// in the main loop (it owns both), fed by `net_task::take_ntp_unix()`.

#[allow(clippy::too_many_arguments)]
fn update_power_stats(
    stats: &mut PowerStats,
    screen_state: u8,
    imu_on: bool,
    wifi_connected: bool,
    wifi_wanted: bool,
    brightness: u8,
    batt_mv: u16,
    batt_pct: u8,
    charging: bool,
) {
    stats.display = Some(match screen_state {
        0 => DisplayState::Off,
        1 => DisplayState::Aod,
        2 => DisplayState::Dim,
        _ => DisplayState::Bright,
    });
    stats.wifi = Some(if !wifi_wanted && !wifi_connected {
        WifiMode::Off
    } else if wifi_connected {
        WifiMode::PowerSave
    } else {
        WifiMode::Active
    });
    stats.imu_on = imu_on;
    stats.brightness = brightness;
    stats.audio_on = false;
    stats.sd_on = false;
    stats.battery_mv = batt_mv;
    stats.battery_pct = batt_pct;
    stats.charging = charging;
}

#[allow(
    clippy::large_stack_frames,
    reason = "it's not unusual to allocate larger buffers etc. in main"
)]
/// Master gate for AOD light sleep. **OFF**: `sleep_light` locks this hardware up
/// (see the long note at the entry condition in the main loop). Flip to `true`
/// only with a reproduction that survives a soak on a BLE-OFF watch.
const AOD_LIGHT_SLEEP: bool = false;

/// UI-loop heartbeat interval (#75). 15s is frequent enough to catch a wedge
/// inside a one-minute trial, sparse enough that it never dominates a log.
const BEAT_SECS: u64 = 15;

/// Quiet period after the last volume change before the config is written to
/// flash (#75). Slightly longer than the 2s volume-HUD dismissal, so a normal
/// adjustment persists once, just after the HUD closes, instead of once per step.
const CFG_SETTLE_MS: u64 = 2_500;

/// Hard ceiling on how long a pending config change may sit unwritten (#75).
/// The quiet-moment gate is the normal path; this guarantees termination so a
/// setting can never be silently lost to an audio path that stays busy.
const CFG_MAX_DEFER_S: u64 = 30;

#[esp_rtos::main]
async fn main(_spawner: Spawner) -> ! {
    esp_println::logger::init_logger_from_env();

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    // Heap / stack layout (esp32c6 memory.x). The RAM region is
    // 0x40800000..0x4086E610 (~441.5KB); the stack grows DOWN from 0x4086E610
    // (RAM top) and its floor is _bss_end, so `stack = 0x4086E610 - _bss_end`.
    // The main pool below is a static array in THIS region's .bss, so its size
    // sets _bss_end and therefore the stack ceiling. The half-res framebuffer
    // (~51KB, framebuffer.rs) + Slint scene + WiFi + BLE + mesh all draw from
    // this pool; it's the durable #35 game-launch-OOM fix (was 264KB interim).
    //
    // #58 crash fix (lucid root-cause, mechanism corrected by Morpheus): v0.5.0
    // added ~6.8KB of .bss (the fat climate_task future in its static TaskPool +
    // climate statics), pushing _bss_end up and dropping the gap-stack
    // 46.5KB→39.7KB. The WiFi-connect/WPA burst peaks in (39.7,46.5]KB, so it
    // overflowed into the WPA/MAC-RX statics above the stack → ppRecycleRxPkt
    // near-null store. Fix: trim the MAIN pool 240KB→228KB — that lowers
    // _bss_end ~12KB → stack ~51.6KB, ~5KB above v0.4.0's glass-proven 46.5KB,
    // clearing the whole overflow band. (Trimming the RECLAIMED pool CANNOT help:
    // it lives in dram2_seg at 0x4086E610.., ABOVE the stack ceiling, so its size
    // never moves _bss_end — measured: a 56→48KB trim dropped total .bss 8KB in
    // `size` but left _stack_end frozen.) Follow-up: box the session/voice socket
    // buffers so those futures leave .bss and the main pool can be restored.
    //
    // Voice-wire (#42/#28): the shared mic_capture adds ~14KB .bss (MIC_RING 8KB
    // StaticCell + MIC_CH channel + the capture-task future), which dropped the
    // gap-stack 51.6KB→37.9KB — under the 46KB guardrail (would fire at boot; the
    // #59 stack-floor tripwire caught it at measure-time). Trim the MAIN pool
    // 228KB→214KB to lower _bss_end ~14KB → stack back to ~51.6KB (v0.5.1 glass-
    // proven). #53's net_task .bss (+5.6KB) thinned the gap to 46.9KB and the
    // CONSOLIDATED shell (power menu + switcher + shade + spectrum) overflowed
    // the guard during WatchShell::new — caught by the wrong-creds acceptance
    // boot. Trimmed further 214→198KB (gap ≈ 63KB): scene-build peak clears
    // with margin; heap keeps ~38KB spare above the 51KB fb need (#35 gets the
    // RAM-busy toast fallback if squeezed). Real fix on the books: box the
    // session/voice socket buffers out of .bss.
        esp_alloc::heap_allocator!(size: 186 * 1024);
    // ROM-reclaimed region (dram2_seg). Second pool so nothing goes to waste; it
    // sits ABOVE the stack ceiling and is independent of _bss_end, so its size has
    // ZERO effect on the stack.
    //
    // #75 (lucid): was 56KB while the region is **exactly 64KB**, so 8,192 B of
    // usable SRAM was being left on the floor. esp-hal-1.1.1/ld/esp32c6/memory.x:
    //   dram2_seg: ORIGIN = 0x40800000 + 0x6E610 = 0x4086E610
    //              len    = 0x4087E610 - 0x4086E610 = 0x10000 = 65,536 B
    // and `.dram2_uninit` has no other occupant in the whole crate graph (only
    // esp-hal's ld/sections/dram2.x defines the section). Verified by `size -A`,
    // identical tree otherwise:
    //   56KB -> .dram2_uninit 57,344   .bss 268,280   .stack 75,120
    //   64KB -> .dram2_uninit 65,536   .bss 268,280   .stack 75,120
    // i.e. +8,192 B of heap with .bss and the stack byte-identical. If a future
    // dep ever also claims `.dram2_uninit`, this over-subscribes the region and
    // the LINK FAILS loudly — it cannot silently corrupt anything.
    //
    // This pool matters more than "nothing goes to waste": `EspHeapInner::alloc_caps`
    // walks the regions IN ORDER and falls through on failure, so every ordinary
    // Rust allocation that the main pool cannot satisfy is retried here. It is the
    // fallback that decides whether a 4 KB renderer request panics (#75).
    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 64 * 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_interrupt =
        esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_interrupt.software_interrupt0);

    // --- Stack-floor guardrail: regression tripwire for #59 ------------------
    // INVARIANT: on esp-hal the stack is the leftover gap under RAM top — it
    // grows DOWN from `_stack_start` to `_stack_end` (== `_bss_end`). Growing
    // `.bss` (a StaticCell, a spawned task's future, a bigger `heap_allocator!`)
    // raises `_bss_end` and SILENTLY steals stack; it's invisible to heap stats
    // and only surfaces as a WiFi-RX corruption crash at connect (ppRecycleRxPkt,
    // mtval=0x4). v0.5.0's climate statics shrank it 46.5 -> 39.6 KB (#59). This
    // reads the linker symbols (never writes the stack, so it won't trip esp-hal's
    // guard canary) and fails LOUD at boot if the gap drops below the floor.
    {
        unsafe extern "C" {
            static _stack_start: u8; // RAM-top side (fixed): stack grows down from here
            static _stack_end: u8; // == _bss_end: stack floor = end of main .bss
        }
        let top = unsafe { core::ptr::addr_of!(_stack_start) as usize };
        let bottom = unsafe { core::ptr::addr_of!(_stack_end) as usize };
        let gap = top.saturating_sub(bottom);
        // Floor 46 KB: just under v0.4.0's glass-proven-good 46.5 KB, well above
        // the 39.6 KB crash. The v0.5.0 fix boots at 51.6 KB (~5.6 KB headroom).
        // Any future .bss creep that drops the gap into the untested (39.6, 46.5]
        // band trips this at boot instead of corrupting WPA state at WiFi-connect.
        // #65: measured, not guessed. The old floor was 46KB — and the fleet
        // crashed at a gap of 61KB, so this assert sat ~15KB BELOW the real
        // requirement and never fired while the watch smashed itself.
        //
        // What actually breaks: the WiFi blob keeps globals at the TOP of .bss,
        // immediately under the downward-growing stack. `ppRxFragmentProc`
        // begins by loading a pointer from 0x4085E5C8 — with gap=63000,
        // _bss_end is 0x4085EFF8, so that pointer is a mere 2,608 B below the
        // stack floor. Overflow past the floor overwrites it with a spilled
        // register, the blob null-checks it (non-null garbage passes), then
        // dereferences → `Store/AMO access fault` at 2.7s, 100% reproducible.
        //
        // Measured on-glass, identical build otherwise:
        //   gap 61KB -> 5/5 panic @2.7s      gap 73KB -> 0/5, clean
        //
        // That is also why the bug looked layout-sensitive and made unrelated
        // edits (an 8-byte .bss change, a chime flag, .bss padding) appear to
        // "cause" or "fix" a WiFi crash: they moved _bss_end, changing how far
        // the stack had to run to reach the blob's pointer. #61's AMPDU change
        // did the same — it relocated the collision rather than removing it.
        //
        // Keep this floor ABOVE the measured failure point with real margin.
        // If it trips, GROW the stack (trim the MAIN heap_allocator!) — do not
        // lower the floor.
        const STACK_FLOOR: usize = 70 * 1024;
        println!("[STACK] gap = {} B ({} KB)", gap, gap / 1024);
        assert!(
            gap >= STACK_FLOOR,
            "stack gap {} B < {} B floor — new .bss ate the stack (see #59); trim \
             the MAIN heap_allocator! (main pool, below the stack) or shrink a \
             StaticCell / spawned-task future",
            gap,
            STACK_FLOOR
        );
    }

    println!("=== smol watch v2 (C6 AMOLED, Embassy) ===");
    let delay = Delay::new();

    // Speaker amp enable (GPIO6). CRITICAL: keep LOW before the ES8311 is
    // initialized and muted below — a floating I2S line through an enabled
    // amp produces loud white noise. From v0.8.5 the pin is driven by
    // audio_out::service_amp (per main-loop tick + inline after each
    // play_pcm): HIGH only while a queued SFX clip is in flight, LOW + codec
    // shutdown otherwise — power + pop discipline (#23).
    let mut amp_en = Output::new(peripherals.GPIO6, Level::Low, OutputConfig::default());

    // === I2C bus (AXP2101 + FT3168 + PCF85063 + QMI8658) ===
    let i2c = I2c::new(
        peripherals.I2C0,
        I2cConfig::default().with_frequency(Rate::from_khz(board::I2C_FREQ_HZ / 1000)),
    )
    .expect("I2C failed")
    .with_sda(peripherals.GPIO8)
    .with_scl(peripherals.GPIO7);
    let i2c_ref = RefCell::new(i2c);

    // === Power (read-mostly: rails left as the bootloader configured them) ===
    let mut power = Axp2101Power::new(RefCellDevice::new(&i2c_ref));
    let _ = power.enable_adc();
    println!("[POWER] OK");

    // === Display: CO5300 over QSPI DMA at 80MHz ===
    let spi_config = SpiConfig::default()
        .with_frequency(Rate::from_mhz(80))
        .with_mode(SpiMode::_0);
    // Raw SpiDma (no SpiDmaBus wrapper): QspiBus owns a single TX DmaTxBuf and
    // drives non-blocking DMA flushes itself (see drivers/qspi_bus.rs). No RX
    // buffer is needed — the panel is write-only.
    let spi = Spi::new(peripherals.SPI2, spi_config)
        .expect("SPI failed")
        .with_sck(peripherals.GPIO0)
        .with_sio0(peripherals.GPIO1)
        .with_sio1(peripherals.GPIO2)
        .with_sio2(peripherals.GPIO3)
        .with_sio3(peripherals.GPIO4)
        .with_dma(peripherals.DMA_CH0);
    let cs = Output::new(peripherals.GPIO5, Level::High, OutputConfig::default());
    let reset = Output::new(peripherals.GPIO11, Level::High, OutputConfig::default());
    let mut display = Co5300Display::new(QspiBus::new(spi, cs), reset);
    display.init();
    println!("[DISPLAY] OK");

    // Slint shell owns the whole watchface + launcher UI now. Construct it once
    // (registers the Slint platform + shows the window). brightness is synced to
    // the real boot value once watch_cfg is loaded (below).
    let mut shell = ShellUi::new();
    println!("[SLINT] shell up");

    // The ~201KB RGB332 framebuffer must NOT be resident while the Slint scene
    // is: the C6's SRAM can't hold both, so allocating it at boot OOM-panics.
    // Keep fb=None in shell mode; games/Settings allocate it fallibly on entry
    // (Framebuffer::try_new) and drop it on exit. Blank the panel directly via
    // the Co5300 (no framebuffer) so the first shell.render lands on black.
    let mut fb: Option<Framebuffer> = None;
    display.fill_screen(Rgb565::BLACK);
    log_heap("boot");

    // === Debug console (feature `debug-console`): UI test automator ==========
    // Take the USB-Serial-JTAG peripheral in async, RX-only mode. esp-println
    // keeps writing the TX side via raw MMIO (it never owns the peripheral or
    // uses interrupts), so `println!` output is unaffected — we only `.read()`
    // here and echo results back through `println!`. USB_DEVICE is otherwise
    // unused. See src/debug_console.rs for the sharing rationale.
    #[cfg(feature = "debug-console")]
    {
        use esp_hal::usb_serial_jtag::UsbSerialJtag;
        let usb = UsbSerialJtag::new(peripherals.USB_DEVICE).into_async();
        _spawner.spawn(debug_console::debug_console_task(usb).expect("debug_console_task token"));
    }

    // === Touch (FT3168: INT=GPIO15, RST=GPIO10) ===
    let mut touch_rst = Output::new(peripherals.GPIO10, Level::High, OutputConfig::default());
    let mut touch_int = Input::new(
        peripherals.GPIO15,
        InputConfig::default().with_pull(Pull::Up),
    );
    touch_rst.set_low();
    delay.delay_millis(10);
    touch_rst.set_high();
    delay.delay_millis(50);
    let mut touch = Ft3168Touch::new(RefCellDevice::new(&i2c_ref));
    let _ = touch.init();
    println!("[TOUCH] OK");

    // === RTC ===
    let mut rtc = Pcf85063aRtc::new(RefCellDevice::new(&i2c_ref));
    let _ = rtc.init();
    println!("[RTC] OK");

    // esp-hal RTC (rtc_cntl / LPWR peripheral) — drives HP-core light sleep for
    // AOD (#29). The wall clock stays on the external PCF85063 above, which is
    // unaffected by light sleep; this only powers the sleep/wake handshake.
    let mut rtc_lp = Rtc::new(peripherals.LPWR);

    // #43: fix the RC_FAST calibration esp-hal runs at every sleep entry
    // (REF_TICK divider unprogrammed on C6 rev >= v0.1 -> cal returns 0 ->
    // divide-by-zero inside sleep_light), seed a sane STORE1 slowclk period if
    // the boot calibration failed, and probe whether light sleep is safe.
    // MUST run before the first `sleep_light` below.
    let sleep_cal_ok = rtc_sleep_cal_init(&delay);

    // C6 on-die temperature sensor (#54) — read on the sensors page.
    let die_temp = DieTemp::new(peripherals.TSENS);

    // === IMU ===
    let mut imu = Qmi8658Imu::new(RefCellDevice::new(&i2c_ref));
    let _ = imu.init();
    println!("[IMU] OK");

    // === Audio (ES8311 codec + I2S) ===
    // CRITICAL ORDER (mirrors the S3 reference):
    // 1. Keep speaker amp DISABLED (GPIO6 LOW, done above) - prevents white
    //    noise from a floating I2S line
    // 2. Init codec (codec init leaves DAC powered but no input yet)
    // 3. Immediately shut the codec down again
    // 4. Init I2S bus
    // Playback (#23) then rides the always-running silent-clock TX ring:
    // play_pcm queues a clip -> service_amp unmutes codec + raises amp ->
    // the clock task substitutes the samples into the ring -> feeder tail
    // ends -> service_amp lowers amp + shuts the codec down again.
    println!("[AUDIO] Init codec...");
    let mut audio_codec = Es8311::new(RefCellDevice::new(&i2c_ref));
    match audio_codec.init() {
        Ok(()) => println!("[AUDIO] Codec OK"),
        Err(_) => println!("[AUDIO] Codec FAILED"),
    }
    // Full power-down of the analog blocks (not just mute). The PGA, DAC
    // and HP driver are explicitly cut — saves ~20 mA versus mute() which
    // only zeroes the volume register. `unmute()` brings them back on
    // demand at playback time.
    let _ = audio_codec.shutdown();

    // === I2S FULL-DUPLEX (shared clock) — beep playback + mic capture ===
    // C6 pins: MCLK=GPIO19, BCLK=GPIO20, LRCK/WS=GPIO22, DSDIN(DAC in)=GPIO23,
    // ASDOUT(ADC/mic out)=GPIO21. DMA_CH1 — the display QSPI owns DMA_CH0.
    // signal_loopback=true is the vendor topology via esp-hal's native API: TX and RX
    // share ONE WS/BCK internally — TX stays master and drives the pins, RX slaves to
    // TX's clock (ES8311 = external slave). This is what makes the ES8311 ADC actually
    // clock data onto ASDOUT (a plain RX-master did not; vendor firmware proved the HW
    // is fine — the topology was the gap).
    println!("[AUDIO] Init I2S (full-duplex, shared clock)...");
    let i2s_config = I2sConfig::default()
        .with_sample_rate(Rate::from_hz(16000))
        .with_data_format(DataFormat::Data16Channel16)
        .with_signal_loopback(true);
    let i2s_periph = I2s::new(peripherals.I2S0, peripherals.DMA_CH1, i2s_config)
        .expect("I2S failed")
        .with_mclk(peripherals.GPIO19);
    // TX is the I2S MASTER: it drives the shared BCLK(GPIO20)/WS(GPIO22) + MCLK. A
    // continuous SILENT circular TX (below) keeps them free-running so the ES8311 ADC
    // clocks data onto ASDOUT; the RX slaves to this clock via signal_loopback (internal).
    // A circular TX needs EXACTLY descriptor_count() descriptors for its ring buffer.
    const TX_RING_LEN: usize = audio_out::TX_RING_LEN; // 3072 bytes → 3 descriptors
    const TX_CIRC_DESCS: usize =
        esp_hal::dma::descriptor_count(TX_RING_LEN, esp_hal::dma::CHUNK_SIZE, true);
    static I2S_TX_DESC: StaticCell<[DmaDescriptor; TX_CIRC_DESCS]> = StaticCell::new();
    let mut i2s_tx = i2s_periph
        .i2s_tx
        .with_bclk(peripherals.GPIO20) // TX-master BCLK → codec (shared w/ RX via loopback)
        .with_ws(peripherals.GPIO22)   // TX-master WS/LRCK → codec
        .with_dout(peripherals.GPIO23) // DAC data → ES8311 DSDIN=GPIO23 (schematic I2S_DSDIN)
        .build(I2S_TX_DESC.init([DmaDescriptor::EMPTY; TX_CIRC_DESCS]));
    // === I2S RX for mic capture (#42 voice + #28 meter) — SLAVE via signal_loopback ===
    // `i2s_periph.i2s_rx` is still available (partial move — tx took i2s_tx). With
    // signal_loopback=true the RX shares the TX-master WS/BCK internally (configure()
    // sets rx_slave_mod + sig_loopback from the Config flag) and just reads DIN(GPIO21)=
    // ASDOUT — NO with_bclk/with_ws, NO GPIO-matrix hack. This is the ONLY place
    // I2S0/DMA_CH1 is claimed; voice PTT + the SoundLevel meter subscribe to MIC_CH.
    // MCLK (GPIO19) is peripheral-wide (with_mclk on i2s_periph). Stays Blocking:
    // mic_capture_task drives it via read_dma_circular + poll.
    //
    // v0.6.0 glass crash (Load fault mtval=0x8 in DmaTransferRxCircular::available):
    // a CIRCULAR RX chain must be sized EXACTLY to the ring. RxCircularState seeds its
    // walk from chain.last() expecting last.next → first; a padded array leaves trailing
    // EMPTY descriptors whose next=null, so the first available() poll derefs null.
    // descriptor_count() gives the exact count (3 for the ring @ CHUNK_SIZE=4092).
    const MIC_RX_DESCS: usize = esp_hal::dma::descriptor_count(
        mic_capture::MIC_RING_LEN,
        esp_hal::dma::CHUNK_SIZE,
        true, // circular
    );
    static I2S_RX_DESC: StaticCell<[DmaDescriptor; MIC_RX_DESCS]> = StaticCell::new();
    let i2s_rx = i2s_periph
        .i2s_rx
        .with_din(peripherals.GPIO21) // ADC/mic data ← ES8311 ASDOUT=GPIO21 (schematic I2S_ASDOUT)
        .build(I2S_RX_DESC.init([DmaDescriptor::EMPTY; MIC_RX_DESCS]));
    // Mic PCM channel (capture task → consumers) + the DMA capture ring.
    // Channel::new() is const → a plain static; the 8 KB ring needs a StaticCell.
    static MIC_CH: mic_capture::MicChannel = mic_capture::MicChannel::new();
    static MIC_RING: StaticCell<[u8; mic_capture::MIC_RING_LEN]> = StaticCell::new();
    let mic_ring = MIC_RING.init([0u8; mic_capture::MIC_RING_LEN]);
    _spawner.spawn(
        mic_capture::mic_capture_task(i2s_rx, mic_ring, MIC_CH.sender())
            .expect("mic_capture_task token"),
    );
    println!("[AUDIO] I2S RX (mic) ready on GPIO21 (DIN <- ES8311 ASDOUT)");

    // === Continuous full-duplex TX — the clock generator + playback ring ===
    // The mic ADC only shifts data onto ASDOUT while it sees BCLK/WS edges. As the
    // I2S MASTER, our TX must free-run those shared clocks continuously; RX slaves to
    // them (signal_loopback). We stream this ring forever: the shared BCLK/WS keep
    // toggling and the ADC keeps clocking real mic data into the RX DMA. The ring is
    // ZEROS except while a queued SFX clip plays (#23): the clock task substitutes
    // clip samples via DmaTransferTxCircular::push, and the feeder's tail scrubs the
    // ring back to all-silence before the amp drops — idle is exactly the proven
    // silent-clock behavior (amp GPIO6 LOW, data all-zero; no tone, no blasting).
    static TX_RING: StaticCell<[u8; TX_RING_LEN]> = StaticCell::new();
    let tx_ring: &'static [u8] = TX_RING.init([0u8; TX_RING_LEN]);
    // Clock + playback task; re-arms on CLOCK_REARM (AOD light sleep clock-gates
    // I2S; the task restarts the DMA after each wake) and per playback session
    // (see silent_clock_task docs: esp-hal's circular push-state goes Late after
    // any idle lap, so each session opens on a fresh transfer). This produces the
    // shared MCLK/BCLK/WS the ES7210 mic ADC (I2S slave) needs.
    _spawner.spawn(
        mic_capture::silent_clock_task(i2s_tx, tx_ring)
            .expect("silent_clock_task token"),
    );
    println!("[AUDIO] I2S TX clock+playback task spawned (full-duplex master, re-arms after sleep)");

    // === ES7210 mic ADC — the ACTUAL microphone codec ===
    // The mics are wired to the ES7210 (SDOUT1 -> GPIO21), NOT the ES8311. It MUST be
    // I2C-inited or our RX DIN stays idle → exact zeros. Init AFTER the silent clock is
    // live so the ES7210 (I2S slave) locks to the SoC's MCLK/BCLK/WS.
    // Power the mic rail FIRST (AXP2101 ALDO1 @3.3V) — otherwise the mic bias rides on
    // residual vendor state and a battery-dead cold boot silences the mic. Rail settles
    // during the 150ms clock delay below.
    let _ = power.enable_mic_rail();
    // Charger profile (issue #16): CV 4.1V / pre 50mA / CC 400mA / term 25mA —
    // vendor parity (board .cc:48-52). Field-masked RMW of regs 0x61-0x64 only;
    // rail enables are untouched (panel brown-out caution in power.rs header).
    if power.configure_charger().is_ok() {
        println!("[POWER] charger configured: CV 4.1V, pre 50mA, CC 400mA, term 25mA");
    } else {
        println!("[POWER] charger config FAILED (I2C)");
    }
    // PWRON key events (#48): pin the long-press IRQ threshold to 1.5s
    // (0x27[5:4], field-masked — the 4s OFFLEVEL failsafe bits are untouched),
    // enable the short/long latches (0x41 RMW), clear stale ones (0x49 W1C).
    // The main loop polls the latch; ladder: 1.5s hold -> power menu, 4s hold
    // -> hardware poweroff (vendor failsafe, works even with firmware hung).
    if power.enable_pwron_events().is_ok() {
        println!("[POWER] PWRON events armed: IRQLEVEL 1.5s menu, 4s hw-off failsafe");
    } else {
        println!("[POWER] PWRON event arm FAILED (I2C)");
    }
    Timer::after(Duration::from_millis(150)).await; // let silent_clock_task bring the clock up
    let mut mic_adc = Es7210::new(RefCellDevice::new(&i2c_ref));
    match mic_adc.init() {
        Ok(()) => {
            let g = mic_adc.read_reg(0x43).unwrap_or(0xEE);
            println!("[ES7210] init OK (MIC1 gain reg43=0x{:02x}, expect 0x1d)", g);
        }
        Err(_) => println!("[ES7210] init FAILED (I2C at 0x40)"),
    }

    // Pre-synthesize the SFX clips (#23) — MONO 16 kHz s16le, the play_pcm
    // format; the feeder duplicates L/R into the stereo TX ring. Both come
    // from mic-dsp (host-unit-tested synth):
    //  - Snake food beep: 800 Hz / 50 ms sine, 2 ms attack/release ramps.
    //  - UI tap click: 12 ms decaying 1.8 kHz "tick" (launcher launch +
    //    UPDATE FIRMWARE — opt-in per control, subtle by design).
    static BEEP_PCM: StaticCell<[u8; 1600]> = StaticCell::new();
    let beep_pcm: &'static [u8] = {
        let buf = BEEP_PCM.init([0u8; 1600]);
        let n = mic_dsp::fill_tone_mono_s16le(buf, 16_000, 800, 50, 12_000, 2);
        &buf[..n]
    };
    // Every-touch tick (#49, v0.9.0): the same 12 ms 1.8 kHz "tick" as the old
    // opt-in click but QUIETER (peak ~6000 ≈ −15 dBFS) — played by the ONE
    // hoisted tap hook below on every tap, so it must read as texture, not
    // notification. Gated on the persisted `touch_sound` flag.
    static TICK_PCM: StaticCell<[u8; mic_dsp::CLICK_LEN]> = StaticCell::new();
    let tick_pcm: &'static [u8] = {
        let buf = TICK_PCM.init([0u8; mic_dsp::CLICK_LEN]);
        let n = mic_dsp::fill_tick_mono_s16le(buf, 16_000);
        &buf[..n]
    };
    // Watch-ping receiver chime (#58): the 700 ms rising C-major arpeggio, the
    // "someone's thinking of you" sound. HEAP-leaked rather than a StaticCell:
    // .bss would come straight out of the stack gap (stack = _stack_start −
    // _bss_end — the #65 crash class), while a one-time boot alloc costs nothing
    // at runtime.
    //
    // Stored at **8 kHz** (11 200 B, half of the 16 kHz form); the playback
    // feeder duplicates each sample up to the 16 kHz ring. Free quality-wise —
    // the chime is pure sines topping out at C6 = 1046.5 Hz, so 8 kHz is still
    // ~4x oversampled — and it repays 11 200 B of the 12 KB of main heap that
    // growing the stack for #65 cost. Without this, the shade OOM'd on a
    // swipe-down ("memory allocation of 4096 bytes failed"): stack safety and UI
    // heap were competing for the same bytes.
    let ping_chime_pcm: &'static [u8] = {
        let buf: &'static mut [u8] = alloc::vec![0u8; mic_dsp::PING_CHIME_8K_LEN].leak();
        let n = mic_dsp::fill_ping_chime_mono_s16le(buf, 8_000);
        &buf[..n]
    };
    println!(
        "[AUDIO] SFX ready (beep {} B, tick {} B, chime {} B mono) — playback via shared TX ring",
        beep_pcm.len(),
        tick_pcm.len(),
        ping_chime_pcm.len()
    );
    // #58: hand the chime to the audio seam so the clock task's feeder can
    // stream the FULL 480 ms melody off this static buffer. Deliberately NOT a
    // dedicated task: an extra Embassy task here panicked the watch 100 % of the
    // time under the debug-console build (see audio_out::LONG_CLIP docs).
    audio_out::register_chime(ping_chime_pcm);

    // BOOT button (GPIO9 on the C6, strapping pin with pull-up).
    let mut boot_button = Input::new(
        peripherals.GPIO9,
        InputConfig::default().with_pull(Pull::Up),
    );

    // === OTA foundation: report partition layout + boot slot ===
    let mut flash = esp_storage::FlashStorage::new(peripherals.FLASH);
    let mut config_offset: Option<u32> = None;
    // #55: the app slot the CPU is actually executing from (MMU probe) and
    // both app slots' geometry, feeding the GuardedFlash deny-list below.
    let mut booted_slot: Option<(u32, u32)> = None;
    let mut app_slots: [Option<(u32, u32)>; 2] = [None, None];
    {
        use esp_bootloader_esp_idf::partitions::{
            self, AppPartitionSubType, DataPartitionSubType, PartitionType,
        };
        let mut pt_mem = [0u8; partitions::PARTITION_TABLE_MAX_LEN];
        match partitions::read_partition_table(&mut flash, &mut pt_mem) {
            Ok(pt) => {
                println!("[OTA] partition table: {} entries", pt.len());
                if let Ok(Some(cp)) =
                    pt.find_partition(PartitionType::Data(DataPartitionSubType::Spiffs))
                {
                    config_offset = Some(cp.offset());
                }
                for (i, sub) in [AppPartitionSubType::Ota0, AppPartitionSubType::Ota1]
                    .into_iter()
                    .enumerate()
                {
                    if let Ok(Some(p)) = pt.find_partition(PartitionType::App(sub)) {
                        app_slots[i] = Some((p.offset(), p.len()));
                    }
                }
                // The booted slot per the MMU vs the slot otadata REQUESTS:
                // they disagree whenever the bootloader fell back (#55 — stale
                // otadata after the #50 re-partition said Ota1 while ota_1 was
                // empty). The MMU is the fact; otadata is only a request.
                match pt.booted_partition() {
                    Ok(Some(bp)) => {
                        println!(
                            "[OTA] booted from {:?} @{:#x} (MMU)",
                            bp.partition_type(),
                            bp.offset()
                        );
                        booted_slot = Some((bp.offset(), bp.len()));
                    }
                    _ => println!("[OTA] booted-slot probe FAILED - protecting both app slots"),
                }
                match pt.find_partition(PartitionType::Data(DataPartitionSubType::Ota)) {
                    Ok(Some(od)) => {
                        let region = od.as_embedded_storage(&mut flash);
                        match esp_bootloader_esp_idf::ota::Ota::new(region, 2) {
                            Ok(mut ota) => println!(
                                "[OTA] otadata requests {:?}, state {:?}",
                                ota.current_app_partition(),
                                ota.current_ota_state()
                            ),
                            Err(e) => println!("[OTA] otadata: {e:?}"),
                        }
                    }
                    _ => println!("[OTA] no otadata partition (factory layout)"),
                }
            }
            Err(e) => println!("[OTA] partition table read failed: {e:?}"),
        }
    }

    // === Radio: WiFi STA + BLE, both OFF at boot (see the S3 power notes) ===
    // In esp-radio 0.18 `set_config` is what starts the controller, so we
    // build the station config here but only apply it on the first toggle.
    log_heap("pre-wifi"); // per-region heap right before the WiFi stack inits
    // #61 WiFi-blob null-deref in `ppRxFragmentProc` (Load access fault,
    // mtval=4). That blob routine is RX aggregation/fragment reassembly, hit
    // during scan/assoc. Disable RX AMPDU so received frames bypass the
    // block-ack reorder path the blob null-derefs in. This is a *runtime*
    // knob (wifi_init_config_t.ampdu_rx_enable = 0) — the sys blob is still
    // compiled WITH AMPDU capability (esp-radio's lib.rs const-asserts
    // CONFIG_ESP_WIFI_AMPDU_RX_ENABLED == 1), we just don't enable it for
    // our session. Throughput loss is irrelevant on a watch. NOTE: esp-radio
    // 0.18's ControllerConfig exposes NO amsdu_rx / raw-802.11-fragment knob;
    // `ampdu_rx_enable` is the only RX-aggregation lever the init API has.
    // #65: `rx_ba_win` MUST go to 0 alongside it. esp-radio's default is 6 and
    // it is passed straight into the blob's `wifi_init_config_t` next to
    // `ampdu_rx_enable: false` — a combination ESP-IDF itself never produces:
    //
    //   #if CONFIG_ESP_WIFI_AMPDU_RX_ENABLED
    //   #define WIFI_RX_BA_WIN   CONFIG_ESP_WIFI_RX_BA_WIN
    //   #else
    //   #define WIFI_RX_BA_WIN   0 /* unused if ampdu_rx_disabled */
    //   #endif
    //
    // A non-zero Block-Ack window with aggregation OFF leaves the blob holding
    // BA reorder state with no aggregation buffers behind it — and
    // `ppRxFragmentProc`, the RX-fragment handler, is exactly what walks that
    // state. That is the crash site for BOTH #61 (null deref) and the
    // `Store/AMO access fault` that made release builds panic 100% at 2.7 s
    // while flipping on and off with a few bytes of `.bss` (layout moved which
    // garbage the stale pointer landed on).
    let wifi_config = esp_radio::wifi::ControllerConfig::default()
        .with_ampdu_rx_enable(false)
        .with_rx_ba_win(0);
    let (wifi_controller, wifi_interfaces) =
        esp_radio::wifi::new(peripherals.WIFI, wifi_config).expect("WiFi init failed");
    log_heap("post-wifi"); // confirms the RX-pool carve isn't starving a region
    let ble_connector =
        BleConnector::new(peripherals.BT, Default::default()).expect("BLE init failed");
    // trouble-host GATT server: wrap the HCI transport and hand it to the
    // host task. The task parks until the watchface BLE button fires.
    let ble_controller: peripherals::ble::WatchController =
        trouble_host::prelude::ExternalController::new(ble_connector);
    _spawner.spawn(
        peripherals::ble::ble_host_task(ble_controller).expect("ble_host_task token"),
    );
    println!("[RADIO] stack ready (WiFi OFF, BLE advertising OFF)");

    // Credentials: flash config wins; compile-time env is the fallback seed.
    let mut watch_cfg = config_offset
        .and_then(|off| peripherals::config::load(&mut flash, off))
        .unwrap_or_default();
    if watch_cfg.ssid.is_empty() {
        let _ = watch_cfg.ssid.push_str(option_env!("WIFI_SSID").unwrap_or(""));
        let _ = watch_cfg.pass.push_str(option_env!("WIFI_PASS").unwrap_or(""));
    }
    println!(
        "[CFG] node id{:03}, ssid={:?}",
        watch_cfg.node_id,
        watch_cfg.ssid.as_str()
    );
    // Boot reads above used the raw handle; everything from here shares it —
    // wrapped in the #55 write guard: the bootloader + partition-table region
    // and the slot the CPU executes from are write-protected. If the booted
    // slot (or the whole table) couldn't be determined, protect both app
    // slots (fail-safe: OTA refuses on its own booted check, config/otadata
    // writes still work).
    let flash = {
        let mut guard = flash_guard::WriteGuard::new();
        let mut deny = |start: u32, len: u32| {
            if guard.deny(start, len).is_err() {
                // Can't happen (<= 3 ranges), but never fail open silently.
                println!("[FLASH-GUARD] deny-list full - range {start:#x}+{len:#x} DROPPED");
            }
        };
        // Bootloader + partition table: nothing runtime-writes below nvs.
        deny(0x0, 0x9000);
        match booted_slot {
            Some((off, len)) => deny(off, len),
            None => {
                let mut any = false;
                for slot in app_slots.into_iter().flatten() {
                    deny(slot.0, slot.1);
                    any = true;
                }
                if !any {
                    // Table unreadable: there are no known-legitimate write
                    // targets either — deny the whole flash.
                    deny(0x9000, u32::MAX - 0x9000);
                }
            }
        }
        guarded_flash::GuardedFlash::new(flash, guard)
    };
    static FLASH_MUTEX: StaticCell<FlashMutex> = StaticCell::new();
    let flash: &'static FlashMutex = FLASH_MUTEX.init(embassy_sync::mutex::Mutex::new(flash));
    // SIGIL IDENTITY (#34): config node id 42 is the never-explicitly-chosen
    // default on every watch (a fleet-wide mesh collision, observed breaking
    // MQTT windows) — treat it as the "unset" sentinel and fall back to the
    // MAC-derived id. An explicitly set config id ≠ 42 still wins. The derived
    // id is never persisted: it stays deterministic from the efuse MAC.
    let sigil = net::sigil::get();
    let node_id = if watch_cfg.node_id == 42 {
        sigil.node_id
    } else {
        watch_cfg.node_id
    };
    println!(
        "[SIGIL] {} (node id {}, {})",
        sigil.sigil.as_str(),
        node_id,
        if watch_cfg.node_id == 42 { "mac-derived" } else { "config" },
    );
    let mut wifi_has_creds = !watch_cfg.ssid.is_empty();
    if !wifi_has_creds {
        println!("[WIFI] no credentials - set them in Settings");
    }
    // The station config itself is built inside net_task from the creds we
    // hand it at spawn (and any later NetCmd::SetCreds).

    // ESP-NOW rides the same radio; usable whenever WiFi is started.
    let mut esp_now = wifi_interfaces.esp_now;

    let net_config = embassy_net::Config::dhcpv4(Default::default());
    // 5 sockets: DHCP + the always-on DNS socket + one transient TCP/UDP
    // (NTP, MQTT-burst, weather, OTA — sequential inside net_task) + the HA
    // session TCP + one spare. The spare matters since #53/#22: the burst
    // runs in net_task CONCURRENTLY with main's voice STT upload — and the
    // #22 latch fires STT the instant the link is ready, exactly when the
    // burst's NTP query starts. Worst overlap = DHCP + DNS + burst + session
    // (draining) + STT = 5; SocketSet::add PANICS when full, so the headroom
    // is correctness, not comfort (~200B of .bss).
    static RESOURCES: StaticCell<embassy_net::StackResources<5>> = StaticCell::new();
    let (stack, runner) = embassy_net::new(
        wifi_interfaces.station,
        net_config,
        RESOURCES.init(embassy_net::StackResources::new()),
        12345u64,
    );
    _spawner.spawn(net_stack_task(runner).expect("net_stack_task token"));

    // #53: the network OWNER. From here on `wifi_controller` belongs to
    // net_task exclusively — the connect state machine, reconnect backoff,
    // scanning, the boot burst, and OTA downloads all run there; main drives
    // it over NetCmd and renders from its published snapshot. `boot_connect`
    // mirrors the old auto-connect intent (creds present and not forced-off).
    _spawner.spawn(
        crate::net::net_task::net_task(
            wifi_controller,
            stack,
            flash,
            watch_cfg.ssid.clone(),
            watch_cfg.pass.clone(),
            wifi_has_creds && !watch_cfg.wifi_off,
        )
        .expect("net_task token"),
    );

    // #58: HA climate session infrastructure. The session runs in its own task
    // (holds WiFi while the Climate screen is open); main.rs drives it via the
    // open/close signals, reads the shared ClimateState for the UI each tick, and
    // releases the WiFi hold on `done` (both Ok + Err arms — see climate_task).
    static LIGHTS_STATE: StaticCell<crate::net::mqtt_climate::LightsStateMutex> = StaticCell::new();
    static CLIMATE_STATE: StaticCell<crate::net::mqtt_climate::ClimateStateMutex> =
        StaticCell::new();
    static ENERGY_STATE: StaticCell<crate::net::mqtt_climate::EnergyStateMutex> = StaticCell::new();
    static CLIMATE_CMDS: StaticCell<crate::net::mqtt_climate::ClimateCmdChannel> = StaticCell::new();
    static CLIMATE_OPEN: StaticCell<crate::net::mqtt_climate::CloseSignal> = StaticCell::new();
    static CLIMATE_CLOSE: StaticCell<crate::net::mqtt_climate::CloseSignal> = StaticCell::new();
    static CLIMATE_DONE: StaticCell<crate::net::mqtt_climate::CloseSignal> = StaticCell::new();
    // Shared (&) refs, not the &mut StaticCell::init yields — both the task and
    // the main loop hold them (Signal/Channel/Mutex methods take &self).
    let climate_state: &'static crate::net::mqtt_climate::ClimateStateMutex =
        CLIMATE_STATE.init(embassy_sync::mutex::Mutex::new(climate_model::ClimateState::new()));
    let climate_energy: &'static crate::net::mqtt_climate::EnergyStateMutex = ENERGY_STATE.init(
        embassy_sync::mutex::Mutex::new(crate::net::mqtt_climate::EnergyState::default()),
    );
    // Lights (#39): the room-lights snapshot rides the same session.
    let lights_state: &'static crate::net::mqtt_climate::LightsStateMutex = LIGHTS_STATE.init(
        embassy_sync::mutex::Mutex::new(crate::net::mqtt_climate::LightsState::new()),
    );
    let climate_cmds: &'static crate::net::mqtt_climate::ClimateCmdChannel =
        CLIMATE_CMDS.init(embassy_sync::channel::Channel::new());
    let climate_open: &'static crate::net::mqtt_climate::CloseSignal =
        CLIMATE_OPEN.init(embassy_sync::signal::Signal::new());
    let climate_close: &'static crate::net::mqtt_climate::CloseSignal =
        CLIMATE_CLOSE.init(embassy_sync::signal::Signal::new());
    let climate_done: &'static crate::net::mqtt_climate::CloseSignal =
        CLIMATE_DONE.init(embassy_sync::signal::Signal::new());
    _spawner.spawn(
        climate_task(
            stack,
            climate_state,
            climate_energy,
            lights_state,
            climate_cmds.receiver(),
            climate_open,
            climate_close,
            climate_done,
        )
        .expect("climate_task token"),
    );

    println!("=== All systems GO! ===");

    // === State ===
    // Watchface live-state that used to hang off WatchFace. The Slint shell owns
    // its own scene state (page, launcher, dirty tracking), so these are plain
    // loop locals pushed into the shell via setters.
    let mut brightness: u8 = watch_cfg.brightness;
    let mut gyro_enabled = false;
    let mut cpu_mhz: u16 = 160;
    let mut last_dt: Option<DateTime> = None;
    display.set_brightness(brightness);
    // Sync the power-page slider knob to the real boot brightness (else it shows
    // the Slint default while the panel is at watch_cfg.brightness).
    shell.set_brightness_from_raw(brightness);
    // Honor the persisted boot page (CFG `S` default). The shell starts on the
    // clock; apply default_page so the watch boots where the user left it. Until
    // now this value was written to flash but never read back at boot.
    shell.set_page(watch_cfg.default_page as i32);
    // Apply the persisted theme scheme (config.rs v3). Records saved before the
    // theme byte existed default to 0 (Midnight), preserving the shipped look.
    shell.set_scheme(watch_cfg.theme as i32);
    // LP (low-power RISC-V) core status on the power page. No offload yet
    // (task #24 got a RED verdict), so this is a static availability indicator:
    // the LP core is idle at its ~20MHz clock (HP core runs 160MHz). One-shot.
    shell.set_lp_core("idle", 20);
    let mut power_stats = PowerStats::new();
    power_stats.cpu_mhz = 160;
    let mut app_state = AppState::Watchface;
    let mut prev_app_state = app_state;
    // Session manager (#31): which apps are suspended (exited with state kept),
    // most recent first. Drives the bottom-edge-hold switcher + the badge chip.
    let mut sessions = crate::apps::session::Sessions::new();
    let mut snake_game = SnakeGame::new();
    // World Snake shares the SMOLv1 node id so its SNK frames name us.
    let mut world_snake = WorldSnakeApp::new(node_id);
    let mut game_2048 = Game2048::new();
    let mut tetris_game = TetrisGame::new();
    let mut flappy_game = FlappyGame::new();
    let mut maze_game = MazeGame::new();
    let mut accel = (0.0f32, 0.0f32, 0.0f32);
    let mut gyro_data = (0i16, 0i16, 0i16);
    let mut imu_temp: i16 = 250;
    let mut batt_pct: u8 = 0;
    let mut batt_mv: u16 = 0;
    let mut charging = false;
    let mut last_interaction = Instant::now();
    // 3=bright 2=dim 1=AOD 0=off (see the S3 firmware for the rationale)
    let mut screen_state: u8 = 3;
    // Shell push-guards: only re-push radio chrome / re-pace page data on change.
    let mut prev_radios: (bool, bool, u8) = (false, false, 0);
    let mut prev_page: i32 = -1;
    // RAM-busy toast lifecycle: shown when a launch can't allocate the fb, then
    // auto-cleared once its window elapses (toast_active gates the single clear).
    let mut toast_until = Instant::now();
    let mut toast_active = false;
    // AOD repaints only when the minute changes; 99 is a sentinel that forces
    // the first paint on AOD entry (any real minute 0..=59 differs).
    let mut aod_last_minute: u8 = 99;
    // Wall-clock (PCF85063) seconds-of-day captured at AOD entry. The AOD->off
    // timeout is driven from this, NOT embassy-time, because embassy-time freezes
    // during light sleep (#29 DS1).
    let mut aod_entry_sod: u32 = 0;
    // Familiar UI snapshot push-guard: only set_fam when the snapshot changes.
    let mut prev_fam = FamUi::default();
    // Climate render push-guard (#60 OOM fix): the fingerprint of the last model
    // pushed to Slint. set_climate rebuilds a heap Vec<ClimateCard> + its
    // SharedStrings; doing it every tick fragmented the allocator until a ~7 KB
    // alloc failed and the watch OOM-panicked on the Climate screen. Push only
    // when the rendered content actually changes. `None` = force the next push
    // (reset on screen open).
    let mut prev_climate_fp: Option<u64> = None;
    // Low-battery notification latch (#32): one warning per discharge.
    let mut low_batt_notified = false;
    // Last pushed step count, cached so the shell can be re-populated after a
    // scene recreate (the pedometer only polls once a minute).
    let mut last_steps: u32 = 0;
    // Last weather (temp_f, code), cached for re-push after a scene recreate:
    // it's fetched only during a WiFi/NTP window, which may be a long way off.
    let mut last_weather: Option<(i16, u8)> = None;

    // Initial shell paint so the panel shows a live clock immediately instead of
    // waiting up to a full tick for the first loop render.
    if let Ok(pct) = power.get_battery_percent() {
        batt_pct = pct;
        batt_mv = power.get_battery_voltage().unwrap_or(0);
        charging = power.is_charging().unwrap_or(false);
        shell.set_battery(batt_pct, batt_mv, charging);
        crate::peripherals::ble::BATTERY_PERCENT
            .store(batt_pct, core::sync::atomic::Ordering::Relaxed);
    }
    if let Ok(dt) = rtc.get_time() {
        let _ = shell.set_time(&dt);
        last_dt = Some(dt);
    }
    // Boot is the first glance of all: shimmer the nav gesture hints (the
    // shell sequences bloom/hold/fade itself — see ShellUi::hint_wake).
    shell.hint_wake();
    shell.render(&mut display);

    let mut next_rtc = Instant::now();
    let mut next_battery = Instant::now();
    // PWRON key poll (#48): 250ms while awake keeps the worst-case menu
    // latency at IRQ(1.5s) + 0.25s + a render — comfortably inside the 4s
    // hardware cutoff. Re-armed after each AOD light-sleep wake (the embassy
    // clock pauses in sleep, so a plain `now + 250ms` would starve there).
    let mut next_pkey = Instant::now();
    // Long-press seen -> the Slint arm raises the menu (deferred one dispatch
    // so a game can be exited + the scene resumed first, same tick).
    let mut power_menu_request = false;
    let mut last_frame = Instant::now();
    let mut next_flush = Instant::now();
    // "Power down" now only gates the gyro: the accel stays on at 62.5Hz
    // so the QMI8658's hardware pedometer keeps counting in the background.
    let _ = imu.power_down();
    let mut imu_powered = false;
    // Wrist-raise (tilt-to-wake) detector for the polling AOD path. The
    // QMI8658 INT is not wired to a wake GPIO on this board, so raise wake is
    // done by reading the accel on each short AOD light-sleep poll (below).
    let mut raise_detector = crate::peripherals::imu::RaiseDetector::new();
    let mut next_step_poll = Instant::now();
    let mut was_touching = false;

    // Radio state (#53): user intent vs. radio truth lives in net_task now —
    // the boot auto-connect intent rode the spawn's `boot_connect` arg. Main
    // keeps only UI-side edge trackers; everything else reads the per-tick
    // `net` snapshot.
    // #58: Climate session lifecycle. climate_active holds WiFi while the screen
    // is open (cleared on session return); climate_running gates the one-shot
    // open-signal so the session spawns once per screen visit.
    let mut climate_active = false;
    let mut energy_active = false;
    // #58 finding-(b) is structural now (#53): the shared session raises its
    // OWN Hold::Session bit in net_task, so dropping it can never clobber a
    // manual WiFi-on (Hold::User). These track the sent-edge so a rejected
    // send (queue full during an OTA) retries next tick.
    let mut session_hold_up = false;
    let mut voice_hold_up = false;
    let mut climate_running = false;
    // Optimistic setpoint for the Climate detail (oracle-t9 C4/C5/E2).
    let mut climate_pending: Option<ClimatePending> = None;
    // #39 Lights: screen-open flag (holds WiFi via the shared session, like
    // climate/energy), the optimistic "sent" tracker, and the transient
    // no-reply hint deadline (shown ~2.5s after a 5s reply timeout).
    let mut lights_active = false;
    let mut lights_pending: Option<LightsPending> = None;
    let mut lights_noreply_until: Option<Instant> = None;
    // [LAT] "Finding your room…" duration: stamped on screen-open, consumed
    // (printed) on the first rendered state frame.
    let mut lights_opened_at: Option<Instant> = None;
    // #22 press-once PTT: a Voice-screen press that lands before WiFi/DHCP is
    // ready LATCHES instead of dropping — the capture auto-fires through the
    // exact same entry path the moment `net.phase.ready()` lands, provided
    // the finger is still down (release cancels; that's the contract: hold
    // through "Connecting…"). `voice_latch` stamps the press; `voice_latch_up`
    // is the 3-read release debounce (authoritative I2C finger count — the
    // INT pin lies for still fingers, same lesson as the capture monitor).
    // Bounded by VOICE_LATCH_WINDOW; a released finger cancels within ~300ms,
    // so the latch can never survive into a dim/AOD/screen-off transition.
    const VOICE_LATCH_WINDOW: Duration = Duration::from_secs(30);
    let mut voice_latch: Option<Instant> = None;
    let mut voice_latch_up: u8 = 0;
    // #28 sound-level meter: whether the ADC+METER gate are currently armed, and
    // the decaying peak-hold value (dBFS). Only touched while app_state==Sound.
    let mut meter_on = false;
    // Digital mic-gain index into mic_capture::GAIN_STEPS_* (Sound-app −/+ stepper).
    // Default 0 dB: the ES7210 analog PGA (36 dB) + the now-explicit ALDO1 mic rail
    // already give a strong, clean level; digital gain adds NO SNR (it amplifies noise
    // equally) and was turning residual hiss into audible static. Restored from the
    // persisted config (v5 mic-gain byte, #46) — clamped in case a downgrade shrank
    // the table; each stepper change re-persists it (edge-triggered, below).
    let mut gain_idx: usize =
        (watch_cfg.mic_gain as usize).min(mic_capture::GAIN_STEPS_Q8.len() - 1);
    mic_capture::MIC_GAIN_Q8.store(
        mic_capture::GAIN_STEPS_Q8[gain_idx],
        core::sync::atomic::Ordering::Relaxed,
    );
    shell.set_mic_gain_db(mic_capture::GAIN_STEPS_DB[gain_idx] as i32);
    let mut meter_peak = mic_dsp::DBFS_FLOOR;
    // Bar envelope (dBFS): fast attack (jumps to the level instantly), slow
    // release — so brief speech syllables visibly fill/hold the bar instead of
    // the raw instantaneous RMS collapsing to -inf between words.
    let mut meter_env = mic_dsp::DBFS_FLOOR;
    // #30 spectrum analyzer: per-band bar + peak-hold envelopes (12 log bands,
    // 80 Hz–8 kHz). Fed ONE 256-pt FFT per Sound tick — the C6 has no FPU, so
    // the softfloat FFT (~few ms) runs once on the latest window, not per chunk.
    let mut spec_env = mic_dsp::SpectrumEnvelope::new();
    // "STA radio (PHY) started" / association / NTP-burst state: all owned by
    // net_task (#53), read back per tick via `net_task::snapshot()`. The
    // toggle latch + idle-backstop rate limit stay here (they're UI intent).
    let mut wifi_toggle_request = false;
    let mut last_wifi_idle_check = Instant::now();
    // #46 (BLE bit): restore the persisted BLE toggle (config v4) so BLE-on —
    // and with it the stable-address Bermuda registration (#47) — survives
    // reboots and OTAs. The parked trouble host starts within ~250 ms.
    let mut ble_on = watch_cfg.ble_on;
    if ble_on {
        crate::peripherals::ble::BLE_START_REQUEST
            .store(true, core::sync::atomic::Ordering::Relaxed);
        power_stats.ble_on = true;
        println!(
            "[BLE] restored ON from config (persisted toggle, '{}')",
            net::sigil::get().sigil.as_str()
        );
    }
    let mut ble_toggle_request = false;
    let mut settings_connect_pending = false;
    // SMOLv1 mesh: explicit flash-config node id, or the MAC-derived sigil id
    // when config still holds the 42 "unset" sentinel (#34, arbitrated above).
    let mut mesh = SmolMesh::new(node_id);
    // Mesh Familiar (fleet #57): always-on holder/arbitration state machine,
    // ticked alongside mesh.tick. The creature renders on the watchface.
    let mut familiar = crate::net::familiar::FamState::new(node_id);
    let mut esp_now_peer_added = false;
    // WiZmote frame sequence (WLED de-dups on it); wraps, monotonic per send.
    let mut wled_seq: u32 = 0;
    // RSSI treasure-hunt game state (fed from the mesh roster while Hunt is open).
    let mut hunt_state = hunt::HuntState::new();
    // Mesh enable flag (MESH chrome dot toggles it). Default OFF (power: the STA
    // radio only comes up when mesh is turned on). Toggling ON starts the radio
    // (below) then the ESP-NOW tick/rx/familiar run; OFF pauses the tick (peer
    // stays registered, radio stays up — a tick-level pause, not a teardown).
    // Restored from the persisted toggle (config v5 mesh bit, #46) like ble_on;
    // an ON restore starts the STA radio exactly as the toggle-on path does —
    // creds NOT required (set_config starts the PHY without connecting).
    let mut mesh_enabled = watch_cfg.mesh_on;
    if mesh_enabled {
        // PHY-only start is a net_task hold now (#53): creds NOT required
        // (set_config starts the PHY without connecting). The mesh block
        // gates on the published radio_started, so it comes up a beat later.
        let _ = crate::net::net_task::send(crate::net::net_task::NetCmd::Raise(
            crate::net::net_task::Hold::Phy,
        ));
        println!("[MESH] restored ON from config (persisted toggle)");
    }
    // Touch sound (#49): the persisted every-tap tick gate. Default ON.
    let mut touch_sound = watch_cfg.touch_sound;

    // === Speaker volume + button mapping (#59) ===
    // Restore the persisted volume step + mute into the codec master-volume
    // atomic so EVERY clip (chime/beeps/clicks/tick) plays at the stored level
    // (audio_out::service_amp reads it on each amp-raise). The amp is down at
    // boot, so this just seeds the atomic; no live codec write needed.
    let mut volume = watch_cfg.volume.min(peripherals::config::VOL_MAX);
    let mut muted = watch_cfg.muted;
    audio_out::MASTER_VOL_REG.store(
        audio_out::vol_to_reg(volume, muted),
        core::sync::atomic::Ordering::Relaxed,
    );
    // Button map (#59): BOOT/PWRON × short/long → action. Restored like the
    // radio toggles; each hub cycle re-persists.
    use crate::peripherals::config::ButtonAction;
    #[cfg(feature = "tts")]
    use crate::peripherals::config::SpeakMode;
    let mut boot_short = watch_cfg.boot_short;
    let mut boot_long = watch_cfg.boot_long;
    let mut pwron_short = watch_cfg.pwron_short;
    let mut pwron_long = watch_cfg.pwron_long;
    // ONE action queued per tick by the button state machines below, dispatched
    // in a single place BEFORE the app-state match (so it can freely set
    // app_state / launcher / power-menu / volume regardless of which arm runs).
    let mut pending_button: Option<ButtonAction> = None;
    // Read-aloud request (#read-aloud), raised by ButtonAction::Speak or by an
    // auto-mode arrival, serviced at the single speak site below. A flag rather
    // than an inline call because that site holds the amp + codec borrows and
    // parks the loop for seconds — it must be the only place that can do so.
    #[cfg(feature = "tts")]
    let mut speak_request = false;
    // BOOT press state machine (short vs long): press start, long-fired latch,
    // and "this press only woke the screen" (so a wake press never also acts).
    let mut boot_press_start: Option<Instant> = None;
    let mut boot_long_fired = false;
    let mut boot_wake_consumed = false;
    const BOOT_LONG_MS: u64 = 600;
    // Volume HUD auto-dismiss deadline (#59): armed on any volume change, a
    // drag resets it; the per-tick check closes the overlay when it passes.
    let mut volume_overlay_until: Option<Instant> = None;

    // === Watch-to-watch ping (#35) — tiny sender/receiver state ===
    // Sender: one outstanding (seq, sent_at) awaiting its PINGACK; a 3s
    // cooldown between sends (etiquette — the hero shows a recharge sweep).
    // `ping_state` mirrors the overlay's presentation machine (0 idle · 1 sent
    // · 2 delivered · 3 no reply); `ping_result` is the ACKER's sigil.
    let mut ping_seq: u16 = 0;
    let mut ping_outstanding: Option<(u16, Instant)> = None;
    let mut ping_cooldown_until: Option<Instant> = None;
    let mut ping_state: i32 = 0;
    let mut ping_result: heapless::String<{ sigil_id::SIGIL_MAX }> = heapless::String::new();
    // Change gate for the per-tick set_ping push (peer, state, result, cooling).
    let mut ping_prev_push: Option<(
        heapless::String<{ sigil_id::SIGIL_MAX }>,
        i32,
        heapless::String<{ sigil_id::SIGIL_MAX }>,
        bool,
    )> = None;
    // Receiver etiquette: exact-duplicate frame guard + a 2s absorb window so
    // rapid re-pings don't restack the pulse/chime (the PINGACK still goes out
    // at the protocol level — see smol_mesh::handle_rx).
    let mut ping_rx_last: Option<(u8, u16)> = None;
    let mut ping_rx_gate_until: Option<Instant> = None;
    // #58 pop-over-everything: the framebuffer app a ping SUSPENDED to take the
    // panel for the pulse. `Some(app)` means "resume this game once the pulse
    // ends" — the fb was freed + the Slint scene brought up so the pulse could
    // composite; on dismiss we re-launch through #31's resume path (state kept).
    let mut ping_resume_app: Option<AppState> = None;
    /// Deferred ping VISUAL (#58): `(fire_at, from_id, mac)`.
    ///
    /// The chime fires the instant a ping lands, but the pulse choreography does a
    /// FULL-FRAME repaint (~200 ms) and the TX DMA ring only buffers 48 ms — so a
    /// repaint anywhere inside the 700 ms melody starves the feeder and the chime
    /// came out intermittently. The ring cannot grow: 16 KB of .bss trips the 70 KB
    /// stack floor (#65) and 16 KB of heap starved the launcher into an allocation
    /// failure (both tried, both reverted). So the repaint waits for the clip
    /// instead — sound is instant, picture follows.
    let mut ping_visual_due: Option<(Instant, u8, [u8; 6])> = None;

    // === Settings hub (v0.9.0, #49) — NETWORK flow state ===
    // The hub is scene-resident (no framebuffer); the WiFi creds flow is
    // scan-first: picker rows come from `scan_list` (dedup'd, strength-sorted,
    // capped to the picker's 6 rows), the keyboard edits ONE field at a time
    // (Rust owns the buffer; Slint displays what push_kb sends).
    #[derive(Clone, Copy, PartialEq)]
    enum NetEdit {
        None,
        Ssid,
        Pass,
    }
    // (ssid, secured) per picker row — pick index == model index.
    let mut scan_list: heapless::Vec<(heapless::String<32>, bool), 6> = heapless::Vec::new();
    let mut net_view: i32 = 0; // 0 hub pages · 1 picker · 2 keyboard (Rust-owned)
    let mut net_edit = NetEdit::None;
    let mut net_status: i32 = 0; // 0 idle · 1 connecting · 2 connected · 3 failed
    let mut pending_ssid: heapless::String<32> = heapless::String::new();
    let mut kb_buf: heapless::String<64> = heapless::String::new();
    let mut kb_plain = false; // show-password eye
    let mut kb_bksp_held = false;
    let mut kb_bksp_next = Instant::now();
    // OTA status line (SYSTEM page), the port of the old fb Settings field:
    // `&'static` so ota_http's error strings drop straight in.
    let mut ota_status_text: &'static str = "";
    // Boot pushes for the hub's static-ish rows (also re-pushed on scene resume).
    shell.set_node_id(node_id as i32);
    shell.set_touch_sound(touch_sound);
    shell.set_mesh_enabled(mesh_enabled);
    shell.set_wifi_intent(!watch_cfg.wifi_off);
    shell.set_net_current(watch_cfg.ssid.as_str());
    shell.set_volume(volume, muted);
    shell.set_button_actions(
        boot_short.label(),
        boot_long.label(),
        pwron_short.label(),
        pwron_long.label(),
    );
    let mut mesh_channel_pinned = false;
    let mut last_mesh_peers: u8 = 0;
    let mut next_diag = Instant::now() + Duration::from_secs(30);
    // UI-loop heartbeat state (#75) — see the beat block in the loop below.
    let mut loop_beats: u32 = 0;
    let mut next_beat = Instant::now() + Duration::from_secs(BEAT_SECS);
    // Deferred config persistence (#75). `Some(t)` = `watch_cfg` differs from
    // flash and last changed at `t`; the flush block below picks a quiet moment.
    let mut cfg_dirty_at: Option<Instant> = None;
    // Heap LOW-WATER between beats. A 15s sample misses the trough: the beat
    // series showed 51K -> 17K -> 32K while the watch was being used, so the
    // real minimum during an app open is somewhere below what any beat printed.
    // Sampling every iteration and reporting the floor is what tells us how
    // close an interaction actually came to the OOM that panicked at 7168 B.
    let mut heap_low: usize = usize::MAX;
    // Time-sync provenance for the DIAG record (tsrc/tage fields).
    let mut sync_src: &str = "none";
    let mut last_sync = Instant::now();

    // OTA rollback-safety: a freshly-OTA'd image boots "on trial" (PendingVerify
    // when the bootloader has auto-rollback on). Once the app has run
    // OTA_HEALTHY_UPTIME without crashing, confirm the slot Valid so the bootloader
    // keeps it; a bricked image that never reaches that point auto-rolls-back.
    // `boot_instant` is the health-window start; `ota_marked_valid` is the one-shot
    // latch. See net::ota_http::mark_valid_if_pending.
    const OTA_HEALTHY_UPTIME: Duration = Duration::from_secs(10);
    let boot_instant = Instant::now();
    let mut ota_marked_valid = false;
    // UPDATE-FIRMWARE (#53): the job (WiFi window, attempts, re-arm, the
    // download itself) lives in net_task now. Main renders its OtaPhase —
    // status line, toasts, and the Staged reboot — off phase EDGES.
    let mut prev_ota_phase = crate::net::net_task::OtaPhase::Idle;
    // Streaming scan (#53): last consumed rows generation; a bump re-pulls
    // the published rows into the picker.
    let mut last_scan_seq: u32 = 0;
    // REBOOT-with-OTA (power page): armed when the reboot tap queued an
    // update first; the reset fires on the job's terminal phase (Staged
    // reboots via the OTA arm) or this deadline, whichever comes first.
    let mut reboot_deadline: Option<Instant> = None;

    // =========================================================================
    // REALTIME BUDGET (#53): >10 ms of blocking in any arm of this loop IS A
    // BUG. The loop is the UI — render and touch share it — so a stalled arm
    // is a frozen watch. WiFi connect/scan, the boot burst (NTP/MQTT/weather)
    // and OTA downloads live in net_task; MQTT sessions in climate_task; mic
    // capture in its own task. Talk to them via channels/signals and render
    // from their published state — NEVER await radio or sockets here.
    //
    // Documented exemptions (each measured, none silent):
    //   - full-frame Slint renders: 90–170 ms hard floor on this panel;
    //     tracked per frame via debug_console::record_frame (`perf`).
    //   - flash config saves: ~ms-scale sector programs; the XIP stall is
    //     physics (cache off while programming), kept rare + edge-triggered.
    //   - voice PTT: parks the loop for the hold BY DESIGN (finger on glass,
    //     dedicated screen); flagged via debug_console::arm_exempt.
    //   - wake/interaction one-offs: display_on settle (20 ms), boot-button
    //     debounce (200 ms) — deliberate interaction latencies.
    // Enforcement: debug-console builds time every loop body (ArmTimer RAII,
    // continue-paths included); `perf` reports arm_max_us / arm_over10ms.
    // =========================================================================
    loop {
        let touch_held = touch_int.is_low();
        let button_held = boot_button.is_low();
        // Pre-select peek at the net state (#53): the AOD arms below defer
        // light sleep while the RADIO IS BUSY, so they need the verdict
        // BEFORE the tick/sleep decision. "Busy" is derived from the pin
        // verdict rather than `wanted` (review F2): mesh_pin_ok is true only
        // when the radio is up, unassociated, not connecting/scanning, with
        // no holds and no OTA — i.e. quiescent BY CONSTRUCTION — so its
        // negation also covers connect tails after a hold drops and scan
        // sweeps, which `wanted` alone missed. A never-started radio is
        // trivially quiescent. The authoritative per-tick snapshot is re-read
        // after the wake select.
        let net_radio_busy = {
            let s = crate::net::net_task::snapshot();
            s.radio_started && !s.mesh_pin_ok
        };

        let tick = if touch_held || button_held {
            Duration::from_millis(16)
        } else if screen_state == 0 {
            Duration::from_secs(30)
        } else if screen_state == 1 {
            // AOD: wake often enough that the minute flip never looks stuck.
            // debug-console builds AND BLE-on release builds skip AOD
            // light-sleep (the raise detector runs on THIS tick instead of the
            // 700ms sleep-poll), so match the sleep-poll cadence there — and
            // so does a radio-busy window (#53, sleep deferred, bounded);
            // only a sleeping release build keeps the lazy 5s (the sleep
            // block self-paces at 700ms).
            if cfg!(feature = "debug-console") || ble_on || net_radio_busy {
                Duration::from_millis(700)
            } else {
                Duration::from_secs(5)
            }
        } else {
            match app_state {
                // Sound meter + spectrum are a live display; pace them
                // explicitly. (In the grouped arm below, a Sound overlay would
                // otherwise inherit the underlying page's cadence — often 1 Hz —
                // so the meter sampled one 16 ms window/sec and read silence.)
                // 66ms (15Hz): still smooth for a meter/spectrum, but halves the
                // scene-render load that was blocking the executor and starving
                // the capture DMA. The #30 FFT (softfloat, ~few ms) also rides
                // this cadence — one 256-pt transform per tick.
                AppState::Sound => Duration::from_millis(66),
                AppState::Watchface
                | AppState::Launcher
                | AppState::Wled
                | AppState::Hunt
                | AppState::Energy
                | AppState::Climate
                | AppState::Lights
                | AppState::Ping
                | AppState::Voice
                | AppState::Theme
                | AppState::Settings => {
                    // Slint animations (launcher slide, flings) need frame pacing;
                    // otherwise pace by the visible page's live-data cadence.
                    if app_state == AppState::Hunt {
                        // Warmer/colder wants a responsive feel; the RSSI EWMA +
                        // 1.5s trend lag keep 4 Hz from flickering.
                        Duration::from_millis(250)
                    } else if shell.has_active_animations()
                        || shell.hints_pending()
                        || shell.ping_pulse_active()
                    {
                        // hints_pending: a wake gesture-hint window is running —
                        // its bloom/fade tweens need frames, and between phase
                        // edges draw_if_needed no-ops (cheap ticks, ≤3.2s).
                        // ping_pulse_active (#35): same pattern — the greeting
                        // pulse's stage edges + ring tweens ride this cadence
                        // for its ≤4.2s window.
                        Duration::from_millis(33)
                    } else {
                        match shell.page() {
                            1 => Duration::from_millis(100), // sensors live
                            2 => Duration::from_secs(2),     // system
                            3 | 4 => Duration::from_secs(1), // power / mesh refresh
                            _ => {
                                // Clock: 33ms while the gyro toy is live, else 1Hz.
                                if gyro_enabled {
                                    Duration::from_millis(33)
                                } else {
                                    Duration::from_secs(1)
                                }
                            }
                        }
                    }
                }
                _ => Duration::from_millis(33),
            }
        };
        // Mesh Familiar cadence override: a holder must beat every ~1.5 s and
        // a non-holder must notice a dead holder within FAM_LOST_MS (~12 s),
        // so the idle 10/30 s sleeps are capped while the mesh is up.
        let tick = if esp_now_peer_added && mesh_enabled {
            if familiar.needs_fast_tick() {
                tick.min(Duration::from_millis(400))
            } else {
                tick.min(Duration::from_secs(3))
            }
        } else {
            tick
        };
        // #22 voice-latch cadence override: while an early PTT press waits
        // for WiFi, tick ≤100ms so the release-cancel debounce (3 reads ≈
        // 300ms) and the ready→auto-fire hop stay snappy. NET_WAKE already
        // wakes us the instant the phase flips; this bounds the finger polls.
        // Latch lifetime is ≤30s (and ≤~300ms once the finger lifts).
        let tick = if voice_latch.is_some() {
            tick.min(Duration::from_millis(100))
        } else {
            tick
        };

        // In debug-console builds, skip AOD light-sleep entirely (fall through to the
        // console-aware select4 below) so unattended UI tests can drive the watch —
        // injected commands can't wake the HP core out of `sleep_light`.
        // `sleep_cal_ok` (#43): if the boot-time RC_FAST calibration probe timed
        // out, esp-hal's own sleep-entry calibration would too and `sleep_light`
        // would panic with a divide-by-zero (esp32c6.rs:665) — skip light sleep
        // on such units the same way (logged once at boot).
        // `!ble_on` (v0.8.7 hotfix): entering `sleep_light` with the BLE
        // controller active LOCKS UP the chip (frozen screen, dead USB —
        // observed on both watches the day BLE-on could first survive to an
        // idle: v0.8.6's toggle persistence). Also matches intent: BLE-on
        // means "be trackable" (#37/#39 room presence) and adverts can't be
        // sent from light sleep anyway — use the tick-idle AOD path instead
        // (same as debug-console builds). Battery tradeoff is the user's,
        // via the BLE toggle.
        //
        // `!net_radio_busy` (#53, review F2): with the connect machine in
        // net_task, an association attempt/scan sweep can now be in flight
        // while THIS loop idles into AOD — the old inline machine made that
        // impossible (the loop was the one connecting). Light-sleeping
        // mid-WPA-handshake is the same hazard class as the BLE lockup
        // above, so defer light sleep until the radio is quiescent by
        // construction (pin-verdict-derived, covering connect tails and
        // scans, not just `wanted`); bounded (the burst gives up after 180 s,
        // the idle backstop drops user intent) and the tick-idle AOD path
        // covers the meantime.
        if screen_state == 1
            && !cfg!(feature = "debug-console")
            && sleep_cal_ok
            && !ble_on
            && !net_radio_busy
            // `!charging` (2026-07-28): do not light-sleep while on USB power.
            //
            // AOD light sleep parks the HP core and wakes on a 700 ms poll timer
            // (AOD_POLL_MS), so every interaction can wait out that cycle — on the
            // bench that reads as "super slow responding", which is exactly what JP
            // hit. It only showed on ONE watch because the `!ble_on` guard above
            // means a BLE-on watch never light-sleeps at all; the BLE-off one did,
            // and felt sluggish while its twin felt fine on identical firmware.
            //
            // Light sleep exists to save battery. Plugged in there is no battery to
            // save, so the trade is all cost and no benefit — and a watch on a
            // bench is plugged in essentially always.
            //
            // NOTE `is_charging()` is the WRONG signal on its own: it reports
            // "battery actively charging", which goes FALSE once the pack is full
            // even with USB still attached. A topped-off watch on USB therefore
            // still light-slept. Kept as a cheap early-out, but AOD_LIGHT_SLEEP
            // below is what actually holds the line.
            && !charging
            // AOD_LIGHT_SLEEP: master gate, currently OFF — for RESPONSIVENESS.
            //
            // What this DOES fix (measured): AOD light sleep parks the HP core and
            // wakes on a 700 ms poll (AOD_POLL_MS), so interactions wait out that
            // cycle. On a BLE-OFF watch — the only kind that reaches this path,
            // since `!ble_on` above blocks it otherwise — that reads as "super slow
            // responding", observed 2026-07-28 with the serial spinning on
            // `[AOD-SLEEP] woke cause=`. A BLE-ON watch never light-slept, so this
            // flag makes an accidental split uniform rather than inventing a regime.
            //
            // What this does NOT fix, stated plainly: it was ALSO tried as a fix for
            // mythic-throne's hard freezes and it did not work — that watch still
            // froze 4/4 reboots with this flag false. Do not read this gate as a
            // freeze fix; see issue #75, where five software hypotheses (AOD wake,
            // mesh+sleep, bad USB hub, light sleep itself, corrupt config) were each
            // tested and rejected, and the evidence points at hardware.
            //
            // Cost: standby battery when UNPLUGGED. Acceptable on bench units.
            // Flipping it back to true should be paired with fixing whatever makes
            // the wake latency user-visible, not just re-enabling it.
            //
            // (The "executor paused -> mesh quiesces" claim further down is wrong
            // regardless: pausing the Embassy executor does not park the radio
            // blob's esp-rtos tasks. Worth correcting separately.)
            && AOD_LIGHT_SLEEP
        {
            // AOD light sleep (#29, now default — tap-wake confirmed on glass)
            // + WRIST-RAISE wake (polling): park the HP core in light sleep
            // instead of WFI-idling. Wake on a short poll timer OR touch (GPIO15)
            // OR boot button (GPIO9), both active-low. GPIO wake needs BOTH the
            // pin armed (`wakeup_enable`) AND the GpioWakeupSource trigger in the
            // wake set — the timer-only source would never wake on the pins. The
            // clock is the external PCF85063 (sleep-safe); embassy-time (TIMG0)
            // pauses, so it lags real time by the sleep span — fine, AOD repaints
            // from the RTC minute. `sleep_light` blocks (executor paused → mesh
            // quiesces).
            //
            // WRIST-RAISE / BATTERY TRADEOFF: the QMI8658 INT is NOT wired to a
            // wake-capable GPIO on this board (vendor BSP BSP_CAPS_IMU=0, no INT
            // macro; only the FT3168 touch INT is a wake input), so there is no
            // hardware wake-on-motion. We POLL instead: the timer is dropped from
            // 60 s to 700 ms so each poll can read the accel (kept live at 62.5Hz
            // by imu.power_down — accel on, gyro off) and test the raise gesture.
            // That is ~85x more light-sleep wakes than the old 60 s AOD timer.
            // Each poll is cheap (a light-sleep re-entry + one 6-byte I2C accel
            // read, sub-ms) and — unlike a real wake — SKIPS the ES7210/mic
            // re-init below, so the average current adder is modest vs. keeping
            // the HP core awake. Side benefit: the AOD minute-repaint lag drops
            // from up to 60 s to <1 s.
            const AOD_POLL_MS: u64 = 700;
            let timer_wake =
                TimerWakeupSource::new(core::time::Duration::from_millis(AOD_POLL_MS));
            let gpio_wake = GpioWakeupSource::new();
            let _ = touch_int.wakeup_enable(true, WakeEvent::LowLevel);
            let _ = boot_button.wakeup_enable(true, WakeEvent::LowLevel);
            let t0 = Instant::now();
            // #43: re-assert the REF_TICK divider feeding the RC_FAST
            // calibration that `sleep_light` runs internally — cheap (one MMIO
            // write per AOD poll), and shields against anything since boot
            // (radio glue, a future esp-hal) having cleared the bits, which
            // would bring back the divide-by-zero.
            esp_hal::peripherals::PCR::regs()
                .ctrl_tick_conf()
                .modify(|_, w| unsafe {
                    w.fosc_tick_num().bits(255);
                    w.tick_enable().set_bit()
                });
            rtc_lp.sleep_light(&[&timer_wake, &gpio_wake]);
            let cause = wakeup_cause();
            // Disarm so normal falling-edge IRQ handling resumes.
            let _ = touch_int.wakeup_enable(false, WakeEvent::LowLevel);
            let _ = boot_button.wakeup_enable(false, WakeEvent::LowLevel);
            // PWRON poll re-arm (#48): embassy-time paused during the sleep,
            // so `next_pkey` set to a pre-sleep `+250ms` may never elapse.
            // Backdate it to the pre-sleep stamp — the poll below then runs
            // on every 700ms AOD wake, keeping the power key live in AOD.
            next_pkey = t0;

            // Wrist-raise: read one accel sample and test the tilt-to-wake
            // gesture. Accel is alive during AOD (power_down keeps it at 62.5Hz),
            // so this is a single cheap I2C read. On a raise, wake to bright
            // exactly like a tap (the display stays ON through AOD — only the
            // brightness was dropped — so no display_on() is needed here).
            let mut raised = false;
            if let Ok(a) = imu.read_accel() {
                raised = raise_detector.update(a);
            }
            if raised {
                last_interaction = Instant::now();
                display.set_brightness(brightness);
                screen_state = 3;
                next_flush = last_interaction;
                shell.set_aod(false);
                shell.hint_wake();
                shell.request_redraw();
                println!("[AOD-SLEEP] wrist-raise -> bright");
            }

            // Mic recovery is only needed on a REAL wake that resumes normal
            // operation (touch / button / raise) — NOT on the frequent poll-timer
            // wakes that go straight back to light sleep (the mic is idle in AOD).
            // Light sleep clock-gated the I2S peripheral, stalling the silent-TX
            // clock + the mic RX DMA; re-arm both and re-init the ES7210 (a
            // separate I2C chip) so the Sound app / Voice never read exact zeros
            // after the AOD. Gating this to real wakes is what keeps 700 ms
            // polling from paying the ES7210 re-init ~85x/min.
            let real_wake = raised || touch_int.is_low() || boot_button.is_low();
            if real_wake {
                mic_capture::CLOCK_REARM.store(true, core::sync::atomic::Ordering::Relaxed);
                mic_capture::RX_REARM.store(true, core::sync::atomic::Ordering::Relaxed);
                let _ = mic_adc.init();
            }

            // embassy-time froze during sleep, so the loop's next_rtc gate won't
            // refresh last_dt — force a read now so the AOD minute repaint and the
            // wall-clock AOD->off (below) both see the real time (#29 DS1).
            if let Ok(dt) = rtc.get_time() {
                last_dt = Some(dt);
            }
            println!(
                "[AOD-SLEEP] woke cause={:?} embassy_lag_ms={} real={}",
                cause,
                (Instant::now() - t0).as_millis(),
                real_wake
            );
        } else {
            // debug-console adds a wake source (a queued synthetic-input
            // command) so the console drives the loop without waiting out the
            // idle tick. Zero cost when nothing is queued.
            //
            // Both variants also select STATE_WAKE: an accepted MQTT state
            // frame (climate/energy/lights) repaints on the next executor pass
            // instead of sitting in the shared mutex for up to a full idle tick
            // (1s on the HA screens — the biggest firmware-side term of the
            // press→render round trip). Coalescing Signal: bursts wake once.
            // NET_WAKE (#53) is the same pattern for net_task state: WiFi
            // phase flips, streaming scan rows, OTA progress, NTP/weather
            // handoffs all render on the next pass instead of a stale tick.
            #[cfg(feature = "debug-console")]
            let _ = embassy_futures::select::select3(
                embassy_futures::select::select4(
                    Timer::after(tick),
                    touch_int.wait_for_falling_edge(),
                    boot_button.wait_for_falling_edge(),
                    debug_console::wait_inject(),
                ),
                crate::net::mqtt_climate::STATE_WAKE.wait(),
                crate::net::net_task::NET_WAKE.wait(),
            )
            .await;
            #[cfg(not(feature = "debug-console"))]
            let _ = embassy_futures::select::select(
                embassy_futures::select::select4(
                    Timer::after(tick),
                    touch_int.wait_for_falling_edge(),
                    boot_button.wait_for_falling_edge(),
                    crate::net::mqtt_climate::STATE_WAKE.wait(),
                ),
                crate::net::net_task::NET_WAKE.wait(),
            )
            .await;

            // Wrist-raise for debug-console builds: they SKIP the AOD light-sleep
            // block above (so automator tests aren't interrupted), which is where
            // the sleep-path raise detector lives — leaving tilt-to-wake dead in
            // test images. Run the SAME detector here whenever the screen is in
            // AOD (the AOD cadence arm ticks this path at 700ms to match). Same
            // wake actions as the sleep path; no mic re-arm needed (no light sleep
            // happened, so the I2S clock was never gated).
            if screen_state == 1 {
                let mut raised = false;
                if let Ok(a) = imu.read_accel() {
                    raised = raise_detector.update(a);
                }
                if raised {
                    last_interaction = Instant::now();
                    display.set_brightness(brightness);
                    screen_state = 3;
                    next_flush = last_interaction;
                    shell.set_aod(false);
                    shell.hint_wake();
                    shell.request_redraw();
                    println!("[AOD] wrist-raise -> bright (no-sleep path)");
                }
            }
        }

        // Loop-body watchdog (#53): times this iteration from wake to loop
        // tail via RAII drop — every `continue` path included. Reported by
        // the console `perf` command as arm_max_us / arm_over10ms.
        #[cfg(feature = "debug-console")]
        let _arm_timer = debug_console::ArmTimer::start();

        let now = Instant::now();
        let dt_ms = (now - last_frame).as_millis() as u32;
        last_frame = now;

        // === UI-loop heartbeat (#75) ===
        // This closes the observability gap that cost five wrong hypotheses
        // about the "frozen watch". When this loop wedges, the panel holds its
        // last drawn frame and serial goes quiet — but serial is ALSO quiet
        // when the watch is merely idle with nothing to report, and the
        // esp-rtos threads (net_task, the WiFi blob) keep logging in BOTH
        // cases because they are not on this executor. From outside, "wedged"
        // and "fine" were literally the same observation, which is why a hang
        // was misread as AOD light sleep, then a bad USB hub, then a corrupt
        // config, then dying hardware.
        //
        // A beat emitted FROM THIS LOOP is the discriminator: if beats stop
        // while [NET] lines keep arriving, the UI loop is wedged and nothing
        // else is. Prints once per BEAT_SECS, so it costs one line per beat
        // and no measurable time — cheap enough to keep in release builds,
        // where the freezes actually happen (debug-console builds disable AOD
        // light sleep and so cannot reproduce them).
        loop_beats = loop_beats.wrapping_add(1);
        let heap_now = esp_alloc::HEAP.free();
        if heap_now < heap_low {
            heap_low = heap_now;
        }
        if now >= next_beat {
            println!(
                "[LOOP] beat={} up={}s heap={} low={}",
                loop_beats,
                now.as_secs(),
                heap_now,
                heap_low
            );
            next_beat = now + Duration::from_secs(BEAT_SECS);
            heap_low = heap_now; // per-window floor, not lifetime
        }

        // === Net snapshot (#53) ===
        // ONE read per tick: the only view of WiFi/scan/OTA state this loop
        // uses. net_task owns the controller; changes wake us via NET_WAKE.
        let net = crate::net::net_task::snapshot();
        let wifi_connected = net.phase.connected();

        // === IMU gating ===
        let need_imu = screen_state >= 2
            && (gyro_enabled
                || app_state == AppState::Maze
                || app_state == AppState::Tetris
                || app_state == AppState::Flappy
                || (app_state == AppState::Watchface
                    && shell.page() == slint_shell::PAGE_SENSORS));
        if need_imu && !imu_powered {
            let _ = imu.power_up();
            imu_powered = true;
        } else if !need_imu && imu_powered {
            let _ = imu.power_down();
            imu_powered = false;
        }
        if need_imu {
            if let Ok(a) = imu.read_accel() {
                accel = (a.x, a.y, a.z);
            }
            if let Ok(g) = imu.read_gyro() {
                gyro_data = ((g.x * 10.0) as i16, (g.y * 10.0) as i16, (g.z * 10.0) as i16);
            }
            if let Ok(t) = imu.read_temperature() {
                imu_temp = (t * 10.0) as i16;
            }
        }

        // === RTC 1Hz ===
        // Stash the reading; the shell arm pushes it via shell.set_time (which
        // no-ops until the second actually ticks). Read at state >= 1 too so the
        // AOD minute-gated repaint has a fresh `last_dt` to compare against.
        if screen_state >= 1 && now >= next_rtc {
            if let Ok(dt) = rtc.get_time() {
                // Feed the notification wall clock (#32): arrival stamps and
                // age labels ride the PCF85063, not embassy-time (which AOD
                // light-sleep freezes).
                crate::notify::set_wall_clock(
                    dt.day,
                    dt.hours as u32 * 3600 + dt.minutes as u32 * 60 + dt.seconds as u32,
                );
                last_dt = Some(dt);
            }
            next_rtc = now + Duration::from_secs(1);
        }

        // === Pedometer ===
        // The hardware step counter runs even while the IMU is "powered
        // down" (gyro off, accel on). One cheap 3-byte I2C read per minute.
        if now >= next_step_poll {
            if let Ok(s) = imu.read_step_count() {
                last_steps = s;
                shell.set_steps(s);
            }
            next_step_poll = now + Duration::from_secs(60);
        }

        // === Battery ===
        if now >= next_battery {
            if let Ok(pct) = power.get_battery_percent() {
                batt_pct = pct;
                batt_mv = power.get_battery_voltage().unwrap_or(0);
                charging = power.is_charging().unwrap_or(false);
                shell.set_battery(batt_pct, batt_mv, charging);
                // Feed the BLE Battery Service (read + notify).
                crate::peripherals::ble::BATTERY_PERCENT
                    .store(batt_pct, core::sync::atomic::Ordering::Relaxed);
                // Low-battery notification (#32): edge-triggered under 15%,
                // re-armed at 20% or on charge — one on-wrist warning per
                // discharge, not a nag stream.
                if batt_pct < 15 && !charging && !low_batt_notified {
                    low_batt_notified = true;
                    let mut body: heapless::String<32> = heapless::String::new();
                    use core::fmt::Write;
                    let _ = write!(body, "{batt_pct}% - charge soon");
                    crate::notify::push(
                        crate::notify::Source::Battery,
                        "Battery low",
                        body.as_str(),
                    );
                } else if low_batt_notified && (charging || batt_pct >= 20) {
                    low_batt_notified = false;
                }
            }
            next_battery = if screen_state == 0 {
                now + Duration::from_secs(600)
            } else {
                now + Duration::from_secs(180)
            };
        }

        // === Power key (#48: AXP2101 PWRON, polled) ===
        // The side button reaches the firmware ONLY as a latched PMIC IRQ bit
        // (no GPIO, no INT line on this board) — one 1-byte I2C read per 250ms
        // (~100us at 400kHz), write-1-to-clear on a hit. Latency budget:
        // long-press latches at 1.5s (IRQLEVEL) + <=250ms poll + a render, so
        // the menu lands well before the 4s hardware OFFLEVEL cutoff.
        //
        // screen_state 0 (panel off, 30s ticks): a latched event can be up to
        // 30s stale -> DISCARD instead of acting (the read already cleared
        // it). A phantom menu on the next wake would be worse than a dead
        // key; from screen-off the hardware ladder still works (hold to 4s =
        // hard poweroff) and any wake (tap/raise/BOOT) re-arms the key within
        // 250ms. Making PWRON itself a wake source needs the PMIC INT line,
        // which isn't routed — documented follow-up in #48.
        if now >= next_pkey {
            next_pkey = now + Duration::from_millis(250);
            // While the menu is up, keep its VBUS caption honest at this same
            // cadence (one status read) — plugging/unplugging USB flips what
            // SHUTDOWN will actually do, and 180s battery-cadence lag lies.
            if shell.power_menu_open() {
                shell.set_vbus(power.is_vbus_in().unwrap_or(false));
            }
            match power.poll_power_key() {
                Ok(Some(key)) if screen_state == 0 => {
                    println!("[PKEY] {:?} discarded (screen off, stale)", key);
                }
                Ok(Some(key)) => {
                    last_interaction = now;
                    // Capture the PRE-wake brightness state: the wake-vs-volume
                    // nuance (#59) hinges on whether the screen was already on.
                    let was_bright = screen_state == 3;
                    if screen_state < 3 {
                        // Wake-to-bright (AOD/dim; the panel is already ON in
                        // states 1-2, so no display_on() dance is needed).
                        display.set_brightness(brightness);
                        screen_state = 3;
                        next_flush = now;
                        shell.set_aod(false);
                        if key == PowerKey::Short {
                            // Same wake seam as tap/raise -> same hints.
                            shell.hint_wake();
                        }
                        shell.request_redraw();
                    }
                    // #59 button mapping: PWRON short acts ONLY when the screen
                    // was already bright (else the press is consumed by the wake
                    // — the power button MUST still wake the watch). PWRON long
                    // always acts after waking (a deliberate hold), preserving
                    // the #48 "long-press → power menu" from any lit/dim state.
                    match key {
                        PowerKey::Long => {
                            println!("[PKEY] long-press -> {:?}", pwron_long);
                            pending_button = Some(pwron_long);
                        }
                        PowerKey::Short => {
                            if was_bright {
                                println!("[PKEY] short-press -> {:?}", pwron_short);
                                pending_button = Some(pwron_short);
                            } else {
                                println!("[PKEY] short-press: wake (mapped action consumed)");
                            }
                        }
                    }
                }
                Ok(None) => {}
                Err(_) => println!("[PKEY] I2C poll failed"),
            }
        }

        // === Audio out: amp + codec sequencing (#23) ===
        // Edge-triggered and cheap (I2C only on a change). The play_pcm call
        // sites also invoke this inline for a same-tick amp raise; this
        // per-tick pass guarantees the DROP side (and any missed raise) even
        // when the queueing code path bails early.
        audio_out::service_amp(&mut amp_en, &mut audio_codec);
        // #58b: prove the chime actually streamed (streamed/total mono bytes).
        // The first feeder rework was SILENT — the clock task idles on
        // PLAYBACK.receive() and nothing woke it — and the console's "ok chime"
        // ack looked identical to success, so playback is now measurable.
        if let Some((sent, total)) = audio_out::chime_done_take() {
            println!("[AUDIO] ping chime streamed {sent}/{total} B");
        }

        // === Touch ===
        // No swipe-preview animation on the C6: the full-frame RGB565
        // snapshots it needs (2x412KB) don't exist without PSRAM. Swipes
        // switch pages directly instead.
        let mut swipe_event = None;
        let mut swipe_start_y: u16 = 0;
        let mut tap_event = false;
        let mut touch_point: Option<crate::peripherals::touch::TouchPoint> = None;
        let int_low = touch_int.is_low();
        let touch_active = screen_state >= 2 && (int_low || was_touching);
        was_touching = int_low;
        if touch_active {
            if let Ok((point, event)) = touch.poll() {
                touch_point = point;
                if let Some(swipe) = event {
                    swipe_event = Some(swipe.direction);
                    swipe_start_y = swipe.start_y;
                    tap_event = swipe.direction == SwipeDirection::Tap;
                }
            }
        }

        // === Synthetic input injection (debug-console) ===
        // Drain ONE queued command and merge it into the SAME variables the real
        // touch path just wrote, so it flows through `shell.handle_touch` (and
        // framebuffer `AppInput`) identically. A tap is press+release across two
        // ticks; the take() re-arms the wake signal so the release lands next
        // tick. `injected_this_tick` feeds the wake check below so an injection
        // lights the screen exactly like a real touch.
        #[cfg(feature = "debug-console")]
        let injected_this_tick = {
            if let Some(inj) = debug_console::take_inject() {
                match inj {
                    debug_console::Inject::Touch { point, swipe, start_y, tap } => {
                        touch_point = point;
                        swipe_event = swipe;
                        swipe_start_y = start_y;
                        if tap {
                            tap_event = true;
                        }
                    }
                    debug_console::Inject::Launch(idx) => {
                        // Same cell a launcher tile tap sets; the launch drain in
                        // the shell arm raises the app identically.
                        if let Some(st) = crate::apps::registry::launch_state(idx) {
                            shell.req.launch.set(Some(st));
                        }
                    }
                    debug_console::Inject::Home => {
                        shell.set_launcher_open(false);
                        shell.set_switcher_open(false);
                        shell.set_shade_open(false);
                        fb = None;
                        app_state = AppState::Watchface;
                    }
                }
                true
            } else {
                false
            }
        };
        #[cfg(not(feature = "debug-console"))]
        let injected_this_tick = false;

        // === BOOT button press machine (#59: short vs long) ===
        // Runs BEFORE the wake block so a press-start captures the PRE-wake
        // screen state — a press that only wakes the watch never also fires its
        // mapped action (the wake-vs-action nuance, matching PWRON). A long
        // press fires AT the threshold while still held (immediate feedback);
        // a short press fires on release if the long didn't. Replaces the old
        // is_low() launcher toggles.
        {
            let boot_low = boot_button.is_low();
            if boot_low && boot_press_start.is_none() {
                boot_press_start = Some(now);
                boot_long_fired = false;
                boot_wake_consumed = screen_state < 3; // this press is waking
            }
            if boot_low && !boot_long_fired {
                if let Some(t0) = boot_press_start {
                    if (now - t0).as_millis() >= BOOT_LONG_MS {
                        boot_long_fired = true;
                        if !boot_wake_consumed {
                            pending_button = Some(boot_long);
                        }
                    }
                }
            }
            if !boot_low {
                if let Some(t0) = boot_press_start {
                    let held = (now - t0).as_millis();
                    // Short press = released before the long threshold, wasn't a
                    // wake, and past a small debounce (spurious sub-40ms blips).
                    if !boot_long_fired && !boot_wake_consumed && held >= 40 {
                        pending_button = Some(boot_short);
                    }
                    boot_press_start = None;
                }
            }
        }

        // === Screen sleep/wake state machine ===
        let any_touch = touch_int.is_low();
        if any_touch || swipe_event.is_some() || tap_event || boot_button.is_low() || injected_this_tick {
            last_interaction = now;
            if screen_state < 3 {
                if screen_state == 0 {
                    display.display_on();
                    Timer::after(Duration::from_millis(20)).await;
                }
                // A real wake from AOD/off (tap, button, injected input) — not
                // a dim→bright re-touch — shimmers the nav gesture hints.
                if screen_state <= 1 {
                    shell.hint_wake();
                }
                display.set_brightness(brightness);
                screen_state = 3;
                next_flush = now;
                // Real AOD is task 11; for now just make sure the AOD flag never
                // sticks, and force one full repaint so we don't wake onto a
                // stale (or app-clobbered) frame.
                shell.set_aod(false);
                shell.request_redraw();
            }
        }
        let idle_secs = (now - last_interaction).as_secs();
        // AOD (light sleep) freezes embassy-time, so idle_secs stalls at ~15 and
        // the 180s->off below can never fire from it. Drive the AOD->off from the
        // external PCF85063 wall clock: 165 s in AOD == 180 s total idle (AOD is
        // entered at the 15 s mark). sod wrap is handled mod 86_400 (#29 DS1).
        if screen_state == 1 {
            if let Some(dt) = last_dt.as_ref() {
                let sod_now =
                    dt.hours as u32 * 3600 + dt.minutes as u32 * 60 + dt.seconds as u32;
                let aod_real = (sod_now + 86_400 - aod_entry_sod) % 86_400;
                if aod_real >= 165 {
                    display.set_brightness(0x00);
                    display.display_off();
                    screen_state = 0;
                    shell.set_aod(false);
                    println!("[AOD-SLEEP] {}s real in AOD -> screen off", aod_real);
                }
            }
        }
        if idle_secs >= 180 && screen_state > 0 {
            display.set_brightness(0x00);
            display.display_off();
            screen_state = 0;
        } else if idle_secs >= 15 && screen_state > 1 {
            // A shell modal (switcher/shade) blocks AOD: dimming into an AOD
            // clock OVER a modal would be dishonest — go dark like any
            // non-clock page instead.
            if app_state == AppState::Watchface
                && shell.page() == slint_shell::PAGE_CLOCK
                && !shell.modal_open()
            {
                display.set_brightness(0x18);
                screen_state = 1;
                shell.set_aod(true);
                // Force the first AOD repaint next iteration (99 != any minute)
                // so the dim overlay appears immediately, not at the next flip.
                aod_last_minute = 99;
                // Stamp the wall-clock AOD-entry time for the sleep-safe timeout.
                if let Some(dt) = last_dt.as_ref() {
                    aod_entry_sod =
                        dt.hours as u32 * 3600 + dt.minutes as u32 * 60 + dt.seconds as u32;
                }
            } else {
                display.set_brightness(0x00);
                display.display_off();
                screen_state = 0;
            }
        } else if idle_secs >= 8 && screen_state > 2 {
            display.set_brightness(0x40);
            screen_state = 2;
        }
        // Power menu (#48) is transient: it never survives the screen
        // sleeping — waking later onto a live SHUTDOWN row would be a
        // foot-gun. Same-tick as the AOD/off transition, so the sleep frame
        // renders without it.
        if screen_state <= 1 && shell.power_menu_open() {
            shell.set_power_menu_open(false);
        }

        // === WiFi intent (the machine itself lives in net_task, #53) ===
        if wifi_toggle_request && (now - last_wifi_idle_check).as_millis() >= 1000 {
            wifi_toggle_request = false;
            last_wifi_idle_check = now;
            if wifi_has_creds {
                // Flip against the CURRENT association intent: net.wanted is
                // the union of holds, so a toggle while a burst/session holds
                // WiFi reads as "turn OFF" (Drop(User) also clears the burst;
                // session holds keep the link, exactly like the old per-tick
                // wifi_want re-raise did).
                let turn_on = !net.wanted;
                if !crate::net::net_task::send(if turn_on {
                    crate::net::net_task::NetCmd::Raise(crate::net::net_task::Hold::User)
                } else {
                    crate::net::net_task::NetCmd::Drop(crate::net::net_task::Hold::User)
                }) {
                    // Queue full (a download in flight — review F4): re-latch
                    // the tap; this debounced arm re-derives the direction
                    // and retries in ~1 s instead of eating user intent.
                    wifi_toggle_request = true;
                }
                println!("[WIFI] toggled -> {}", if turn_on { "ON" } else { "OFF" });
                // Persist the WiFi INTENT (#46 wifi bit, config v5): only the
                // USER toggle writes it — the automatic drops (NTP burst done,
                // idle timeout, session close) leave the persisted "auto"
                // intent alone, so this stays edge-triggered and flash-cheap.
                if watch_cfg.wifi_off != !turn_on {
                    watch_cfg.wifi_off = !turn_on;
                    // Deferred (#75): mark dirty; the flush block writes once at a
                    // quiet moment. An inline erase here can hang the watch.
                    cfg_dirty_at = Some(now);
                }
                shell.set_wifi_intent(!watch_cfg.wifi_off);
            } else {
                // No stored SSID: the STA can't associate, so a toggle can't do
                // anything. Surface it (reuse the RAM-busy toast) instead of a
                // silent dead button. The creds requirement is intentional —
                // they're entered in the Settings app.
                shell.set_toast("No WiFi credentials \u{2014} set in Settings");
                toast_active = true;
                toast_until = now + Duration::from_secs(3);
                println!("[WIFI] tap ignored: no credentials");
            }
        }
        // A WIFI tap arriving inside the 1000ms debounce window is NOT dropped:
        // wifi_toggle_request stays latched and the branch above processes it
        // once the window clears — rate-limits WiFi start/stop without losing
        // the tap (the old `else if { = false }` silently ate it).

        // Connect machine, link-loss detection, reconnect backoff, the boot
        // burst (NTP/MQTT/weather) and scanning all run in net_task now
        // (#53) — the arms below only consume its published results. Under a
        // dead AP this loop never blocks: the worst case is a status dot.

        // Settings-hub connect feedback, derived from the published phase:
        // associated → connected; 3+ consecutive failures → failed (the old
        // inline machine's give-up threshold, now just a UI verdict — the
        // backoff keeps retrying behind it).
        if settings_connect_pending {
            if wifi_connected {
                net_status = 2;
                shell.set_net_status(net_status);
                settings_connect_pending = false;
            } else if net.connect_fails >= 3 {
                net_status = 3;
                shell.set_net_status(net_status);
                settings_connect_pending = false;
            }
        }
        // Notification (#32): the connect give-up threshold, on-wrist —
        // preserved from the inline machine it rode in on. Deduped by source:
        // a down AP keeps failing behind the backoff (connect_fails stays
        // >= 3) and must not stack a card per attempt; after a dismissal the
        // still-failing link re-raises one card, matching the old
        // once-per-burst retrigger.
        if net.connect_fails >= 3 && !crate::notify::has_source(crate::notify::Source::Wifi) {
            crate::notify::push(
                crate::notify::Source::Wifi,
                "WiFi failed",
                "3 attempts - check network",
            );
        }

        // Safety net: association wanted + 5 min idle -> drop the user/burst
        // intent (rate-limited; session/voice/OTA holds are their own owners').
        if net.wanted && idle_secs >= 300 && (now - last_wifi_idle_check).as_secs() >= 60 {
            let _ = crate::net::net_task::send(crate::net::net_task::NetCmd::Drop(
                crate::net::net_task::Hold::User,
            ));
            last_wifi_idle_check = now;
        }

        // One-shot NTP handoff from the burst: main owns the RTC + mesh, so
        // the time is APPLIED here (the socket query ran in net_task).
        if let Some(unix) = crate::net::net_task::take_ntp_unix() {
            let (h, m, s) = set_rtc_from_unix(&mut rtc, unix);
            println!("[NTP] {h:02}:{m:02}:{s:02} (US Pacific), unix={unix}");
            sync_src = "ntp";
            last_sync = now;
            mesh.set_time_authoritative(unix, now.as_secs());
            if let Ok(dt) = rtc.get_time() {
                last_dt = Some(dt);
            }
            println!("[NTP] synced - RTC set, mesh authority claimed");
        }
        // One-shot weather handoff (fetched in the same burst window).
        if let Some((temp_f, code)) = crate::net::net_task::take_weather() {
            last_weather = Some((temp_f, code));
            shell.set_weather(Some(temp_f), code);
        }

        // OTA rollback-safety: once the app has stayed alive OTA_HEALTHY_UPTIME
        // (peripherals up + this loop ticking), confirm the running slot so the
        // bootloader stops treating a freshly-OTA'd image as on-trial and won't
        // revert it on the next boot. WiFi-independent (a credential-less watch
        // still confirms a good image) and one-shot regardless of outcome.
        if !ota_marked_valid && now.duration_since(boot_instant) >= OTA_HEALTHY_UPTIME {
            if let Err(e) = crate::net::ota_http::mark_valid_if_pending(&mut *flash.lock().await) {
                println!("[OTA] mark-valid failed: {e}");
            }
            ota_marked_valid = true;
        }

        // === OTA render arm (the job runs in net_task, #53) ===
        // The download, its 45 s WiFi window, and the 3-attempt re-arm loop
        // all live in net_task; this arm turns OtaPhase EDGES into the status
        // line, toasts, and the Staged reboot. Works from ANY screen (games
        // included) — exactly the old hoisted executor's coverage, minus the
        // minutes-long loop stall.
        if net.ota != prev_ota_phase {
            use crate::net::net_task::OtaPhase;
            match net.ota {
                OtaPhase::Idle => {}
                OtaPhase::WaitingWifi => {
                    ota_status_text = "Connecting WiFi\u{2026}";
                    shell.set_ota_status(ota_status_text);
                }
                OtaPhase::Downloading { pct } => {
                    // Live percent (the old blocking executor could never
                    // paint one). The static fallback keeps scene-recreate
                    // re-pushes sane; the formatted line rides on top.
                    ota_status_text = "Updating\u{2026}";
                    let mut line: heapless::String<24> = heapless::String::new();
                    use core::fmt::Write as _;
                    let _ = write!(line, "Updating\u{2026} {pct}%");
                    shell.set_ota_status(line.as_str());
                }
                OtaPhase::Retrying { attempt } => {
                    println!("[OTA] retrying (attempt {attempt} failed; WiFi reconnects under the job)");
                    ota_status_text = "Retrying update\u{2026}";
                    shell.set_ota_status(ota_status_text);
                    shell.set_toast("Update retrying\u{2026}");
                    toast_until = Instant::now() + Duration::from_secs(20);
                    toast_active = true;
                }
                OtaPhase::Staged => {
                    println!("[OTA] staged - rebooting to apply");
                    ota_status_text = "Staged \u{2013} rebooting";
                    shell.set_ota_status(ota_status_text);
                    if app_state == AppState::Settings && screen_state >= 2 {
                        shell.render(&mut display);
                        Timer::after(Duration::from_millis(1200)).await;
                    }
                    esp_hal::system::software_reset();
                }
                OtaPhase::Failed { msg } => {
                    ota_status_text = msg;
                    shell.set_ota_status(ota_status_text);
                    // Notification (#32): the final give-up persists in the
                    // shade after the toast fades. ("Staged" is deliberately
                    // NOT posted — the ring is RAM and the staged path reboots
                    // 1.2s later.) Both old failure paths — download give-up
                    // AND the 45s WiFi window — funnel through this one edge.
                    crate::notify::push(crate::notify::Source::Ota, "Update failed", msg);
                    let mut toast: heapless::String<64> = heapless::String::new();
                    let _ = toast.push_str("Update failed: ");
                    let _ = toast.push_str(msg);
                    shell.set_toast(toast.as_str());
                    toast_until = Instant::now() + Duration::from_secs(5);
                    toast_active = true;
                    // REBOOT-with-OTA: the update this reboot queued is dead;
                    // honor the reboot now (edge-triggered, so a stale Failed
                    // from an earlier update can never false-fire).
                    if reboot_deadline.is_some() {
                        println!("[OTA] reboot-queued update failed - rebooting anyway");
                        esp_hal::system::software_reset();
                    }
                }
            }
            prev_ota_phase = net.ota;
        }
        // REBOOT-with-OTA deadline backstop (edge above handles the fast path).
        if reboot_deadline.is_some_and(|t| now >= t) {
            println!("[OTA] reboot-queued update still pending at deadline - rebooting");
            esp_hal::system::software_reset();
        }

        // === Push-OTA announce accept ===
        // `ota_http::handle_announce` (fed by both MQTT paths) already applied
        // the BUILD_EPOCH monotonicity gate; anything taken here is a go. Same
        // flow as the Settings tap: queue on net_task (it raises the WiFi hold
        // and runs the window), toast here.
        if let Some(ann) = crate::net::ota_http::take_announce() {
            if net.ota.active() {
                println!("[OTA] push: build {} ignored (update already pending)", ann.build);
            } else {
                println!("[OTA] push: build {} queued (zero-touch)", ann.build);
                ota_status_text = "Updating\u{2026}";
                shell.set_ota_status(ota_status_text);
                shell.set_toast("Updating firmware\u{2026}");
                toast_until = now + Duration::from_secs(30);
                toast_active = true;
                let _ = crate::net::net_task::send(crate::net::net_task::NetCmd::Ota {
                    url: ann.url,
                });
            }
        }

        // TIME-SHARE steady state: whenever WiFi is down but the radio is up,
        // pin ESP-NOW to the fleet's fixed channel. The pin DECISION is
        // net_task's now (#53, `mesh_pin_ok`), preserving the v0.9.1
        // arbitration verbatim: the pin yields to ANY WiFi intent — a pending
        // OTA (watch #2: pin → link lost → the MQTT/OTA window never
        // stabilizes) and association attempts (mythic-throne: pinning ch6
        // between attempts dropped auth frames — AuthenticationExpired at
        // -61dBm masquerading as dead RX for two days). Main still executes
        // the set_channel because the mesh owns the esp_now handle.
        // Level-reconciled BOTH ways — a bonus over v0.9.1: after a scan
        // sweep the verdict returns true and the mesh re-pins ch6 instead of
        // idling on whatever channel the sweep stopped at.
        //
        // FRESH read, not the tick-start `net` snapshot (review F1): the awaits
        // above (a cfg_save flash program in the toggle arm) can park this loop
        // while net_task processes the very Raise that just went out — pinning
        // ch6 off a stale true here is exactly the a5a4c27 auth-frame hazard.
        let mesh_pin_ok = crate::net::net_task::snapshot().mesh_pin_ok;
        if mesh_pin_ok && !mesh_channel_pinned {
            match esp_now.set_channel(crate::net::smol_mesh::MESH_CHANNEL) {
                Ok(()) => {
                    mesh_channel_pinned = true;
                    println!(
                        "[MESH] pinned to ch{}",
                        crate::net::smol_mesh::MESH_CHANNEL
                    );
                }
                Err(e) => println!("[MESH] set_channel failed: {e:?}"),
            }
        }
        if !mesh_pin_ok && mesh_channel_pinned {
            mesh_channel_pinned = false; // rides the AP/scan channel meanwhile
        }

        // === SMOLv1 mesh (ESP-NOW) ===
        if net.radio_started {
            if !esp_now_peer_added {
                let peer = esp_radio::esp_now::PeerInfo {
                    interface: esp_radio::esp_now::EspNowWifiInterface::Station,
                    peer_address: esp_radio::esp_now::BROADCAST_ADDRESS,
                    lmk: None,
                    channel: None,
                    encrypt: false,
                };
                match esp_now.add_peer(peer) {
                    Ok(())
                    | Err(esp_radio::esp_now::EspNowError::Error(
                        esp_radio::esp_now::Error::PeerExists,
                    )) => {
                        esp_now_peer_added = true;
                        println!("[MESH] up as node id{node_id:03}");
                    }
                    Err(e) => println!("[MESH] add_peer failed: {e:?}"),
                }
            }
            if esp_now_peer_added && mesh_enabled {
                let now_ms = now.as_millis();
                let uptime_secs = now.as_secs();
                mesh.tick(&mut esp_now, now_ms, uptime_secs).await;
                let peers = mesh.peer_count(now_ms) as u8;
                if peers != last_mesh_peers {
                    last_mesh_peers = peers;
                }
                // DIAG record every 60s: full field set in spec order (the HA
                // dashboard parses positionally), zeros where the watch has
                // no equivalent counter yet.
                if now >= next_diag {
                    let mut rec: heapless::String<240> = heapless::String::new();
                    use core::fmt::Write as _;
                    let tage = if sync_src == "none" { 0 } else { (now - last_sync).as_secs() };
                    let _ = write!(
                        rec,
                        "DIAG|slot=ota_0|rst=unknown|boot=0|ota=none|up={}|heap={}|hmin=0\
                         |btn=0|btnl=0|fok=0|ffl=0|vok=0|vfl=0|loss=0|rtt=0|rx={}|tx=0\
                         |led=off:off|tage={}|tsrc={}|net=0:ok|brk=baked|otah=slot\
                         |fwd=0|dedup=0|ttl=0|hop=1|dlseq=0|dfwd=0",
                        uptime_secs,
                        esp_alloc::HEAP.free(),
                        mesh.other_frames_heard,
                        tage,
                        sync_src,
                    );
                    mesh.broadcast_diag(&mut esp_now, rec.as_bytes()).await;
                    next_diag = now + Duration::from_secs(60);
                }
                // World Snake mesh service (only while the app is active):
                // feed it the mesh Unix clock (food/treasure buckets converge
                // fleet-wide) and drain its phase-jittered 5 Hz SNK snapshot.
                if app_state == AppState::WorldSnake {
                    if let Some(unix) = mesh.unix_now(uptime_secs) {
                        world_snake.set_unix(unix);
                    }
                    let mut snk = [0u8; crate::apps::world_snake::SNK_TX_BUF];
                    if let Some(n) = world_snake.pending_tx(&mut snk) {
                        crate::net::smol_mesh::send_bounded(
                            &mut esp_now,
                            &esp_radio::esp_now::BROADCAST_ADDRESS,
                            &snk[..n],
                        )
                        .await;
                    }
                }
                // RELAY leaf uplink: a fresh DIAG-style stat message every
                // ~15s while a peer is alive, then bounded retransmit of
                // whatever fragments the gateway's RELAYACK still misses.
                if mesh.relay_emit_due(now_ms) {
                    let mut tele: heapless::String<192> = heapless::String::new();
                    use core::fmt::Write as _;
                    let _ = write!(
                        tele,
                        "WATCH|up={}|heap={}|batt={}|mv={}|chg={}|peers={}|scr={}:{}",
                        uptime_secs,
                        esp_alloc::HEAP.free(),
                        batt_pct,
                        batt_mv,
                        u8::from(charging),
                        last_mesh_peers,
                        page_scr_name(shell.page()),
                        shell.page() as u8,
                    );
                    mesh.relay_emit(&mut esp_now, tele.as_bytes(), now_ms).await;
                }
                mesh.relay_retransmit(&mut esp_now, now_ms).await;
                while let Some(rx) = esp_now.receive() {
                    // SNK frames route to World Snake when it's active; they
                    // also fall through to mesh.handle_rx (peer proof of life).
                    if app_state == AppState::WorldSnake
                        && rx.data().starts_with(crate::apps::world_snake::SNK_PREFIX)
                    {
                        world_snake.handle_rx(rx.data());
                    }
                    // Per-frame receive RSSI (dBm) — Marauder's Watch EWMA.
                    let rssi =
                        rx.info.rx_control.rssi.clamp(i8::MIN as i32, i8::MAX as i32) as i8;
                    let event = mesh.handle_rx(
                        &mut esp_now,
                        rx.info.src_address,
                        rx.data(),
                        Some(rssi),
                        now_ms,
                        uptime_secs,
                    )
                    .await;
                    match event {
                        Some(MeshEvent::TimeAdopted { unix, from_id }) => {
                            let (h, m, s) = set_rtc_from_unix(&mut rtc, unix);
                            sync_src = "mesh";
                            last_sync = now;
                            println!(
                                "[MESH] RTC set from mesh (id{from_id}): {h:02}:{m:02}:{s:02}"
                            );
                            if let Ok(dt) = rtc.get_time() {
                                last_dt = Some(dt);
                            }
                        }
                        // CFG `S`: apply live + persist. EDGE-TRIGGERED save —
                        // the gateway re-broadcasts cached configs every ~10s,
                        // so a same-value re-arm must never wear flash.
                        Some(MeshEvent::CfgScreen { page }) => {
                            // Switch the visible page live (ShellUi::set_page
                            // clamps out-of-range). Takes effect immediately on
                            // the watchface; if we're in an app it sets the page
                            // the shell returns to. The save stays edge-triggered.
                            shell.set_page(page as i32);
                            if watch_cfg.default_page != page {
                                watch_cfg.default_page = page;
                                // Deferred (#75): mark dirty; the flush block writes once at a
                                // quiet moment. An inline erase here can hang the watch.
                                cfg_dirty_at = Some(now);
                            }
                        }
                        // CFG `U`: store + persist (edge-triggered, as above).
                        Some(MeshEvent::CfgUnits { temp_f, clk_24h }) => {
                            if watch_cfg.units_temp_f != temp_f
                                || watch_cfg.units_clk_24h != clk_24h
                            {
                                watch_cfg.units_temp_f = temp_f;
                                watch_cfg.units_clk_24h = clk_24h;
                                // Deferred (#75): mark dirty; the flush block writes once at a
                                // quiet moment. An inline erase here can hang the watch.
                                cfg_dirty_at = Some(now);
                            }
                        }
                        // CFG `R`: transient, never persisted. Boot-debounced
                        // (a retained/re-armed `R` within 10s of boot is
                        // consumed but ignored — never a reboot-loop).
                        Some(MeshEvent::CfgReboot) => {
                            if now_ms >= REBOOT_DEBOUNCE_MS {
                                println!("[CFG] remote reboot - software_reset()");
                                esp_hal::system::software_reset();
                            } else {
                                println!("[CFG] reboot ignored (boot debounce)");
                            }
                        }
                        Some(MeshEvent::Fam { frame, rssi }) => {
                            let unix_now = mesh.unix_now(uptime_secs).unwrap_or(0);
                            familiar.ingest(&frame, rssi, now_ms, unix_now);
                        }
                        // #35 receiver magic: a greeting for us. The PINGACK
                        // already went out in handle_rx (protocol-level, like
                        // HELLO→ACK); here is the choreography — dedup, chime,
                        // wake, pulse — or the shade card when it can't land.
                        Some(MeshEvent::Ping { from_id, seq, mac }) => {
                            let dup = ping_rx_last == Some((from_id, seq));
                            let gated =
                                ping_rx_gate_until.is_some_and(|t| Instant::now() < t);
                            ping_rx_last = Some((from_id, seq));
                            if !dup && !gated {
                                ping_rx_gate_until =
                                    Some(Instant::now() + Duration::from_secs(2));
                                let from = ping_sigil(from_id, mac);
                                // (1) The chime IS the ping — ALWAYS play it,
                                // wherever the greeting lands (bright, dim, AOD,
                                // full-off, mid-game). Half-duplex is fine.
                                // Signal the feeder task to play the FULL 480 ms
                                // melody: play_pcm here truncated it to the 128 ms
                                // queue (73 % dropped → near-silent). chime_task /
                                // play_all drain the whole clip; the per-tick
                                // service_amp (below) raises the amp, the feeder
                                // holds samples until it's up (pop insurance).
                                // INSTANT PING: clip released FIRST, and it now
                                // SURVIVES the repaint because the TX ring holds
                                // 256 ms (was 48 ms — see TX_RING_LEN).
                                //
                                // Order matters: queue the clip, raise the amp, then
                                // yield briefly. The yield lets the clock task open
                                // its session and push real samples into the ring
                                // BEFORE the ~200 ms full-frame repaint starves the
                                // executor. With 256 ms buffered, the DMA keeps
                                // playing straight through that stall — which is
                                // what made this unreliable at 48 ms.
                                //
                                // The wait also covers AMP_SETTLE_MS, so it is not
                                // added latency: the amp has to settle anyway, and
                                // skipping it is what made the FIRST ping after idle
                                // silent (AMP_READY is a flag, not physical
                                // readiness).
                                let _ = audio_out::play_chime();
                                audio_out::service_amp(&mut amp_en, &mut audio_codec);
                                Timer::after(Duration::from_millis(140)).await;
                                audio_out::service_amp(&mut amp_en, &mut audio_codec);
                                // (3) ALWAYS log a shade card (#58) — a persistent,
                                // RTC-stamped record that survives the ~4s pulse.
                                // The time in the body keeps distinct pings from
                                // the same peer out of notify's consecutive-dup
                                // suppression (so the unread badge bumps each ping).
                                {
                                    use core::fmt::Write as _;
                                    let mut title: heapless::String<
                                        { crate::notify::TITLE_CAP },
                                    > = heapless::String::new();
                                    let _ = write!(title, "Ping from {}", from.as_str());
                                    let mut body: heapless::String<
                                        { crate::notify::BODY_CAP },
                                    > = heapless::String::new();
                                    match last_dt.as_ref() {
                                        Some(dt) => {
                                            let _ = write!(
                                                body,
                                                "A greeting across the mesh \u{00b7} {:02}:{:02}:{:02}",
                                                dt.hours, dt.minutes, dt.seconds
                                            );
                                        }
                                        None => {
                                            let _ = body
                                                .push_str("A greeting across the mesh");
                                        }
                                    }
                                    crate::notify::push(
                                        crate::notify::Source::System,
                                        title.as_str(),
                                        body.as_str(),
                                    );
                                }
                                // (2) DEFER the visual until the melody is out.
                                // Sound is the ping; the pulse follows ~760 ms
                                // later (clip 700 ms + tail). Deferring rather than
                                // parking the loop keeps mesh/touch alive meanwhile.
                                ping_visual_due =
                                    Some((now + Duration::from_millis(760), from_id, mac));
                                println!(
                                    "[PING] greeting from {} (id{from_id} seq {seq})",
                                    from.as_str()
                                );
                            }
                        }
                        // #35 sender side: our greeting landed — flip the hero
                        // to "delivered to <sigil>" + the confirm tick. Runs
                        // even if the Ping screen was closed meanwhile (the
                        // tick is still meaningful; the UI push is gated on
                        // the screen being open, below).
                        Some(MeshEvent::PingAck { from_id, seq, mac }) => {
                            if ping_outstanding.is_some_and(|(s, _)| s == seq) {
                                ping_outstanding = None;
                                ping_state = 2;
                                ping_result = ping_sigil(from_id, mac);
                                audio_out::play_pcm(tick_pcm);
                                audio_out::service_amp(&mut amp_en, &mut audio_codec);
                                println!(
                                    "[PING] delivered to {} (seq {seq})",
                                    ping_result.as_str()
                                );
                            }
                        }
                        // A peer watch spoke: surface the transcription as a
                        // shade card so it survives the moment (the sender's
                        // own screen shows it live; this is the other wrist).
                        Some(MeshEvent::Say { from_id, text, mac }) => {
                            let from = ping_sigil(from_id, mac);
                            let mut title: heapless::String<{ crate::notify::TITLE_CAP }> =
                                heapless::String::new();
                            use core::fmt::Write as _;
                            let _ = write!(title, "{} said", from.as_str());
                            crate::notify::push(
                                crate::notify::Source::System,
                                title.as_str(),
                                text.as_str(),
                            );
                            // Same arrival cue as a ping so it can't be missed.
                            let _ = audio_out::play_chime();
                            audio_out::service_amp(&mut amp_en, &mut audio_codec);
                            println!("[SAY] from {}: {}", from.as_str(), text.as_str());
                        }
                        None => {}
                    }
                }

                // Deferred ping visual (#58): the melody has played out, so a
                // full-frame repaint can no longer starve the audio feeder.
                if let Some((at, vid, vmac)) = ping_visual_due {
                    if now >= at {
                        ping_visual_due = None;
                        let vfrom = ping_sigil(vid, vmac);
                        // A framebuffer game owns the panel + heap and the Slint
                        // scene is parked, so the pulse cannot composite over it:
                        // suspend the app (state kept, #31), free the fb, un-park
                        // the scene, and arm the resume.
                        if fb.is_some() {
                            ping_resume_app = Some(app_state);
                            sessions.suspend(app_state);
                            fb = None;
                            shell.resume_scene();
                            app_state = AppState::Watchface;
                            println!("[PING] suspended {:?} for the pulse", ping_resume_app);
                        }
                        // Wake to bright from ANY sleep state — a ping is a
                        // can't-miss event. Panel fully off needs display_on() +
                        // the warmup the touch-wake path uses.
                        if screen_state < 3 {
                            if screen_state == 0 {
                                display.display_on();
                                Timer::after(Duration::from_millis(20)).await;
                            }
                            display.set_brightness(brightness);
                            screen_state = 3;
                            next_flush = now;
                            shell.set_aod(false);
                            shell.request_redraw();
                        }
                        last_interaction = Instant::now(); // hold the screen through the pulse
                        shell.ping_pulse_show(vfrom.as_str());
                    }
                }

                // Mesh Familiar tick (fleet #57): arbitration + holder beats,
                // driven every loop alongside mesh.tick. Any frame it emits
                // (heartbeat/handoff) is broadcast on the fleet wire format.
                {
                    let unix_now = mesh.unix_now(uptime_secs).unwrap_or(0);
                    let mut ids = [0u8; 16];
                    let n = mesh.live_peer_ids(now_ms, &mut ids);
                    if let Some(frame) = familiar.tick(&ids[..n], now_ms, unix_now) {
                        mesh.broadcast_fam(&mut esp_now, &frame).await;
                    }
                    // Push the creature UI snapshot to the Slint clock nook
                    // (task 12), gated on change so we don't churn properties.
                    // stage/hunger are age-derived on the Creature, not FamState.
                    let creature = familiar.creature();
                    let fam = FamUi {
                        known: familiar.known(),
                        holding: familiar.is_holder(),
                        mood: familiar.mood(),
                        hunger: creature.hunger_level(unix_now),
                        stage: creature.stage_level(unix_now),
                    };
                    if fam != prev_fam {
                        shell.set_fam(&fam);
                        prev_fam = fam;
                    }
                }
            }
        }

        // === BLE toggle ===
        // First press wakes the parked trouble-host task, which advertises
        // as the per-device sigil (#34, e.g. "eldritch-lantern") and serves
        // the Battery GATT service. The trouble
        // host owns the controller from then on and cannot be torn down at
        // runtime, so "off" requires a reboot. Presses while running flip the
        // PERSISTED intent instead (#46): the next boot honors it — press,
        // reboot, BLE stays off; press again before rebooting to keep it on.
        // (The old raw-HCI scan/device-discovery logging was dropped: the
        // scanner would drive the central role against the same
        // single-connection peripheral host.)
        if ble_toggle_request {
            ble_toggle_request = false;
            let persist_intent;
            if !ble_on {
                ble_on = true;
                persist_intent = true;
                crate::peripherals::ble::BLE_START_REQUEST
                    .store(true, core::sync::atomic::Ordering::Relaxed);
                println!(
                    "[BLE] GATT server start requested ('{}')",
                    net::sigil::get().sigil.as_str()
                );
            } else {
                persist_intent = !watch_cfg.ble_on;
                println!(
                    "[BLE] host can't be stopped at runtime - persisted {} for next boot",
                    if persist_intent { "ON" } else { "OFF (reboot to disable)" }
                );
                // On-glass feedback (#46 follow-up): while the host is running,
                // a press flips ONLY the persisted boot intent — the dot keeps
                // showing the runtime state (still ON), so without this toast a
                // stray second tap silently disarmed BLE-at-boot and the toggle
                // "didn't survive" the next reset. Make the divergence visible.
                shell.set_toast(if persist_intent {
                    "BLE: stays on"
                } else {
                    "BLE: off after reboot"
                });
                toast_active = true;
                toast_until = Instant::now() + Duration::from_secs(3);
            }
            // Persist the toggle (#46 BLE bit, config v4) — edge-triggered
            // like the page/units/theme saves.
            if watch_cfg.ble_on != persist_intent {
                watch_cfg.ble_on = persist_intent;
                if config_offset.is_some() {
                    // Deferred (#75): mark dirty; the flush block writes once at a
                    // quiet moment. An inline erase here can hang the watch.
                    cfg_dirty_at = Some(now);
                }
            }
            power_stats.ble_on = ble_on;
        }

        // Push radio chrome (wifi/ble/mesh-peers) only when it actually changed —
        // set_radios itself is cheap, but this avoids touching the scene every
        // iteration and keeps Slint's dirty tracking meaningful.
        let radios = (wifi_connected, ble_on, last_mesh_peers);
        if radios != prev_radios {
            shell.set_radios(radios.0, radios.1, radios.2);
            prev_radios = radios;
        }

        // === screen off ===
        // State 0 = panel fully off: skip all render/interaction work. State 1
        // (AOD) falls through — the shell arm renders it minute-gated below.
        if screen_state == 0 {
            continue;
        }

        // === Every-touch tick (#49, v0.9.0) ===
        // ONE hoisted hook for BOTH dispatch families below — the Slint shell
        // (tap_event → shell.handle_touch) and the framebuffer apps (AppInput.tap
        // in run_fb_app's caller) — never per-widget. Taps only: swipe/drag
        // frames classify as directional (not Tap) and never set tap_event, and
        // AOD wake-touches don't reach the poll. Skipped while a clip is already
        // in flight (audio_out::busy) and during a PTT hold (RECORDING — the mic
        // half-duplex gate would eat it anyway). Inline service_amp = same-tick
        // amp raise; the clip still starts ≥ one ring of driven silence later
        // (pop insurance, see audio_out).
        if tap_event
            && touch_sound
            && !audio_out::busy()
            && !mic_capture::RECORDING.load(core::sync::atomic::Ordering::Relaxed)
        {
            audio_out::play_pcm(tick_pcm);
            audio_out::service_amp(&mut amp_en, &mut audio_codec);
        }

        // === Mapped button-action dispatch (#59) ===
        // ONE place both button state machines feed (BOOT SM + PWRON poll), run
        // BEFORE the app-state match so it can freely set app_state / launcher /
        // power-menu / volume regardless of which arm runs this tick. Volume
        // actions defer the apply+persist+overlay to a single shared tail so
        // VolUp/VolDown/Mute don't each duplicate it.
        {
            let mut vol_changed = false;
            let mut vol_feedback = false; // play the tick at the NEW level
            if let Some(action) = pending_button.take() {
                match action {
                    ButtonAction::None => {}
                    ButtonAction::VolUp => {
                        muted = false;
                        volume = (volume + 1).min(peripherals::config::VOL_MAX);
                        vol_changed = true;
                        vol_feedback = true;
                    }
                    ButtonAction::VolDown => {
                        muted = false;
                        volume = volume.saturating_sub(1);
                        vol_changed = true;
                        vol_feedback = true;
                    }
                    ButtonAction::Mute => {
                        muted = !muted;
                        vol_changed = true;
                        vol_feedback = !muted; // only the unmute is audible
                    }
                    ButtonAction::PowerMenu => {
                        // Leaving a game: SUSPEND it first (#31) so it stays
                        // resumable, exactly like the fb-arm exit path.
                        if fb.is_some() {
                            sessions.suspend(app_state);
                            fb = None;
                            app_state = AppState::Watchface;
                        }
                        power_menu_request = true;
                    }
                    ButtonAction::Shutdown => {
                        println!("[BTN] shutdown");
                        if power.shutdown().is_err() {
                            println!("[BTN] shutdown I2C failed");
                        }
                    }
                    ButtonAction::Launcher => {
                        if fb.is_some() {
                            sessions.suspend(app_state);
                            fb = None;
                            app_state = AppState::Watchface;
                        }
                        if shell.modal_open() {
                            shell.set_switcher_open(false);
                            shell.set_shade_open(false);
                        } else {
                            let opening = app_state == AppState::Watchface;
                            shell.set_launcher_open(opening);
                            app_state =
                                if opening { AppState::Launcher } else { AppState::Watchface };
                        }
                    }
                    ButtonAction::Ping => {
                        if fb.is_some() {
                            sessions.suspend(app_state);
                            fb = None;
                        }
                        app_state = AppState::Watchface;
                        shell.req.launch.set(Some(AppState::Ping));
                    }
                    ButtonAction::Voice => {
                        if fb.is_some() {
                            sessions.suspend(app_state);
                            fb = None;
                        }
                        app_state = AppState::Watchface;
                        shell.req.launch.set(Some(AppState::Voice));
                    }
                    ButtonAction::Speak => {
                        // On-demand read-aloud. Serviced at the speak site (it
                        // owns the amp/codec borrows), on this pass or the next.
                        // Inert without `tts` (out of ROM — see Cargo.toml).
                        #[cfg(feature = "tts")]
                        {
                            speak_request = true;
                        }
                    }
                }
            }
            if vol_changed {
                last_interaction = now;
                audio_out::set_master_volume(&mut audio_codec, volume, muted);
                shell.set_volume(volume, muted);
                shell.set_volume_overlay_open(true);
                shell.request_redraw(); // snappy HUD even at the 1Hz clock idle
                volume_overlay_until = Some(now + Duration::from_secs(2));
                if watch_cfg.volume != volume || watch_cfg.muted != muted {
                    watch_cfg.volume = volume;
                    watch_cfg.muted = muted;
                    // DEFERRED, not written here — see the flush block below (#75).
                    cfg_dirty_at = Some(now);
                }
                if vol_feedback && !audio_out::busy() {
                    audio_out::play_pcm(tick_pcm);
                    audio_out::service_amp(&mut amp_en, &mut audio_codec);
                }
            }
        }

        // === Deferred config flush (#75) ===
        // A config save is TWO 4 KB sector erase+program cycles (primary +
        // backup mirror, `config::save`). esp-storage performs each as a
        // read-modify-write with interrupts disabled and the flash cache
        // suspended, so for the duration nothing executing from flash runs —
        // no DMA completion handler, no radio-blob servicing.
        //
        // Both volume paths used to do that INLINE, per change. The slider drag
        // path fired per drag SAMPLE, so dragging volume meant dozens of erase
        // pairs back to back. JP's reproduction was exactly this: open the
        // SoundLevel meter (I2S RX DMA streaming, plus a 256-point softfloat FFT
        // per tick) and then adjust the volume — the watch hard-freezes with no
        // panic and TOTAL serial silence from every thread, recoverable only by
        // a physical power cycle. Interrupts being off is precisely why nothing
        // can log it. This project already has a precedent for erases colliding
        // with live execution: the #55 brick, where erasing the sector holding
        // live WiFi rodata killed the app mid-read-modify-write.
        //
        // So: coalesce, and pick a quiet moment. One adjustment = one save,
        // taken once the HUD has closed AND no audio is streaming AND the mic
        // meter is off. `flash-guard` protects WHERE a write lands; this is the
        // missing WHEN.
        if let Some(dirty_at) = cfg_dirty_at {
            let settled = now >= dirty_at + Duration::from_millis(CFG_SETTLE_MS);
            let quiet = !audio_out::busy() && !meter_on;
            // Staleness cap, deliberately ASYMMETRIC. `quiet` is normally true
            // within a tick or two, but a write must never be deferrable forever
            // or a setting is silently lost — so after CFG_MAX_DEFER_S the cap
            // overrides `busy()` (a playback tail is short and the erase can wait
            // it out or ride over it).
            //
            // It does NOT override `meter_on`. The mic meter means I2S RX DMA is
            // streaming continuously, which is half of the reproduction this
            // whole block exists for (SoundLevel open + volume change). Erasing
            // flash there is the hazard, so the meter stays a hard block; it
            // clears as soon as the user leaves that screen, which bounds the
            // wait by an action the user is already taking.
            let stale = now >= dirty_at + Duration::from_secs(CFG_MAX_DEFER_S);
            if settled && !meter_on && (quiet || stale) {
                if cfg_save(flash, config_offset, &watch_cfg).await {
                    println!(
                        "[CFG] saved (deferred{}): vol={} muted={} page={} theme={} ble={} mesh={}",
                        if stale && !quiet { ", staleness cap" } else { "" },
                        watch_cfg.volume,
                        watch_cfg.muted,
                        watch_cfg.default_page,
                        watch_cfg.theme,
                        watch_cfg.ble_on,
                        watch_cfg.mesh_on,
                    );
                } else {
                    println!("[CFG] deferred save FAILED — setting not persisted");
                }
                cfg_dirty_at = None;
            }
        }

        // Volume HUD auto-dismiss (#59): close after the 2s window OR the moment
        // the screen leaves bright (it must never linger into AOD/off; a drag in
        // the hub drain re-arms the deadline).
        if let Some(deadline) = volume_overlay_until {
            if now >= deadline || screen_state < 2 {
                shell.set_volume_overlay_open(false);
                shell.request_redraw(); // clear it promptly at idle cadence
                volume_overlay_until = None;
            }
        }

        // === App state machine ===
        // Snapshot the state we dispatch on THIS iteration. The app→shell guard
        // below ("force a fresh repaint on return") compares against the state we
        // *ran*, not the one we're about to run next — a game arm that exits to
        // Watchface mutates app_state mid-match, so recording app_state at the end
        // would make prev_app_state == app_state at the next top and the guard
        // could never fire. Recording `dispatched` keeps the transition visible.
        let dispatched = app_state;
        match app_state {
            AppState::Watchface
            | AppState::Launcher
            | AppState::Wled
            | AppState::Hunt
            | AppState::Energy
            | AppState::Climate
            | AppState::Lights
            | AppState::Ping
            | AppState::Voice
            | AppState::Sound
            | AppState::Theme
            | AppState::Settings => {
                // Just came back from an app that painted straight to the panel
                // (bypassing Slint) — force one full repaint so we don't sit on a
                // stale game frame that Slint thinks is still valid.
                if !matches!(
                    prev_app_state,
                    AppState::Watchface
                        | AppState::Launcher
                        | AppState::Wled
                        | AppState::Hunt
                        | AppState::Energy
                        | AppState::Climate
                        | AppState::Lights
                        | AppState::Ping
                        | AppState::Voice
                        | AppState::Sound
                        | AppState::Theme
                        | AppState::Settings
                ) {
                    // Returning from a game: the Slint scene was dropped on launch
                    // to free heap for the framebuffer. Recreate it, then re-push
                    // the state the per-tick on-change guards won't refresh on
                    // their own (a fresh scene is all defaults). Runs before the
                    // page-data match below, so prev_page = -1 re-pushes page data
                    // this same iteration.
                    shell.resume_scene();
                    prev_page = -1;
                    shell.set_radios(wifi_connected, ble_on, last_mesh_peers);
                    prev_radios = (wifi_connected, ble_on, last_mesh_peers);
                    shell.set_fam(&prev_fam);
                    shell.set_battery(batt_pct, batt_mv, charging);
                    shell.set_brightness_from_raw(brightness);
                    shell.set_gyro(gyro_enabled);
                    shell.set_cpu_mhz(cpu_mhz);
                    shell.set_steps(last_steps);
                    // LP-core status is a one-shot boot push; re-assert it (same
                    // static "idle"/20MHz) so the power row isn't blank after a
                    // scene recreate (wisp's review — same lost-on-recreate class).
                    shell.set_lp_core("idle", 20);
                    // Session badge (#31, same lost-on-recreate class) — this is
                    // also what makes the chip appear right after a game exit
                    // (the suspend happened while the scene was down).
                    shell.set_suspended_count(sessions.len() as i32);
                    // Unread badge (#32, same class): arrivals during a game
                    // are badge-only; surface them now.
                    shell.set_notif_unread(crate::notify::unread() as i32);
                    // Settings-hub state (same lost-on-recreate class): the hub
                    // reads these whenever it next opens; a fresh scene resets
                    // them all to component defaults.
                    shell.set_node_id(node_id as i32);
                    shell.set_touch_sound(touch_sound);
                    shell.set_mesh_enabled(mesh_enabled);
                    shell.set_wifi_intent(!watch_cfg.wifi_off);
                    shell.set_net_current(watch_cfg.ssid.as_str());
                    shell.set_net_status(net_status);
                    shell.set_ota_status(ota_status_text);
                    shell.set_mic_gain_db(mic_capture::GAIN_STEPS_DB[gain_idx] as i32);
                    // #59: volume + button map are also lost on a scene rebuild.
                    shell.set_volume(volume, muted);
                    shell.set_button_actions(
                        boot_short.label(),
                        boot_long.label(),
                        pwron_short.label(),
                        pwron_long.label(),
                    );
                    if let Some((t, c)) = last_weather {
                        shell.set_weather(Some(t), c);
                    }
                    // We only return from a game while awake, so AOD is off; make
                    // it explicit against the fresh scene's default.
                    shell.set_aod(false);
                    if let Some(dt) = last_dt.as_ref() {
                        let _ = shell.set_time(dt);
                    }
                    shell.request_redraw();
                }

                // Power menu (#48): the PWRON long-press poll requested it —
                // raise it now that the scene is guaranteed live (a game exit
                // resumes the scene in the block just above, same tick).
                // Freshen the status the menu shows first: the 180s battery
                // cadence can be stale, and the VBUS caption ("restarts after
                // shutdown") must reflect the cable RIGHT NOW.
                if power_menu_request {
                    power_menu_request = false;
                    if let Ok(pct) = power.get_battery_percent() {
                        batt_pct = pct;
                        batt_mv = power.get_battery_voltage().unwrap_or(0);
                        charging = power.is_charging().unwrap_or(false);
                        shell.set_battery(batt_pct, batt_mv, charging);
                    }
                    shell.set_vbus(power.is_vbus_in().unwrap_or(false));
                    shell.set_power_menu_open(true);
                    println!("[PKEY] long-press -> power menu");
                }

                // Mirror overlay open-state into the scene, feed touch, then
                // reconcile any swipe-driven overlay close (Right-swipe / WLED
                // back-chevron) back into app_state. The shell owns page/launcher
                // navigation via swipes internally; WLED is a scene overlay that
                // shares this Slint branch (no framebuffer of its own).
                // Mirror app_state -> overlay open-flags, feed touch, then
                // reconcile any swipe-driven close back into app_state. All three
                // are table-driven (OVERLAYS in slint_shell.rs) so adding an
                // overlay app doesn't fan out here.
                shell.mirror_overlays(app_state);
                shell.handle_touch(touch_point, swipe_event, swipe_start_y);
                app_state = shell.reconcile_overlay();

                // WLED remote: back-chevron closes; a tapped tile broadcasts a
                // WiZmote frame over ESP-NOW (reusing the mesh block's broadcast
                // peer, which is added whenever the radio is up). Fire-and-forget
                // — WLED controllers listen promiscuously; on-glass channel tuning
                // vs the controller is a hardware follow-up.
                if shell.req.wled_close.take() {
                    shell.set_wled_open(false);
                    app_state = AppState::Watchface;
                }
                if let Some(act) = shell.req.wled_action.take() {
                    if let Some(btn) = wled_button(act) {
                        if net.radio_started && esp_now_peer_added {
                            wled_seq = wled_seq.wrapping_add(1);
                            let frame = wled_wizmote::encode_wizmote(btn, wled_seq, batt_pct);
                            crate::net::smol_mesh::send_bounded(
                                &mut esp_now,
                                &esp_radio::esp_now::BROADCAST_ADDRESS,
                                &frame,
                            )
                            .await;
                            shell.set_wled_status(wled_status(act));
                            println!("[WLED] act={act} seq={wled_seq}");
                        } else {
                            shell.set_wled_status("Radio off \u{2014} enable WiFi/MESH");
                        }
                    }
                }

                // Hunt: back-chevron closes; "next" cycles the roster target; each
                // tick feeds the target's raw RSSI from the mesh roster the watch
                // already tracks (no new radio frames). With no live peers the view
                // reads LOST/reacquiring — hunt is a mesh-dependent game.
                if shell.req.hunt_close.take() {
                    shell.set_hunt_open(false);
                    app_state = AppState::Watchface;
                }
                if app_state == AppState::Hunt {
                    let now_ms = now.as_millis();
                    let mut ids = [0u8; MESH_MAX_ROWS];
                    let n_ids = mesh.live_peer_ids(now_ms, &mut ids);
                    if shell.req.hunt_next.take() || hunt_state.target().is_none() {
                        hunt_state.cycle_target(&ids[..n_ids], now_ms);
                    }
                    // Target's current raw RSSI from the roster (None if unheard).
                    let present_raw = hunt_state.target().and_then(|t| {
                        let mut rows = [PeerView::default(); MESH_MAX_ROWS];
                        let n = mesh.peers(now_ms, &mut rows);
                        rows[..n]
                            .iter()
                            .find(|p| p.id == Some(t))
                            .and_then(|p| p.rssi_dbm.map(|r| r as i32))
                    });
                    let view = hunt_state.update(present_raw, now_ms);
                    shell.set_hunt(&view);
                } else {
                    let _ = shell.req.hunt_next.take(); // drop a late "next" tap
                }

                // Energy overlay is display-only; back-chevron / Right-swipe closes.
                if shell.req.energy_close.take() {
                    energy_active = false; // stop wanting the shared session
                    shell.set_energy_open(false);
                    app_state = AppState::Watchface;
                }

                // === #58: shared HA MQTT session (feeds Climate + Energy screens) ===
                // climate_task holds ONE CONNECT while EITHER screen is open; each
                // screen reads its shared state (ClimateState / EnergyState). Reset
                // `running` on session return (done fires on Ok AND Err); it
                // restarts below if a screen is still open (error resilience).
                if climate_done.try_take().is_some() {
                    climate_running = false;
                }
                // Climate back-chevron / right-swipe → dismiss + stop wanting it.
                if shell.req.climate_closed.take() {
                    // oracle-t9 flush-on-close: if the user tapped ± then left before
                    // the 400ms debounce fired (sent_at still None), publish the
                    // pending setpoint NOW so the adjustment isn't silently deferred
                    // to the next Climate visit. (The session is still draining
                    // cmd_rx this tick — it hasn't torn down yet.)
                    if let Some(p) = climate_pending.as_ref() {
                        if p.sent_at.is_none() {
                            let obj = {
                                let st = climate_state.lock().await;
                                st.entities.get(p.id as usize).map(|(o, _)| o.clone())
                            };
                            if let Some(obj) = obj {
                                let _ = climate_cmds.sender().try_send(
                                    crate::net::mqtt_climate::ClimateCmd::SetTemp {
                                        obj,
                                        temp: p.temp,
                                    },
                                );
                            }
                        }
                    }
                    climate_pending = None; // optimistic state doesn't outlive the screen
                    climate_active = false;
                    shell.set_climate_open(false);
                    println!("[HEAP] climate close: free={}", esp_alloc::HEAP.free());
                    if app_state == AppState::Climate {
                        app_state = AppState::Watchface;
                    }
                }
                // Lights back-chevron / right-swipe → dismiss + stop wanting the
                // session (same cell-close pattern as Climate so the WiFi hold is
                // released below, never stranded).
                if shell.req.lights_closed.take() {
                    lights_pending = None; // optimistic state doesn't outlive the screen
                    lights_noreply_until = None;
                    lights_opened_at = None;
                    lights_active = false;
                    shell.set_lights_open(false);
                    if app_state == AppState::Lights {
                        app_state = AppState::Watchface;
                    }
                }
                // WiFi hold + session start/stop, keyed on "either screen open".
                // The holds are net_task bits now (#53): Session for the HA
                // screens, Voice for STT — each raised on the open edge and
                // dropped on the close edge, so closing the screen(s) frees
                // WiFi and returns the mesh PROMPTLY (oracle-t10 inv b /
                // finding-b), while a manual WiFi-on (Hold::User) is preserved
                // by construction. The edge trackers only flip when the send
                // is accepted, so a full queue (mid-OTA) retries next tick —
                // a hold can never strand silently.
                let climate_session_want = climate_active || energy_active || lights_active;
                if climate_session_want != session_hold_up {
                    let cmd = if climate_session_want {
                        crate::net::net_task::NetCmd::Raise(crate::net::net_task::Hold::Session)
                    } else {
                        crate::net::net_task::NetCmd::Drop(crate::net::net_task::Hold::Session)
                    };
                    if crate::net::net_task::send(cmd) {
                        session_hold_up = climate_session_want;
                    }
                }
                let voice_want = app_state == AppState::Voice;
                if voice_want != voice_hold_up {
                    let cmd = if voice_want {
                        crate::net::net_task::NetCmd::Raise(crate::net::net_task::Hold::Voice)
                    } else {
                        crate::net::net_task::NetCmd::Drop(crate::net::net_task::Hold::Voice)
                    };
                    if crate::net::net_task::send(cmd) {
                        voice_hold_up = voice_want;
                    }
                }
                if climate_session_want {
                    // DHCP gate (phase.ready() == associated + lease): opening
                    // the session before the lease lands made the first TCP
                    // connect fail instantly (no route) — ~10s of "Finding
                    // your room…" on a healthy LAN. Unchanged v0.8.8 gate,
                    // phase-derived now.
                    if net.phase.ready() && !climate_running {
                        climate_open.signal(());
                        climate_running = true;
                    }
                } else if climate_running {
                    climate_close.signal(());
                }
                // Climate screen: route setpoint/mode commands + push the roster.
                if app_state == AppState::Climate {
                    // C4/C5/E2: a ±tap sends an absolute clamped target. Hold it as
                    // `climate_pending` and display it immediately (optimistic);
                    // publish ONCE ~400ms after the last tap (debounce); revert to
                    // authoritative state if HA does not confirm within 5s.
                    if let Some((id, temp)) = shell.req.climate_set_temp.take() {
                        climate_pending = Some(ClimatePending {
                            id,
                            temp,
                            last_tap: Instant::now(),
                            sent_at: None,
                        });
                    }
                    // Mode changes publish immediately (no debounce specced for mode).
                    if let Some((id, mode)) = shell.req.climate_set_mode.take() {
                        let obj = {
                            let st = climate_state.lock().await;
                            st.entities.get(id as usize).map(|(o, _)| o.clone())
                        };
                        if let Some(obj) = obj {
                            let _ = climate_cmds.sender().try_send(
                                crate::net::mqtt_climate::ClimateCmd::SetMode {
                                    obj,
                                    mode: hvac_from_ui(mode),
                                },
                            );
                        }
                    }

                    let st = climate_state.lock().await;
                    // C5 debounce: publish the pending setpoint once, ~400ms after
                    // the last tap settles, so a multi-tap sweep emits one command.
                    if let Some(p) = climate_pending.as_mut() {
                        if p.sent_at.is_none()
                            && Instant::now().duration_since(p.last_tap)
                                >= Duration::from_millis(400)
                        {
                            if let Some(obj) =
                                st.entities.get(p.id as usize).map(|(o, _)| o.clone())
                            {
                                let _ = climate_cmds.sender().try_send(
                                    crate::net::mqtt_climate::ClimateCmd::SetTemp {
                                        obj,
                                        temp: p.temp,
                                    },
                                );
                            }
                            p.sent_at = Some(Instant::now());
                        }
                    }
                    // E2: drop the optimistic value when HA confirms it OR after a
                    // 5s no-confirm timeout (then the display reverts to authority).
                    let clear_pending = if let Some(p) = climate_pending.as_ref() {
                        let confirmed = st
                            .entities
                            .get(p.id as usize)
                            .and_then(|(_, e)| e.set)
                            .map(|s| (s - p.temp).abs() < 0.05)
                            .unwrap_or(false);
                        let timed_out = p
                            .sent_at
                            .map(|t| {
                                Instant::now().duration_since(t) >= Duration::from_secs(5)
                            })
                            .unwrap_or(false);
                        confirmed || timed_out
                    } else {
                        false
                    };
                    if clear_pending {
                        climate_pending = None;
                    }
                    // conn-state: 0 ready · 1 connecting · 2 unreachable.
                    let conn = if !st.entities.is_empty() {
                        0
                    } else if climate_session_want {
                        1
                    } else {
                        2
                    };
                    // C4 optimistic: override the pending entity's setpoint in the
                    // pushed roster so the UI reflects the tap instantly. The UI
                    // reads its stepper base from this model, so the next ±tap
                    // accumulates from the optimistic value rather than the stale
                    // authoritative one.
                    //
                    // #60 OOM fix: gate the push on a fingerprint of exactly what
                    // we'd render (state + optimistic override + conn). Rebuilding
                    // the heap Vec<ClimateCard> + SharedStrings every tick
                    // fragmented the allocator until a ~7 KB alloc OOM-panicked
                    // the watch on this screen. Now the model is pushed only when
                    // the rendered content actually changes.
                    let conn_mix = (conn as u64).wrapping_mul(0x9E3779B97F4A7C15);
                    if let Some(p) = climate_pending.as_ref() {
                        let mut opt = st.clone();
                        if let Some((_, e)) = opt.entities.get_mut(p.id as usize) {
                            e.set = Some(p.temp);
                        }
                        let fp = opt.render_fingerprint() ^ conn_mix;
                        if prev_climate_fp != Some(fp) {
                            shell.set_climate(&opt, conn);
                            prev_climate_fp = Some(fp);
                        }
                    } else {
                        let fp = st.render_fingerprint() ^ conn_mix;
                        if prev_climate_fp != Some(fp) {
                            shell.set_climate(&st, conn);
                            prev_climate_fp = Some(fp);
                        }
                    }
                } else {
                    let _ = shell.req.climate_set_temp.take();
                    let _ = shell.req.climate_set_mode.take();
                }
                // Energy screen: push the live EnergyState from the shared session.
                // conn-state: 0 ready · 1 connecting · 2 unreachable (HA LWT offline).
                if app_state == AppState::Energy {
                    let es = climate_energy.lock().await;
                    // conn precedence (energy-conn-gate fix): "HA unreachable"
                    // only when the avail LWT has actually SAID `offline` this
                    // boot (avail_seen && !online). A missing avail topic —
                    // bridge flow not deployed / retained LWT lost — used to
                    // hard-fail the screen as "unreachable" even while live
                    // state frames were rendering-ready. Now: no avail info +
                    // no data = "connecting"; no avail info + data = live.
                    let conn = if !climate_running {
                        1
                    } else if es.avail_seen && !es.online {
                        2
                    } else if !es.has_data() {
                        // Session up, but no EnergyState frame yet: stay
                        // "connecting" so the UI shows that instead of the
                        // -1% sentinel that battery_pct=None maps to below (luna #1).
                        1
                    } else {
                        0
                    };
                    shell.set_energy(
                        es.battery_pct.map_or(-1, |v| v as i32),
                        es.solar_w.unwrap_or(0),
                        es.grid_w.unwrap_or(0),
                        es.charging,
                    );
                    shell.set_energy_conn(conn);
                }

                // Lights screen (#39): route hero/pill commands + push the
                // room snapshot from the shared session.
                if app_state == AppState::Lights {
                    // A tap queues one command publish (toggle/on/off) and arms
                    // the optimistic "sent" flash. `seq_at_send` pins the state
                    // frame we already had, so ANY later frame (even an identical
                    // payload — HA republishes after acting) resolves the flash.
                    if let Some(a) = shell.req.lights_cmd.take() {
                        let action = match a {
                            1 => crate::net::mqtt_climate::LightsAction::On,
                            2 => crate::net::mqtt_climate::LightsAction::Off,
                            _ => crate::net::mqtt_climate::LightsAction::Toggle,
                        };
                        // Session-phase gate: only queue while the session is UP
                        // or actively CONNECTING (a press during the open
                        // handshake is seconds old at delivery — fine). A press
                        // while the session is DOWN (reconnect backoff, WiFi
                        // drop) used to queue silently and REPLAY at the next
                        // connect — lights flipping on their own many seconds
                        // later. Reject it with the no-reply hint instead.
                        let phase = crate::net::mqtt_climate::SESSION_PHASE
                            .load(core::sync::atomic::Ordering::Relaxed);
                        let seq_now = lights_state.lock().await.seq;
                        if phase != crate::net::mqtt_climate::PHASE_DOWN
                            && climate_cmds
                                .sender()
                                .try_send(crate::net::mqtt_climate::ClimateCmd::Lights(action))
                                .is_ok()
                        {
                            println!(
                                "[LAT] lights cmd queued t={}ms",
                                Instant::now().as_millis()
                            );
                            lights_pending = Some(LightsPending {
                                sent_at: Instant::now(),
                                seq_at_send: seq_now,
                            });
                            lights_noreply_until = None; // a fresh send clears the hint
                        } else {
                            // Immediate, honest feedback: "no reply — try again"
                            // (pending=2) rather than a fake "sending…" that
                            // can't complete.
                            lights_pending = None;
                            lights_noreply_until =
                                Some(Instant::now() + Duration::from_millis(2500));
                        }
                    }

                    let ls = lights_state.lock().await;
                    // First state frame after this screen-open: the "Finding
                    // your room…" duration, measured to the render tick.
                    if ls.has_data() {
                        if let Some(t) = lights_opened_at.take() {
                            println!(
                                "[LAT] lights open->first-state {}ms",
                                Instant::now().duration_since(t).as_millis()
                            );
                        }
                    }
                    // Resolve the optimistic flash: HA's republish landed (seq
                    // moved) → clear; 5s with no reply → revert + transient hint.
                    if let Some(p) = lights_pending.as_ref() {
                        if ls.seq != p.seq_at_send {
                            println!(
                                "[LAT] lights press->state-render {}ms (render tick)",
                                Instant::now().duration_since(p.sent_at).as_millis()
                            );
                            lights_pending = None;
                        } else if Instant::now().duration_since(p.sent_at)
                            >= Duration::from_secs(5)
                        {
                            lights_pending = None;
                            lights_noreply_until =
                                Some(Instant::now() + Duration::from_millis(2500));
                        }
                    }
                    let pending = if lights_pending.is_some() {
                        1
                    } else if lights_noreply_until.is_some_and(|t| Instant::now() < t) {
                        2
                    } else {
                        lights_noreply_until = None;
                        0
                    };
                    // status: 0 finding (no state yet / connecting) · 1 ok ·
                    // 2 no_presence · 3 error — the UI maps 0 to the
                    // "Finding your room…" shimmer.
                    let status = if !ls.has_data() {
                        0
                    } else {
                        match ls.status {
                            crate::net::mqtt_climate::LightsStatus::Ok => 1,
                            crate::net::mqtt_climate::LightsStatus::NoPresence => 2,
                            crate::net::mqtt_climate::LightsStatus::Error => 3,
                        }
                    };
                    shell.set_lights(ls.area.as_str(), ls.on, ls.total, status, pending);
                } else {
                    let _ = shell.req.lights_cmd.take();
                }

                // === Watch-to-watch ping (#35) ===
                // Receiver-pulse dismiss (tap on the pulse, or any swipe while
                // it is up): disarm the auto-dismiss clock WITH the overlay.
                // Drained on every Slint tick — the pulse can bloom over any
                // scene state, not just the Ping screen.
                if shell.req.ping_pulse_tap.take() {
                    shell.ping_pulse_dismiss();
                }
                // #58 pop-over-everything: if a ping SUSPENDED a fb game to take
                // the panel, resume it the instant the pulse ends (tap, swipe,
                // OR the ~4s auto-dismiss — all observed through the one
                // ping_pulse_active flag). Re-launch through #31's resume path
                // (launch cell → take_resume skips setup → state preserved).
                if let Some(app) = ping_resume_app {
                    if !shell.ping_pulse_active() {
                        shell.req.launch.set(Some(app));
                        ping_resume_app = None;
                        println!("[PING] pulse done \u{2014} resuming {app:?}");
                    }
                }
                // Hero tap → one PING broadcast. The Slint gate (mesh-on +
                // not cooling + not mid-flight) already filtered; re-verify
                // here — properties mirror the loop one tick behind.
                if shell.req.ping_send.take()
                    && app_state == AppState::Ping
                    && mesh_enabled
                    && esp_now_peer_added
                    && ping_cooldown_until.is_none_or(|t| now >= t)
                    && ping_outstanding.is_none()
                {
                    ping_seq = ping_seq.wrapping_add(1);
                    mesh.send_ping(&mut esp_now, ping_seq).await;
                    ping_outstanding = Some((ping_seq, now));
                    ping_state = 1; // "sending…" + the static sent ring
                    ping_result.clear();
                    // Etiquette: 3s between pings — the hero recharge sweep.
                    ping_cooldown_until = Some(now + Duration::from_secs(3));
                }
                if app_state == AppState::Ping {
                    // No-reply timeout: 2s without a PINGACK is an honest
                    // "maybe out of range", not an endless "sending…".
                    if let Some((_, sent_at)) = ping_outstanding {
                        if now.duration_since(sent_at) >= Duration::from_secs(2) {
                            ping_outstanding = None;
                            ping_state = 3;
                        }
                    }
                    // Cooldown end re-arms the hero and retires the
                    // delivered / no-reply caption back to idle.
                    let cooling = ping_cooldown_until.is_some_and(|t| now < t);
                    if !cooling {
                        ping_cooldown_until = None;
                        if ping_state == 2 || ping_state == 3 {
                            ping_state = 0;
                        }
                    }
                    // Resolve the hero target from the live roster: the first
                    // live id-known peer (2-watch fleet — with more peers the
                    // hero honestly falls back to "PING THE FLEET" below only
                    // when NONE are known). Sigil via id table / MAC (#34).
                    let now_ms = now.as_millis();
                    let mut rows = [PeerView::default(); MESH_MAX_ROWS];
                    let n = mesh.peers(now_ms, &mut rows);
                    let peer_name = rows[..n]
                        .iter()
                        .filter(|p| p.age_ms < crate::net::smol_mesh::PEER_STALE_MS)
                        .find_map(|p| {
                            p.id.filter(|&id| id != node_id)
                                .map(|id| ping_sigil(id, p.mac))
                        })
                        .unwrap_or_default();
                    // Push gated on change — the strings churn allocs.
                    let snap = (peer_name.clone(), ping_state, ping_result.clone(), cooling);
                    if ping_prev_push.as_ref() != Some(&snap) {
                        shell.set_ping(
                            peer_name.as_str(),
                            ping_state,
                            ping_result.as_str(),
                            cooling,
                        );
                        ping_prev_push = Some(snap);
                    }
                }

                // Voice push-to-talk: on the finger-down Slint reported
                // (voice_ptt_pressed), stream the mic to the STT bridge while the
                // button is HELD, then show the transcript on release.
                //
                // Release is detected off the PHYSICAL touch INT pin, not the Slint
                // `ptt-released` callback: the loop is parked in the stream `.await`
                // for the whole hold, so it can't dispatch Slint pointer events —
                // the callback can't fire until we're already done. `voice_ptt_released`
                // is drained here only to keep the cell from staling (advisory).
                //
                // join (NOT select): `MicPcmSource::next_chunk` returns 0 the instant
                // `RECORDING` clears, so `stream_utterance` self-terminates on release,
                // flushes the final HTTP chunk, and does the STT round-trip. `select`
                // would cancel that mid-flush and drop the transcript.
                let voice_pressed = shell.req.voice_ptt_pressed.take();
                let _ = shell.req.voice_ptt_released.take();
                // STT needs WiFi associated AND DHCP landed. A press before
                // that no longer drops (#22 press-once): it LATCHES — the
                // Voice screen already holds Hold::Voice (raised on the
                // screen-open edge above), so net_task is bringing the link
                // up; when phase goes ready the latch fires the capture
                // through the SAME entry below, exactly as if just pressed.
                let voice_net_ready = net.phase.ready();
                if app_state == AppState::Voice && voice_pressed && !voice_net_ready {
                    voice_latch = Some(now); // re-press refreshes the window
                    voice_latch_up = 0;
                    shell.set_voice_state(5); // connecting — keep holding
                    shell.request_redraw();
                }
                // Latch upkeep (armed only, ≤30s, ticks capped at 100ms):
                // fire when ready + finger still down; cancel on release
                // (3-read debounce on the authoritative I2C finger count —
                // the INT pin goes high for still fingers, the same trap the
                // capture monitor dodges); fail visibly if the window lapses
                // (the shade's "WiFi failed" card rides the existing
                // connect_fails>=3 source, deduped — nothing extra posted).
                let mut voice_latch_fire = false;
                if let Some(t0) = voice_latch {
                    if app_state != AppState::Voice {
                        // Screen closed — Hold::Voice drops on the edge above;
                        // the latch dies with it.
                        voice_latch = None;
                    } else {
                        let finger_down = matches!(touch.read(), Ok(Some(_)));
                        if finger_down {
                            voice_latch_up = 0;
                        } else {
                            voice_latch_up = voice_latch_up.saturating_add(1);
                        }
                        if voice_net_ready && finger_down {
                            // A transient I2C miss (finger_down false for a
                            // tick or two) just defers the fire to the next
                            // 100ms tick — never a junk 180ms capture.
                            voice_latch = None;
                            voice_latch_fire = true;
                        } else if voice_latch_up >= 3 {
                            voice_latch = None; // released while waiting → cancel
                            shell.set_voice_state(0); // back to "Hold to talk"
                            shell.request_redraw();
                        } else if now.duration_since(t0) > VOICE_LATCH_WINDOW {
                            voice_latch = None;
                            shell.set_voice_error("WiFi failed");
                            shell.set_voice_state(4);
                            shell.request_redraw();
                        }
                    }
                }
                if app_state == AppState::Voice
                    && (voice_pressed || voice_latch_fire)
                    && voice_net_ready
                {
                    use core::sync::atomic::Ordering;
                    // Mic is the ES7210 (inited at boot + kept alive); just arm the gate.
                    // Flush stale chunks + reset the live level so the meter starts low.
                    let rx = MIC_CH.receiver();
                    while rx.try_receive().is_ok() {}
                    mic_capture::MIC_LEVEL.store(mic_dsp::DBFS_FLOOR as i32, Ordering::Relaxed);
                    mic_capture::RECORDING.store(true, Ordering::Relaxed);
                    shell.set_voice_state(1); // listening
                    shell.set_voice_level(0.0);
                    shell.render(&mut display); // paint LISTENING ONCE (no repaint during the hold)
                    let mut src = mic_capture::MicPcmSource::new(rx);

                    // ONE monitor future runs alongside the stream during the hold. It does
                    // NOT render (see below) — its only jobs are release-detection + peak-track:
                    //  detect release from the AUTHORITATIVE I2C finger count — NOT the INT pin.
                    //  The INT is a data-ready PULSE that goes high the moment a still finger
                    //  stops generating reports, so the old `touch_int.is_high()` fired ~20ms
                    //  into a hold → 0.3s truncated captures. REG_FINGER_NUM reflects real
                    //  contact. Debounced (3 no-finger reads ≈ 180ms) vs transient I2C misreads;
                    //  a ~20s tick cap backstops a stuck finger / I2C error.
                    // `touch`/`shell`/`display` are borrowed ONLY here; the stream uses
                    // `stack`/`src` → no aliasing.
                    let monitor = async {
                        let mut up = 0u8;
                        let mut peak_dbfs = i32::MIN;
                        let mut ticks: u32 = 0;
                        loop {
                            // Track the loudest window (for the "too quiet" heuristic).
                            // CRITICAL: do NOT render here. Painting the Slint scene blocks
                            // the single-threaded executor for ~tens of ms, starving the
                            // audio-capture DMA → ring overruns → the recording came back
                            // truncated with static/glitch "peaks". LISTENING is painted
                            // ONCE before the join; the live level bar is sacrificed so the
                            // capture stays clean and full-length. (A live bar can return
                            // later once capture runs in its own task, not parked here.)
                            let dbfs = mic_capture::MIC_LEVEL.load(Ordering::Relaxed);
                            if dbfs > peak_dbfs {
                                peak_dbfs = dbfs;
                            }
                            // Authoritative finger-present via I2C (fingers == 0 ⇒ up).
                            let finger_down = matches!(touch.read(), Ok(Some(_)));
                            if finger_down {
                                up = 0;
                            } else {
                                up += 1;
                                if up >= 3 {
                                    break; // released
                                }
                            }
                            ticks += 1;
                            if ticks > 330 {
                                break; // ~20s max-duration cap
                            }
                            Timer::after(Duration::from_millis(60)).await;
                        }
                        mic_capture::RECORDING.store(false, Ordering::Relaxed);
                        shell.set_voice_state(2); // transcribing (STT round-trip in flight)
                        shell.render(&mut display); // one render AFTER release (capture done → safe)
                        peak_dbfs
                    };

                    let (result, peak_dbfs) =
                        join(voice_stt::stream_utterance(stack, &mut src), monitor).await;

                    // Ensure the gate is down (belt-and-suspenders).
                    mic_capture::RECORDING.store(false, Ordering::Relaxed);

                    // The PTT hold parks this loop for the whole utterance BY
                    // DESIGN (see the budget banner at the loop head) — keep
                    // it out of the arm watchdog so `perf` regressions stay
                    // signal, not noise.
                    #[cfg(feature = "debug-console")]
                    debug_console::arm_exempt();

                    match result {
                        Ok(t) if !t.is_empty() => {
                            shell.set_voice_transcript(t.as_str());
                            shell.set_voice_state(3); // result
                            // Share it with the fleet: the other watch shows it
                            // as a shade card. Broadcast, fire-and-forget — a
                            // dropped frame just means no card, which beats a
                            // retransmit protocol for a convenience feature.
                            mesh.send_say(&mut esp_now, t.as_str()).await;
                        }
                        Ok(_) => {
                            // 200 + empty text: tell the user WHY so they can act. A low
                            // peak ⇒ the mic barely heard them (speak up / closer); a
                            // healthy peak ⇒ audio was fine, Azure just found no words.
                            if peak_dbfs < -40 {
                                shell.set_voice_error("Too quiet — speak up");
                            } else {
                                shell.set_voice_error(""); // page shows "No speech heard"
                            }
                            shell.set_voice_state(4);
                        }
                        Err(e) => {
                            shell.set_voice_error(e);
                            shell.set_voice_state(4); // error
                        }
                    }
                    shell.request_redraw(); // paint the transcript/error promptly
                }

                // === Read the newest notification aloud (#read-aloud) ========
                //
                // THE single speak site. Parks this loop for the utterance BY
                // DESIGN — same as the PTT hold above, and for a sharper reason:
                // `PlaybackFeeder::gate_open()` withholds every sample until
                // AMP_READY, which ONLY `audio_out::service_amp` sets, and that
                // needs the amp GPIO + the codec's I2C, both owned here. If the
                // stream ran anywhere that couldn't pump it, every chunk would
                // wait out the 1 s AMP_WAIT_MS failsafe and then drain into a
                // MUTED DAC: silent in the room, fully "successful" in the log.
                // `speak_text` pumps it per chunk — hence the borrows below.
                //
                // No render happens during the call: painting blocks the
                // single-threaded executor for tens of ms and would starve the
                // audio DMA behind a 128 ms queue (same rule the PTT path
                // documents at its monitor future).
                #[cfg(feature = "tts")]
                if speak_request {
                    speak_request = false;
                    if watch_cfg.speak.enabled() && !muted && net.phase.ready() {
                        if let Some(n) = crate::notify::newest() {
                            let text = tts_proto::compose_utterance(
                                crate::notify::source_label(n.source),
                                n.title.as_str(),
                                n.body.as_str(),
                            );
                            // The utterance parks the loop for seconds; keep it
                            // out of the arm watchdog so `perf` stays signal.
                            #[cfg(feature = "debug-console")]
                            debug_console::arm_exempt();
                            // Tell the user this is speech, not a freeze, and
                            // that a tap ends it. Painted BEFORE any audio is
                            // queued — a render mid-stream would starve the
                            // audio DMA behind the 128 ms queue.
                            shell.set_toast("Reading aloud — tap to stop");
                            toast_active = true;
                            toast_until = now + Duration::from_secs(3);
                            shell.render(&mut display);
                            // Finger down = stop. `touch` is borrowed only by
                            // this closure; `amp_en`/`audio_codec` are distinct
                            // bindings, so the borrows don't overlap.
                            let mut stop_on_tap =
                                || matches!(touch.read(), Ok(Some(_)));
                            match voice_tts::speak_text(
                                stack,
                                text.as_str(),
                                &mut amp_en,
                                &mut audio_codec,
                                &mut stop_on_tap,
                            )
                            .await
                            {
                                Ok(s) => println!(
                                    "[TTS] {} {} B ({} ms)",
                                    s.label(),
                                    s.bytes(),
                                    s.duration_ms()
                                ),
                                Err(e) => println!("[TTS] failed: {e}"),
                            }
                        } else {
                            println!("[TTS] nothing to read");
                        }
                    }
                }

                // #28 sound-level meter + #30 spectrum: drain the SHARED ES7210
                // capture → dBFS bar + peak-hold + 12-band FFT spectrum on
                // SoundLevel. Non-blocking (unlike the PTT flow, which parks the
                // loop): update once per tick. Opens the METER gate on entry (mic
                // is the ES7210, inited at boot), closes on exit.
                if app_state == AppState::Sound {
                    if !meter_on {
                        mic_capture::METER.store(true, core::sync::atomic::Ordering::Relaxed);
                        meter_peak = mic_dsp::DBFS_FLOOR;
                        meter_env = mic_dsp::DBFS_FLOOR;
                        spec_env.reset();
                        shell.set_spectrum(spec_env.bars(), spec_env.peaks());
                        meter_on = true;
                    }
                    // Drain ALL buffered chunks each tick; rms (cheap) runs per 16 ms
                    // window, the FFT (softfloat, ~few ms) only on the LAST window.
                    let rx = MIC_CH.receiver();
                    let mut latest_dbfs: Option<f32> = None;
                    let mut samples = [0i16; mic_capture::MONO_CHUNK / 2];
                    let mut last_n = 0usize;
                    while let Ok(chunk) = rx.try_receive() {
                        let n = chunk.len() / 2;
                        for i in 0..n {
                            samples[i] = i16::from_le_bytes([chunk[2 * i], chunk[2 * i + 1]]);
                        }
                        last_n = n;
                        latest_dbfs = Some(mic_dsp::rms_dbfs(&samples[..n]));
                    }
                    if let Some(dbfs) = latest_dbfs {
                        // Bar = fast-attack / slow-release envelope so speech visibly
                        // fills + holds instead of collapsing between syllables.
                        const RELEASE_DB: f32 = 1.5; // per 33 ms tick (~45 dB/s)
                        meter_env = dbfs.max(meter_env - RELEASE_DB).max(mic_dsp::DBFS_FLOOR);
                        // Peak marker: slower decay so it lingers after a transient.
                        meter_peak = (meter_peak - 0.5).max(dbfs).max(mic_dsp::DBFS_FLOOR);
                        shell.set_mic_level(meter_env, meter_peak);
                        // #30: 256-pt real FFT → 12 log bands → per-band bar +
                        // peak-hold envelopes (the meter's feel, per band).
                        let bands = mic_dsp::spectrum_dbfs(&samples[..last_n]);
                        spec_env.update(&bands);
                        shell.set_spectrum(spec_env.bars(), spec_env.peaks());
                    }
                } else if meter_on {
                    // Close the meter gate (ES7210 stays inited; RX idles + discards).
                    mic_capture::METER.store(false, core::sync::atomic::Ordering::Relaxed);
                    meter_on = false;
                }

                // Refresh per-page data immediately on a page switch, then pace it.
                let page = shell.page();
                if page != prev_page {
                    prev_page = page;
                    next_flush = now;
                }
                match page {
                    slint_shell::PAGE_SENSORS => {
                        shell.set_sensors(accel, gyro_data, imu_temp);
                        shell.set_die_temp(die_temp.decidegrees());
                    }
                    slint_shell::PAGE_SYSTEM => {
                        if now >= next_flush {
                            shell.set_system(esp_alloc::HEAP.free(), batt_pct, batt_mv);
                            next_flush = now + Duration::from_secs(2);
                        }
                    }
                    slint_shell::PAGE_POWER => {
                        if now >= next_flush {
                            update_power_stats(
                                &mut power_stats,
                                screen_state,
                                imu_powered,
                                wifi_connected,
                                net.wanted,
                                brightness,
                                batt_mv,
                                batt_pct,
                                charging,
                            );
                            shell.set_power(&power_stats);
                            next_flush = now + Duration::from_secs(1);
                        }
                    }
                    slint_shell::PAGE_MESH => {
                        if now >= next_flush {
                            let mut rows = [PeerView::default(); MESH_MAX_ROWS];
                            let n = mesh.peers(now.as_millis(), &mut rows);
                            shell.set_mesh_rows(node_id, &rows[..n]);
                            next_flush = now + Duration::from_secs(1);
                        }
                    }
                    _ => {}
                }

                // 1Hz clock push (no-ops until the second actually ticks).
                if let Some(dt) = last_dt.as_ref() {
                    // Piggyback the shade's age refresh (#32) on the minute
                    // flip while it's open — "5m" ticks to "6m" in place.
                    if shell.set_time(dt) && dt.seconds == 0 && shell.shade_open() {
                        push_shade(&shell);
                    }
                }

                // Gyro parallax: nudge the clock face by scaled accel while the
                // gyro toy is on (page-0 arm already paces at 33ms for this). Off
                // the clock or with gyro off, the offsets stay at their last value
                // — reset to neutral once so the face doesn't freeze askew.
                if shell.page() == slint_shell::PAGE_CLOCK {
                    if gyro_enabled {
                        shell.set_parallax(accel.0, accel.1);
                    } else {
                        shell.set_parallax(0.0, 0.0);
                    }
                }

                // Drain UI requests raised by the Slint callbacks.
                if let Some(raw) = shell.req.brightness.take() {
                    brightness = raw;
                    display.set_brightness(raw);
                }
                // Sound-app mic-gain stepper: bump the digital-gain index, apply it
                // live (the capture task reads MIC_GAIN_Q8), refresh the readout.
                // Read each cell exactly once (take() also clears it).
                let gain_up = shell.req.mic_gain_up.take();
                let gain_down = shell.req.mic_gain_down.take();
                if gain_up || gain_down {
                    if gain_up {
                        gain_idx = (gain_idx + 1).min(mic_capture::GAIN_STEPS_Q8.len() - 1);
                    } else {
                        gain_idx = gain_idx.saturating_sub(1);
                    }
                    mic_capture::MIC_GAIN_Q8.store(
                        mic_capture::GAIN_STEPS_Q8[gain_idx],
                        core::sync::atomic::Ordering::Relaxed,
                    );
                    shell.set_mic_gain_db(mic_capture::GAIN_STEPS_DB[gain_idx] as i32);
                    shell.request_redraw();
                    // Persist the step (#46 mic-gain byte, config v5) — edge-
                    // triggered; a rail-clamped repeat tap doesn't wear flash.
                    if watch_cfg.mic_gain != gain_idx as u8 {
                        watch_cfg.mic_gain = gain_idx as u8;
                        // Deferred (#75): mark dirty; the flush block writes once at a
                        // quiet moment. An inline erase here can hang the watch.
                        cfg_dirty_at = Some(now);
                    }
                }
                if let Some(scheme) = shell.req.theme.take() {
                    // The picker already set Theme.scheme for instant preview;
                    // sync our stored scheme (so a scene resume restores it) and
                    // persist to flash, edge-triggered like the page/units saves.
                    shell.set_scheme(scheme);
                    if watch_cfg.theme != scheme as u8 {
                        watch_cfg.theme = scheme as u8;
                        if config_offset.is_some() {
                            // Deferred (#75): mark dirty; the flush block writes once at a
                            // quiet moment. An inline erase here can hang the watch.
                            cfg_dirty_at = Some(now);
                        }
                    }
                }
                // === Settings hub drains (v0.9.0, #49) ===
                // Touch-sound toggle: flip + persist (edge-triggered, mirror
                // save). The switch visual IS the feedback — no toast.
                if shell.req.touch_sound_toggle.take() {
                    touch_sound = !touch_sound;
                    shell.set_touch_sound(touch_sound);
                    if watch_cfg.touch_sound != touch_sound {
                        watch_cfg.touch_sound = touch_sound;
                        // Deferred (#75): mark dirty; the flush block writes once at a
                        // quiet moment. An inline erase here can hang the watch.
                        cfg_dirty_at = Some(now);
                    }
                }
                // === Volume + buttons hub drains (#59) ===
                // SOUND-page steppers / mute route through the SAME pending
                // action path as the hardware buttons, so apply+persist+overlay
                // is single-sourced (dispatched above next tick... no — feed it
                // THIS tick by setting pending_button before it's re-checked?
                // The dispatcher already ran this tick; instead apply inline via
                // the shared helper so the touch path is immediate). Simplest:
                // queue onto pending_button for next tick's dispatcher.
                if shell.req.volume_up.take() {
                    pending_button = Some(ButtonAction::VolUp);
                }
                if shell.req.volume_down.take() {
                    pending_button = Some(ButtonAction::VolDown);
                }
                if shell.req.volume_mute.take() {
                    pending_button = Some(ButtonAction::Mute);
                }
                // Volume HUD / SOUND-page slider drag: absolute set (0..1 →
                // 0..15), clears mute, re-arms the 2s dismiss. Applied inline
                // (not via pending_button, which is momentary ±).
                if let Some(frac) = shell.req.volume_changed.take() {
                    let new_level =
                        (frac.clamp(0.0, 1.0) * peripherals::config::VOL_MAX as f32 + 0.5) as u8;
                    let new_level = new_level.min(peripherals::config::VOL_MAX);
                    if new_level != volume || muted {
                        volume = new_level;
                        muted = false;
                        audio_out::set_master_volume(&mut audio_codec, volume, muted);
                        shell.set_volume(volume, muted);
                        if watch_cfg.volume != volume || watch_cfg.muted != muted {
                            watch_cfg.volume = volume;
                            watch_cfg.muted = muted;
                            // A DRAG fires this every sample. Writing here meant
                            // dozens of erase pairs during the drag (#75).
                            cfg_dirty_at = Some(now);
                        }
                    }
                    // Keep the HUD alive while dragging (re-arm the 2s window).
                    shell.set_volume_overlay_open(true);
                    volume_overlay_until = Some(now + Duration::from_secs(2));
                }
                // Buttons page: cycle a mapping slot's action + persist.
                if let Some(slot) = shell.req.button_cycle.take() {
                    let target = match slot {
                        0 => &mut boot_short,
                        1 => &mut boot_long,
                        2 => &mut pwron_short,
                        _ => &mut pwron_long,
                    };
                    *target = target.next();
                    watch_cfg.boot_short = boot_short;
                    watch_cfg.boot_long = boot_long;
                    watch_cfg.pwron_short = pwron_short;
                    watch_cfg.pwron_long = pwron_long;
                    shell.set_button_actions(
                        boot_short.label(),
                        boot_long.label(),
                        pwron_short.label(),
                        pwron_long.label(),
                    );
                    // Deferred (#75): mark dirty; the flush block writes once at a
                    // quiet moment. An inline erase here can hang the watch.
                    cfg_dirty_at = Some(now);
                }
                // UPDATE FIRMWARE (SYSTEM page): same semantics, one queue —
                // net_task raises the WiFi hold, runs the 45 s window and the
                // download; the OTA render arm above paints its progress.
                if shell.req.settings_ota.take() {
                    if !crate::net::ota_http::URL_SET {
                        println!("[OTA] tap: no OTA_URL baked into this build");
                        ota_status_text = "No OTA URL in build";
                    } else if net.ota.active() {
                        println!("[OTA] tap: update already pending");
                    } else {
                        println!("[OTA] tap: queueing update (net_task owns the window)");
                        ota_status_text = if net.phase.ready() {
                            "Updating\u{2026}"
                        } else {
                            "Connecting WiFi\u{2026}"
                        };
                        let _ = crate::net::net_task::send(
                            crate::net::net_task::NetCmd::Ota { url: None }, // tap = baked OTA_URL
                        );
                    }
                    shell.set_ota_status(ota_status_text);
                }

                // === NETWORK flow: scan → pick → password → connect ===
                // A connect can be triggered by two paths this tick (an OPEN
                // network pick, or ✓ on the password) — one shared arm below.
                let mut net_connect = false;
                // Scan trigger (choose-network + rescan): raise the picker and
                // hand the sweep to net_task (#53). The loop keeps rendering —
                // the scanning animation actually animates now — and rows
                // STREAM in below as each channel completes.
                if shell.req.wifi_scan.take() {
                    net_view = 1;
                    shell.set_net_view(net_view);
                    shell.set_net_scanning(true);
                    scan_list.clear();
                    shell.set_wifi_nets(&[]);
                    if !crate::net::net_task::send(crate::net::net_task::NetCmd::Scan) {
                        // Queue full (an OTA in flight): don't leave the
                        // picker spinning on a scan that will never run.
                        shell.set_net_scanning(false);
                    }
                }
                // Streaming scan results: net_task bumps scan_seq after every
                // channel; re-pull the published rows into the picker (top 6,
                // already dedup'd + strength-sorted) and mirror the pick list.
                if net.scan_seq != last_scan_seq {
                    last_scan_seq = net.scan_seq;
                    scan_list.clear();
                    let mut top: heapless::Vec<(heapless::String<32>, i8, bool), 6> =
                        heapless::Vec::new();
                    crate::net::net_task::with_scan_rows(|rows| {
                        for r in rows.iter().take(6) {
                            let _ = scan_list.push((r.0.clone(), r.2));
                            let _ = top.push(r.clone());
                        }
                    });
                    shell.set_wifi_nets(&top);
                    shell.set_net_scanning(net.scanning);
                }
                // Picker row tapped: secured → password keyboard; OPEN network
                // → connect right away with an empty password.
                if let Some(i) = shell.req.wifi_pick.take() {
                    if let Some((ssid, secured)) = scan_list.get(i as usize) {
                        pending_ssid.clear();
                        let _ = pending_ssid.push_str(ssid.as_str());
                        kb_buf.clear();
                        kb_plain = false;
                        if *secured {
                            net_edit = NetEdit::Pass;
                            net_view = 2;
                            shell.set_net_view(net_view);
                            push_kb(&shell, false, pending_ssid.as_str(), "", kb_plain);
                        } else {
                            net_connect = true;
                        }
                    }
                }
                // Hidden network: keyboard for the SSID first, then password.
                if shell.req.wifi_manual.take() {
                    net_edit = NetEdit::Ssid;
                    pending_ssid.clear();
                    kb_buf.clear();
                    kb_plain = false;
                    net_view = 2;
                    shell.set_net_view(net_view);
                    push_kb(&shell, true, "", "", kb_plain);
                }
                // Keyboard: Rust owns the buffer; keys are one char each.
                if let Some(k) = shell.req.kb_key.take() {
                    let cap = if net_edit == NetEdit::Ssid { 32 } else { 64 };
                    if kb_buf.len() + k.len() <= cap {
                        let _ = kb_buf.push_str(k.as_str());
                    }
                    push_kb(
                        &shell,
                        net_edit == NetEdit::Ssid,
                        pending_ssid.as_str(),
                        kb_buf.as_str(),
                        kb_plain,
                    );
                }
                // Backspace: one delete on the DOWN edge, then auto-repeat
                // while held (the touch-held 16ms tick paces the repeats).
                if shell.req.kb_bksp_down.take() {
                    kb_bksp_held = true;
                    let _ = kb_buf.pop();
                    push_kb(
                        &shell,
                        net_edit == NetEdit::Ssid,
                        pending_ssid.as_str(),
                        kb_buf.as_str(),
                        kb_plain,
                    );
                    kb_bksp_next = Instant::now() + Duration::from_millis(420);
                }
                if shell.req.kb_bksp_up.take() {
                    kb_bksp_held = false;
                }
                if kb_bksp_held && net_view == 2 && Instant::now() >= kb_bksp_next {
                    if kb_buf.pop().is_some() {
                        push_kb(
                            &shell,
                            net_edit == NetEdit::Ssid,
                            pending_ssid.as_str(),
                            kb_buf.as_str(),
                            kb_plain,
                        );
                    }
                    kb_bksp_next = Instant::now() + Duration::from_millis(110);
                }
                // Show/hide-password eye.
                if shell.req.kb_eye.take() {
                    kb_plain = !kb_plain;
                    push_kb(
                        &shell,
                        net_edit == NetEdit::Ssid,
                        pending_ssid.as_str(),
                        kb_buf.as_str(),
                        kb_plain,
                    );
                }
                // ✓ commit: SSID stage advances to the password; password
                // stage connects (empty password allowed — open networks).
                if shell.req.kb_done.take() {
                    match net_edit {
                        NetEdit::Ssid => {
                            if !kb_buf.is_empty() {
                                pending_ssid.clear();
                                let _ = pending_ssid.push_str(kb_buf.as_str());
                                net_edit = NetEdit::Pass;
                                kb_buf.clear();
                                kb_plain = false;
                                push_kb(&shell, false, pending_ssid.as_str(), "", kb_plain);
                            }
                        }
                        NetEdit::Pass => net_connect = true,
                        NetEdit::None => {}
                    }
                }
                // Back out of a sub-view (chevron / right-swipe): keyboard →
                // picker (buffer dropped), picker → hub pages. Rust owns the
                // transitions so keyboard state can never fork from the view.
                if shell.req.net_back.take() {
                    if net_view == 2 {
                        net_edit = NetEdit::None;
                        kb_buf.clear();
                        kb_bksp_held = false;
                        net_view = 1;
                    } else if net_view == 1 {
                        net_view = 0;
                    }
                    shell.set_net_view(net_view);
                }
                // Shared connect arm: persist here (main owns flash config),
                // then hand the creds to net_task — SetCreds reconnects with
                // them, resets the backoff (user action), and re-arms the
                // NTP/MQTT/weather burst; the feedback arm above maps the
                // published phase back onto net_status.
                if net_connect {
                    watch_cfg.ssid.clear();
                    let _ = watch_cfg.ssid.push_str(pending_ssid.as_str());
                    watch_cfg.pass.clear();
                    let _ = watch_cfg.pass.push_str(kb_buf.as_str());
                    // Connecting IS wifi intent — clear a forced-off bit in
                    // the same (single) save as the creds.
                    watch_cfg.wifi_off = false;
                    // Deferred (#75): mark dirty; the flush block writes once at a
                    // quiet moment. An inline erase here can hang the watch.
                    cfg_dirty_at = Some(now);
                    wifi_has_creds = !watch_cfg.ssid.is_empty();
                    let sent = crate::net::net_task::send(crate::net::net_task::NetCmd::SetCreds {
                        ssid: watch_cfg.ssid.clone(),
                        pass: watch_cfg.pass.clone(),
                    });
                    // Queue full (review F4): the creds ARE persisted (they
                    // apply at next boot), but no reconnect will fire — show
                    // "failed" honestly instead of spinning "Connecting…";
                    // the user re-taps ✓ once the download window passes.
                    settings_connect_pending = sent;
                    net_status = if sent { 1 } else { 3 };
                    net_edit = NetEdit::None;
                    kb_buf.clear();
                    net_view = 0;
                    shell.set_net_view(net_view);
                    shell.set_net_status(net_status);
                    shell.set_net_current(watch_cfg.ssid.as_str());
                    shell.set_wifi_intent(true);
                }

                if shell.req.wifi_toggle.take() {
                    wifi_toggle_request = true;
                }
                if shell.req.ble_toggle.take() {
                    ble_toggle_request = true;
                }
                if shell.req.mesh_toggle.take() {
                    mesh_enabled = !mesh_enabled;
                    if mesh_enabled {
                        // Bring up the STA radio for ESP-NOW if it isn't already
                        // (creds NOT required — set_config starts the PHY without
                        // connecting). PHY-only hold on net_task (#53); the mesh
                        // block gates on the published radio_started, and the
                        // channel pin rides the mesh_pin_ok verdict.
                        let _ = crate::net::net_task::send(crate::net::net_task::NetCmd::Raise(
                            crate::net::net_task::Hold::Phy,
                        ));
                    } else {
                        // Reflect "off" in the MESH chrome dot immediately; peers
                        // repopulate from HELLOs once re-enabled. Radio stays up
                        // (tick-level pause, not a teardown).
                        last_mesh_peers = 0;
                    }
                    println!("[MESH] toggled -> {}", if mesh_enabled { "ON" } else { "OFF" });
                    // Persist the toggle (#46 mesh bit, config v5) — edge-
                    // triggered like the BLE/theme saves.
                    if watch_cfg.mesh_on != mesh_enabled {
                        watch_cfg.mesh_on = mesh_enabled;
                        // Deferred (#75): mark dirty; the flush block writes once at a
                        // quiet moment. An inline erase here can hang the watch.
                        cfg_dirty_at = Some(now);
                    }
                    shell.set_mesh_enabled(mesh_enabled);
                }
                if shell.req.cpu_cycle.take() {
                    // Mirror the old WatchFace::cycle_cpu ladder: 80 -> 160 -> 240.
                    cpu_mhz = match cpu_mhz {
                        80 => 160,
                        160 => 240,
                        _ => 80,
                    };
                    let actual = crate::peripherals::cpu_clock::set_cpu_mhz(cpu_mhz);
                    cpu_mhz = actual;
                    power_stats.cpu_mhz = actual;
                    shell.set_cpu_mhz(actual);
                }
                if shell.req.gyro_toggle.take() {
                    gyro_enabled = !gyro_enabled;
                    shell.set_gyro(gyro_enabled);
                    println!("Gyro: {}", if gyro_enabled { "ON" } else { "OFF" });
                }
                if shell.req.reboot.take() {
                    println!("REBOOT requested");
                    // Old semantics: with WiFi up + a baked OTA_URL, stage an
                    // update first, reboot either way. The download runs in
                    // net_task now, so the UI stays live meanwhile: queue the
                    // job and arm a bounded reboot — Staged reboots via the
                    // OTA render arm, a Failed edge or this 6-min deadline
                    // (the download's own hard cap) reboots without it.
                    if wifi_connected
                        && crate::net::ota_http::URL_SET
                        && !net.ota.active()
                        && crate::net::net_task::send(crate::net::net_task::NetCmd::Ota {
                            url: None,
                        })
                    {
                        reboot_deadline = Some(now + Duration::from_secs(360));
                        shell.set_toast("Updating, then rebooting\u{2026}");
                        toast_until = now + Duration::from_secs(30);
                        toast_active = true;
                    } else {
                        esp_hal::system::software_reset();
                    }
                }
                if shell.req.power_shutdown.take() {
                    // Power menu SHUTDOWN (#48): AXP2101 poweroff (0x10 bit0,
                    // the vendor PowerOff() write). On battery the rails cut
                    // within the PMIC's shutdown sequence and this loop simply
                    // stops; PWRON (128ms ONLEVEL) powers back on. On USB the
                    // PMIC re-powers immediately = a cold reboot (the menu
                    // caption says so while VBUS is live).
                    println!("[PKEY] SHUTDOWN -> AXP2101 poweroff (0x10 bit0)");
                    if power.shutdown().is_err() {
                        // Still alive = the write never landed. Keep the menu
                        // up rather than pretending; the log tells the story.
                        println!("[PKEY] poweroff write FAILED (I2C)");
                    }
                }
                // === App switcher (#31) ===
                // Bottom-edge HOLD (handle_touch) or the status-cluster chip
                // queued an open: build the session cards, then raise the
                // overlay. Cards must exist BEFORE the scrim shows.
                if shell.req.open_switcher.take() {
                    push_switcher(&mut shell, &sessions);
                    shell.set_switcher_open(true);
                }
                // Kill-swipe on a card: drop the session (next open runs
                // setup()) and rebuild in place — the overlay stays up (empty
                // state if that was the last one) so a second kill doesn't
                // need a fresh hold gesture.
                if let Some(idx) = shell.req.switcher_kill.take() {
                    if let Some(state) = crate::apps::registry::launch_state(idx as usize) {
                        sessions.kill(state);
                        println!("[SESSION] killed {state:?} ({} left)", sessions.len());
                    }
                    push_switcher(&mut shell, &sessions);
                    shell.set_suspended_count(sessions.len() as i32);
                }

                // === Notification shade (#32) ===
                // Top-edge swipe-down (handle_touch) or the unread chip:
                // build the cards, zero the badge, then raise the overlay.
                if shell.req.open_shade.take() {
                    push_shade(&shell);
                    crate::notify::mark_read();
                    shell.set_notif_unread(0);
                    shell.set_shade_open(true);
                }
                // Per-card dismiss (X tap or Left-swipe on the card) and
                // CLEAR ALL: ring edits + in-place rebuild, shade stays up.
                if let Some(slot) = shell.req.notif_dismiss.take() {
                    crate::notify::dismiss(slot as usize);
                    push_shade(&shell);
                }
                if shell.req.notif_clear.take() {
                    crate::notify::clear();
                    push_shade(&shell);
                    shell.set_notif_unread(0);
                }
                // Arrival: badge always. A FRESH arrival (not one that aged
                // out while a game held the panel) toasts while the screen is
                // on — never wakes it (battery: screen-off arrivals are badge-
                // only) — or lands straight into an open shade.
                if let Some((title, posted_ms)) = crate::notify::take_arrival() {
                    shell.set_notif_unread(crate::notify::unread() as i32);
                    if shell.shade_open() {
                        push_shade(&shell);
                        crate::notify::mark_read();
                        shell.set_notif_unread(0);
                    } else if screen_state >= 2
                        && !toast_active
                        && Instant::now().as_millis().saturating_sub(posted_ms) < 2_000
                    {
                        shell.set_toast(title.as_str());
                        toast_active = true;
                        toast_until = now + Duration::from_secs(3);
                    }
                    // Auto read-aloud (#read-aloud) — deliberately narrow. Every
                    // gate maps to a specific failure it prevents: speaking parks
                    // the main loop for SECONDS, so an arrival mid-game would
                    // freeze a framebuffer app (hence Watchface-only); the watch
                    // is worn in rooms with people (hence screen-on, i.e. the
                    // user is already looking at it); and stacking a second
                    // utterance over a live one would just fight for the queue.
                    #[cfg(feature = "tts")]
                    if watch_cfg.speak == SpeakMode::Auto
                        && screen_state >= 2
                        && app_state == AppState::Watchface
                        && !muted
                        && !audio_out::busy()
                    {
                        speak_request = true;
                    }
                }

                if let Some(target) = shell.req.launch.take() {
                    // Launch tap-click: covered by the hoisted every-touch tick
                    // (#49) — the old per-control click here would double up.
                    shell.set_launcher_open(false);
                    // A switcher-card resume arrives on this same cell; close
                    // the overlays before dispatching (idempotent — the Slint
                    // side already hard-cut the switcher on the tap).
                    shell.set_switcher_open(false);
                    shell.set_shade_open(false);
                    if target == AppState::Wled {
                        // WLED is a Slint overlay, not a framebuffer app: it renders
                        // through the resident scene, so raise the overlay in place
                        // — no scene suspend, no ~51KB fb alloc. Taps broadcast
                        // WiZmote frames (drained above); back/Right-swipe closes.
                        shell.set_wled_status("");
                        shell.set_wled_open(true);
                        app_state = AppState::Wled;
                    } else if target == AppState::Hunt {
                        // Hunt is a Slint overlay too — raise it and seed the target
                        // from the current roster (the per-tick feed does the rest).
                        // No scene suspend, no fb.
                        if hunt_state.target().is_none() {
                            let now_ms = now.as_millis();
                            let mut ids = [0u8; MESH_MAX_ROWS];
                            let n_ids = mesh.live_peer_ids(now_ms, &mut ids);
                            hunt_state.cycle_target(&ids[..n_ids], now_ms);
                        }
                        shell.set_hunt_open(true);
                        app_state = AppState::Hunt;
                    } else if target == AppState::Energy {
                        // #58: home energy is LIVE off the shared HA MQTT session.
                        // Raise the overlay + hold WiFi; the session (climate_task)
                        // comes up for either screen; the live feed is in the shared
                        // session block below.
                        shell.set_energy_open(true);
                        energy_active = true; // Hold::Session rises on the edge above
                        app_state = AppState::Energy;
                    } else if target == AppState::Climate {
                        // #58: raise the Climate overlay + hold WiFi up. The MQTT
                        // session task starts once WiFi associates (Climate tick
                        // below); released on session return (both Ok + Err).
                        shell.set_climate_open(true);
                        climate_active = true; // Hold::Session rises on the edge above
                        app_state = AppState::Climate;
                        // Force the first climate push (change-gate reset) + log the
                        // heap the screen opens against, so the #60 OOM fix is
                        // measurable on-glass (free should hold steady now, not
                        // bleed down per tick).
                        prev_climate_fp = None;
                        println!("[HEAP] climate open: free={}", esp_alloc::HEAP.free());
                    } else if target == AppState::Lights {
                        // Lights (#39): raise the overlay + hold WiFi, riding the
                        // shared HA MQTT session exactly like Climate. The session
                        // task starts once WiFi associates (session block above);
                        // released on close (both chevron-cell + right-swipe paths).
                        lights_pending = None;
                        lights_noreply_until = None;
                        lights_opened_at = Some(Instant::now()); // [LAT] open->first-state
                        shell.set_lights_open(true);
                        lights_active = true; // Hold::Session rises on the edge above
                        app_state = AppState::Lights;
                    } else if target == AppState::Ping {
                        // Ping (#35): a Slint overlay riding the always-on
                        // ESP-NOW radio — no WiFi hold, no framebuffer. Reset
                        // the presentation machine so a stale delivered/no-reply
                        // caption doesn't linger; an in-flight seq and the
                        // cooldown carry over (etiquette survives a quick
                        // close/reopen). The per-tick block above re-pushes the
                        // fresh snapshot (ping_prev_push = None forces it).
                        if ping_outstanding.is_none() && ping_state != 0 {
                            ping_state = 0;
                            ping_result.clear();
                        }
                        ping_prev_push = None;
                        shell.set_ping_open(true);
                        app_state = AppState::Ping;
                    } else if target == AppState::Voice {
                        // Voice-to-text (#42): a Slint overlay (scene-resident, no
                        // fb). Open in idle; the PTT flow below drives capture +
                        // transcript. Reset to idle each open so a prior transcript
                        // or error doesn't linger.
                        shell.set_voice_state(0);
                        shell.set_voice_transcript("");
                        shell.set_voice_error("");
                        shell.set_voice_open(true);
                        // STT is WiFi-dependent (HTTP to the LAN bridge).
                        // Hold::Voice rises on the app_state==Voice edge above,
                        // and drops the same way when the screen closes
                        // (right-swipe → reconcile → app_state=Watchface) —
                        // deterministic release, never strands the mesh; a
                        // manual WiFi-on (Hold::User) is untouched by design.
                        app_state = AppState::Voice;
                    } else if target == AppState::Sound {
                        // Sound-level meter (#28): a Slint overlay (scene-resident,
                        // no fb). NO WiFi — rms_dbfs is local. The per-tick meter
                        // block below arms the ADC + METER gate on entry and drains
                        // MIC_CH → rms_dbfs → dBFS/peak; tears them down on close.
                        shell.set_mic_open(true);
                        app_state = AppState::Sound;
                    } else if target == AppState::Theme {
                        // Theme picker: a Slint overlay (scene-resident, no fb, no
                        // WiFi). Raise it; taps set Theme.scheme for instant preview
                        // and emit theme-changed (drained above → flash persist).
                        // Right-swipe closes via the OVERLAYS table (Flag close).
                        shell.set_theme_open(true);
                        app_state = AppState::Theme;
                    } else if target == AppState::Settings {
                        // Settings hub (v0.9.0, #49): a Slint overlay — the
                        // scene-resident successor of the fb Settings app.
                        // Push fresh state (the guards elsewhere only push on
                        // change), reset the NETWORK sub-view, and raise it.
                        net_view = 0;
                        net_edit = NetEdit::None;
                        kb_buf.clear();
                        kb_plain = false;
                        kb_bksp_held = false;
                        if net_status != 1 {
                            // Not mid-connect: reflect the live association.
                            net_status = if wifi_connected { 2 } else { 0 };
                        }
                        shell.set_net_view(net_view);
                        shell.set_net_status(net_status);
                        shell.set_net_current(watch_cfg.ssid.as_str());
                        shell.set_touch_sound(touch_sound);
                        shell.set_mesh_enabled(mesh_enabled);
                        shell.set_wifi_intent(!watch_cfg.wifi_off);
                        shell.set_ota_status(ota_status_text);
                        // #59 volume + button map for the SOUND/BUTTONS pages.
                        shell.set_volume(volume, muted);
                        shell.set_button_actions(
                            boot_short.label(),
                            boot_long.label(),
                            pwron_short.label(),
                            pwron_long.label(),
                        );
                        shell.set_settings_open(true);
                        app_state = AppState::Settings;
                    } else {
                        // Games paint through the framebuffer, now HALF-RES (~51KB,
                        // see framebuffer.rs). It fits alongside the resident Slint
                        // scene at the 240KB heap with ~80KB to spare, so a launch no
                        // longer *needs* the scene gone. We still close the launcher
                        // and drop the scene here (kept as heap headroom; post-ship
                        // task #37 may remove it), then allocate the fb fallibly. The
                        // failure path (now practically impossible) recreates the
                        // scene and stays put with a toast.
                        if toast_active {
                            shell.set_toast("");
                            toast_active = false;
                        }
                        log_heap("pre-fb pre-drop");
                        shell.suspend_scene();
                        log_heap("pre-fb post-drop");
                        match Framebuffer::try_new() {
                            Some(f) => {
                                fb = Some(f);
                                log_heap("app enter");
                                // Session manager (#31): a suspended app RESUMES —
                                // its state struct was kept, so setup() (the reset)
                                // is exactly what a resume must skip. Fresh
                                // launches (never suspended, or killed from the
                                // switcher) run the SAME per-app setup the old
                                // launcher arm did (without it the games boot
                                // into garbage).
                                let resumed = sessions.take_resume(target);
                                if !resumed {
                                    match target {
                                        AppState::Snake => snake_game.setup(),
                                        AppState::WorldSnake => world_snake.setup(),
                                        AppState::Game2048 => game_2048.setup(),
                                        AppState::Tetris => tetris_game.setup(),
                                        AppState::Flappy => flappy_game.setup(),
                                        AppState::Maze => maze_game.setup(),
                                        AppState::Settings => {}
                                        _ => {}
                                    }
                                }
                                // Entry frame: event-driven apps (2048 — dirty
                                // only on a move) would sit on a black fb until
                                // their first input. True for a fresh 2048 (the
                                // old inline render) and for EVERY resume: the
                                // kept state must show NOW, not after a touch.
                                let entry: Option<&dyn App> = match target {
                                    AppState::Game2048 => Some(&game_2048),
                                    _ if resumed => match target {
                                        AppState::Snake => Some(&snake_game),
                                        AppState::WorldSnake => Some(&world_snake),
                                        AppState::Tetris => Some(&tetris_game),
                                        AppState::Flappy => Some(&flappy_game),
                                        AppState::Maze => Some(&maze_game),
                                        _ => None,
                                    },
                                    _ => None,
                                };
                                if let Some(app) = entry {
                                    let fb = fb.as_mut().unwrap();
                                    app.render(fb);
                                    fb.flush(&mut display);
                                }
                                if resumed {
                                    println!("[SESSION] resumed {target:?}");
                                }
                                app_state = target;
                            }
                            None => {
                                // Even with the scene freed the fb won't fit —
                                // recreate the scene, stay in the shell, and toast.
                                // The reset guards force a repopulate over the next
                                // ticks.
                                shell.resume_scene();
                                prev_page = -1;
                                prev_radios = (!wifi_connected, ble_on, last_mesh_peers);
                                shell.set_battery(batt_pct, batt_mv, charging);
                                if let Some(dt) = last_dt.as_ref() {
                                    let _ = shell.set_time(dt);
                                }
                                shell.set_toast("RAM busy \u{2014} try again");
                                toast_active = true;
                                toast_until = now + Duration::from_secs(3);
                            }
                        }
                    }
                }

                // (#59) BOOT is now a mapped button: the short/long dispatch
                // above owns launcher-toggle (the default BOOT hold = Launcher),
                // so the old raw is_low() toggle here is gone. Modal-dismiss +
                // launcher-toggle live in the ButtonAction::Launcher arm.

                // Auto-clear the RAM-busy toast once its window elapses (guarded
                // so we only push the empty string a single time).
                if toast_active && now >= toast_until {
                    shell.set_toast("");
                    toast_active = false;
                }

                // Repaint if the scene is dirty (full-frame, line-streamed).
                // Skip when a launch just switched us into an app this iteration:
                // that app already painted its first frame (e.g. Game2048) and the
                // trailing shell repaint would clobber it. Every scene-resident
                // overlay must be listed here — Theme included: it paints through
                // the scene (no framebuffer first-frame), so omitting it left the
                // picker unpainted on the open-tick and it only appeared once some
                // later event forced a render (the "Theme slow to load" bug).
                if matches!(
                    app_state,
                    AppState::Watchface
                        | AppState::Launcher
                        | AppState::Wled
                        | AppState::Hunt
                        | AppState::Energy
                        | AppState::Climate
                        | AppState::Lights
                        | AppState::Ping
                        | AppState::Voice
                        | AppState::Sound
                        | AppState::Theme
                        | AppState::Settings
                ) {
                    if screen_state >= 2 {
                        // Time the shell render — the responsiveness metric the
                        // `perf` command exposes (catches theme-slow /
                        // launcher-scroll style regressions).
                        #[cfg(feature = "debug-console")]
                        let _t0 = Instant::now();
                        shell.render(&mut display);
                        #[cfg(feature = "debug-console")]
                        debug_console::record_frame((Instant::now() - _t0).as_micros() as u32);
                    } else if screen_state == 1 {
                        // AOD: repaint only when the minute changes so the dim
                        // scene isn't driven every wake. last_dt is refreshed at
                        // state >= 1 above; set_time (shell arm) dirtied the scene.
                        if let Some(dt) = last_dt.as_ref() {
                            if dt.minutes != aod_last_minute {
                                aod_last_minute = dt.minutes;
                                #[cfg(feature = "debug-console")]
                                let _t0 = Instant::now();
                                shell.render(&mut display);
                                #[cfg(feature = "debug-console")]
                                debug_console::record_frame(
                                    (Instant::now() - _t0).as_micros() as u32,
                                );
                            }
                        }
                    }
                }
            }

            // === Framebuffer apps (games) ===
            // ONE generic arm for every app whose registry `kind` is Framebuffer.
            // The per-game arms collapsed into `run_fb_app` (update -> drain sfx ->
            // render+flush on the app's own `dirty`/`min_flush_ms`). Peripheral
            // service that can't live behind the trait stays keyed on the state
            // (Flappy's INT-touch); WorldSnake's ESP-NOW feed already runs in the
            // per-tick net section above. (Settings left this arm in v0.9.0: the
            // hub is a scene-resident overlay; its cred/OTA service moved to the
            // Slint-arm drains above.)
            s if crate::apps::registry::is_framebuffer(s) => {
                let Some(fb_ref) = fb.as_mut() else {
                    app_state = AppState::Watchface;
                    continue;
                };

                // Per-app input shaping: Flappy reads the touch INT for a reliable
                // held-to-flap signal; games otherwise ignore touch coords.
                let touch = match s {
                    AppState::Flappy => {
                        if touch_int.is_low() {
                            Some(crate::peripherals::touch::TouchPoint {
                                x: 200,
                                y: 250,
                                fingers: 1,
                            })
                        } else {
                            None
                        }
                    }
                    _ => None,
                };
                let input = AppInput {
                    touch,
                    swipe: swipe_event,
                    tap: tap_event,
                    // Live finger-on-glass flag: Some only while the controller
                    // reports a contact this tick (drops to None on the lift tick,
                    // the same tick `tap` fires). Drives pressed-state redraw in
                    // Settings/T9 (touch overhaul); games ignore it.
                    down: touch_point.is_some(),
                    accel,
                    dt_ms: dt_ms.max(1),
                };

                // The one instance-wiring match: state -> concrete app as dyn App.
                let app: &mut dyn App = match s {
                    AppState::Snake => &mut snake_game,
                    AppState::WorldSnake => &mut world_snake,
                    AppState::Game2048 => &mut game_2048,
                    AppState::Tetris => &mut tetris_game,
                    AppState::Flappy => &mut flappy_game,
                    AppState::Maze => &mut maze_game,
                    // is_framebuffer(s) already gated this arm; anything else is a
                    // registry/enum mismatch — bail to the watchface.
                    _ => {
                        app_state = AppState::Watchface;
                        fb = None;
                        continue;
                    }
                };
                let (exit, sfx) =
                    run_fb_app(app, &input, fb_ref, &mut display, now, &mut next_flush);
                // Snake food-eat beep — restored in v0.8.5 (#23): queued onto
                // the shared TX ring (the clock task substitutes the samples;
                // the mic clock never stops). Dead since the mic work made the
                // continuous silent TX the full-duplex clock master.
                if let Some(Sfx::Beep) = sfx {
                    audio_out::play_pcm(beep_pcm);
                    audio_out::service_amp(&mut amp_en, &mut audio_codec);
                }
                // Exit (app-signalled) returns to the launcher. (#59) BOOT no
                // longer force-exits here — it's a mapped button now (default
                // hold = Launcher, handled in the dispatcher above, which
                // suspends the session identically). In-game BOOT tap defaults
                // to Volume+.
                if exit {
                    // Session manager (#31): every fb exit SUSPENDS — the state
                    // struct is a main-loop local and persists, so the next
                    // open resumes mid-game unless the switcher killed it.
                    // (Game-over screens self-reset on tap in-app, so resuming
                    // onto one is fine.) Badge count lands via the
                    // resume_scene re-push in the shell arm.
                    sessions.suspend(s);
                    app_state = AppState::Launcher;
                    fb = None;
                    println!("[HEAP] app exit free: {}", esp_alloc::HEAP.free());
                }
            }

            _ => {
                app_state = AppState::Watchface;
                fb = None;
            }
        }

        // Publish the UI snapshot for the debug-console `state` command (once per
        // awake tick — screen-off ticks `continue` above and keep the last one).
        #[cfg(feature = "debug-console")]
        debug_console::publish_state(debug_console::UiState {
            app: app_state,
            page: shell.page(),
            screen_state,
            wifi: wifi_connected,
            ble: ble_on,
            mesh_peers: last_mesh_peers,
            modal: shell.modal_kind(),
        });

        // Track the arm we ran so the shell arm can detect a return from an app
        // that painted straight to the panel and force a repaint. Use the
        // pre-dispatch snapshot, not the (possibly mutated) current app_state.
        prev_app_state = dispatched;
    }
}

/// Generic per-frame driver for a framebuffer app, dispatched through `&mut dyn
/// App` (object-safe since `App::render` is monomorphized to `Framebuffer`).
/// Runs `update` → drains any queued [`Sfx`] → renders + flushes when the app is
/// `dirty` and its `min_flush_ms` cadence has elapsed.
///
/// Returns `(exit, sfx)`: `exit` is true when the app's own `update` signalled
/// `AppResult::Exit` (the boot-button exit stays in the caller's arm — it needs
/// `.await`); `sfx` is any one-shot effect for the caller to play on the shared
/// I2S path. This one body replaces the per-game render/flush arms.
fn run_fb_app(
    app: &mut dyn App,
    input: &AppInput,
    fb: &mut Framebuffer,
    display: &mut Co5300Display,
    now: Instant,
    next_flush: &mut Instant,
) -> (bool, Option<Sfx>) {
    let result = app.update(input);
    let sfx = app.take_sfx();
    if let AppResult::Exit = result {
        return (true, sfx);
    }
    if app.dirty() && now >= *next_flush {
        app.render(fb);
        fb.flush(display);
        *next_flush = now + Duration::from_millis(app.min_flush_ms() as u64);
    }
    (false, sfx)
}

/// Rebuild the app-switcher cards (#31) from the suspension list: registry
/// indices, most recently suspended first. The overlay shows the first 4;
/// the full count drives its "+N more" line.
fn push_switcher(shell: &mut ShellUi, sessions: &crate::apps::session::Sessions) {
    let mut rows: heapless::Vec<i32, 8> = heapless::Vec::new();
    for st in sessions.iter() {
        if let Some(pos) = crate::apps::registry::REGISTRY
            .iter()
            .position(|d| d.state == st)
        {
            if rows.push(pos as i32).is_err() {
                break;
            }
        }
    }
    shell.set_switcher_cards(&rows, sessions.len());
}

/// Rebuild the notification-shade cards (#32) from the ring (newest first;
/// the overlay shows 4, its footer counts the rest). Snapshot-then-push keeps
/// the critical section tiny — no Slint work under the ring lock.
fn push_shade(shell: &ShellUi) {
    let mut buf: heapless::Vec<crate::notify::Notification, { crate::notify::CAP }> =
        heapless::Vec::new();
    crate::notify::snapshot(&mut buf);
    shell.set_shade_cards(&buf);
}

/// Push the Settings-hub keyboard display state (v0.9.0 NETWORK flow). Rust
/// owns the text buffer; this derives what the glass shows: the stage title,
/// the context line (SSID being joined / "hidden network"), and the display
/// text — MASKED for passwords (unless the eye is open) and TAIL-WINDOWED to
/// the last 24 chars so the caret end (where typing happens) is always
/// visible. The keyboard only emits ASCII, so per-char masking is safe.
fn push_kb(shell: &ShellUi, edit_ssid: bool, ssid: &str, buf: &str, plain: bool) {
    let (title, context) = if edit_ssid {
        ("NETWORK NAME", "hidden network")
    } else {
        ("PASSWORD", ssid)
    };
    let mut disp: heapless::String<80> = heapless::String::new();
    let n = buf.chars().count();
    if n > 24 {
        let _ = disp.push('\u{2026}');
    }
    for c in buf.chars().skip(n.saturating_sub(24)) {
        let _ = disp.push(if edit_ssid || plain { c } else { '*' });
    }
    shell.set_kb(title, context, disp.as_str(), plain);
}

/// Map a WLED page action id (see ui/slint/wled.slint) to a WiZmote button.
/// 0 On · 1 Off · 2-5 Preset 1-4 · 6 Dim+ · 7 Dim- · 8 Night.
fn wled_button(act: i32) -> Option<wled_wizmote::WledButton> {
    use wled_wizmote::WledButton as B;
    Some(match act {
        0 => B::On,
        1 => B::Off,
        2 => B::Preset(1),
        3 => B::Preset(2),
        4 => B::Preset(3),
        5 => B::Preset(4),
        6 => B::BrightUp,
        7 => B::BrightDown,
        8 => B::Night,
        _ => return None,
    })
}

/// Short confirmation shown under the WLED tiles after a broadcast.
fn wled_status(act: i32) -> &'static str {
    match act {
        0 => "\u{2192} ON",
        1 => "\u{2192} OFF",
        2 => "\u{2192} Preset 1",
        3 => "\u{2192} Preset 2",
        4 => "\u{2192} Preset 3",
        5 => "\u{2192} Preset 4",
        6 => "\u{2192} Dim +",
        7 => "\u{2192} Dim \u{2212}",
        8 => "\u{2192} Night",
        _ => "",
    }
}
