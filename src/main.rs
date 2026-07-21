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
mod net;
mod peripherals;
mod ui;
mod apps;

use core::cell::RefCell;

use embassy_executor::Spawner;
use embassy_futures::join::join;
use embassy_futures::select::select3;
use embassy_time::{Duration, Instant, Timer};
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::RgbColor;
use embedded_hal_bus::i2c::RefCellDevice;
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    delay::Delay,
    dma::{DmaDescriptor, DmaRxBuf, DmaTxBuf},
    dma_buffers,
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
use crate::apps::settings::SettingsApp;
use crate::apps::snake::SnakeGame;
use crate::apps::tetris::TetrisGame;
use crate::apps::world_snake::WorldSnakeApp;
use crate::apps::{App, AppInput, AppResult, AppState};
use crate::drivers::co5300::Co5300Display;
use crate::net::familiar::FamUi;
use crate::net::smol_mesh::{MeshEvent, MESH_MAX_ROWS, PeerView, SmolMesh};
use crate::net::voice_stt;
use crate::drivers::framebuffer::Framebuffer;
use crate::drivers::qspi_bus::QspiBus;
use crate::peripherals::audio::{fill_beep_buffer, Es8311};
use crate::peripherals::die_temp::DieTemp;
use crate::peripherals::imu::Qmi8658Imu;
use crate::peripherals::mic_capture;
use crate::peripherals::power::Axp2101Power;
use crate::peripherals::power_stats::{DisplayState, PowerStats, WifiMode};
use crate::peripherals::rtc::{DateTime, Pcf85063aRtc};
use crate::peripherals::touch::{Ft3168Touch, SwipeDirection};
use crate::ui::slint_shell::{self, ShellUi};

extern crate alloc;

esp_bootloader_esp_idf::esp_app_desc!();

#[embassy_executor::task]
async fn net_task(
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
    cmd_rx: crate::net::mqtt_climate::ClimateCmdReceiver,
    open: &'static crate::net::mqtt_climate::CloseSignal,
    close: &'static crate::net::mqtt_climate::CloseSignal,
    done: &'static crate::net::mqtt_climate::CloseSignal,
) {
    loop {
        open.wait().await;
        // One session feeds BOTH the Climate + Energy screens (shared CONNECT).
        if let Err(e) =
            crate::net::mqtt_climate::run_climate_session(stack, state, energy, cmd_rx, close).await
        {
            println!("[CLIM] session ended: {e}");
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
fn page_scr_name(page: i32) -> &'static str {
    match page {
        1 => "SENSORS",
        2 => "SYSTEM",
        3 => "POWER",
        4 => "MESH",
        _ => "CLOCK",
    }
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

/// One-shot SNTP query; sets the RTC and returns the Unix time.
async fn ntp_sync(
    stack: embassy_net::Stack<'static>,
    rtc: &mut crate::peripherals::rtc::Pcf85063aRtc<impl embedded_hal::i2c::I2c>,
) -> Result<u32, ()> {
    use embassy_net::udp::{PacketMetadata, UdpSocket};

    let mut rx_meta = [PacketMetadata::EMPTY; 1];
    let mut rx_buf = [0u8; 256];
    let mut tx_meta = [PacketMetadata::EMPTY; 1];
    let mut tx_buf = [0u8; 256];

    let mut socket = UdpSocket::new(stack, &mut rx_meta, &mut rx_buf, &mut tx_meta, &mut tx_buf);
    socket.bind(12345).map_err(|_| ())?;

    let mut ntp_request = [0u8; 48];
    ntp_request[0] = 0x1B; // LI=0, VN=3, Mode=3 (client)

    // time.google.com anycast (no DNS needed)
    let ntp_addr = embassy_net::Ipv4Address::new(216, 239, 35, 0);
    socket
        .send_to(&ntp_request, (ntp_addr, 123))
        .await
        .map_err(|_| ())?;

    let mut response = [0u8; 48];
    match embassy_time::with_timeout(Duration::from_secs(5), socket.recv_from(&mut response)).await
    {
        Ok(Ok((len, _addr))) if len >= 48 => {
            let ntp_secs =
                u32::from_be_bytes([response[40], response[41], response[42], response[43]]);
            let unix_secs = ntp_secs.wrapping_sub(2_208_988_800);
            let (h, m, s) = set_rtc_from_unix(rtc, unix_secs);
            println!("[NTP] {h:02}:{m:02}:{s:02} (US Pacific), unix={unix_secs}");
            Ok(unix_secs)
        }
        _ => Err(()),
    }
}

#[allow(clippy::too_many_arguments)]
fn update_power_stats(
    stats: &mut PowerStats,
    screen_state: u8,
    imu_on: bool,
    wifi_connected: bool,
    wifi_on_request: bool,
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
    stats.wifi = Some(if !wifi_on_request && !wifi_connected {
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
    // proven). 214KB still leaves ~54KB spare above the 51KB fb (#35 intact); the
    // reclaimed pool + reviewed mic buffers are untouched.
    esp_alloc::heap_allocator!(size: 214 * 1024);
    // ROM-reclaimed region (dram2_seg, ~64KB, ~100% free at boot). Second pool so
    // nothing goes to waste; it sits ABOVE the stack ceiling and is independent of
    // _bss_end, so its size has zero effect on the stack. Kept at 56KB.
    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 56 * 1024);

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
        const STACK_FLOOR: usize = 46 * 1024;
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
    // amp produces loud white noise. Raised only for the duration of a beep.
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
    // Only when we actually beep: unmute codec -> raise amp -> write DMA ->
    // lower amp -> mute
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

    // === I2S TX for beep playback (16kHz stereo 16-bit) ===
    // C6 pins: MCLK=GPIO19, BCLK=GPIO20, LRCK/WS=GPIO22, DAC data=GPIO21.
    // DMA_CH1 — the display QSPI owns DMA_CH0.
    println!("[AUDIO] Init I2S...");
    let i2s_config = I2sConfig::default()
        .with_sample_rate(Rate::from_hz(16000))
        .with_data_format(DataFormat::Data16Channel16);
    let i2s_periph = I2s::new(peripherals.I2S0, peripherals.DMA_CH1, i2s_config)
        .expect("I2S failed")
        .with_mclk(peripherals.GPIO19);
    static I2S_TX_DESC: StaticCell<[DmaDescriptor; 8]> = StaticCell::new();
    let mut i2s_tx = i2s_periph
        .i2s_tx
        .with_bclk(peripherals.GPIO20)
        .with_ws(peripherals.GPIO22)
        .with_dout(peripherals.GPIO21)
        .build(I2S_TX_DESC.init([DmaDescriptor::EMPTY; 8]));
    // === I2S RX for mic capture — the SINGLE shared owner (#42 voice + #28 meter) ===
    // `i2s_periph.i2s_rx` is still available (partial move — tx took i2s_tx).
    // BCLK/WS/MCLK are configured once on the TX side above (full-duplex shares
    // the peripheral clock); RX only needs its data-in pin (GPIO23) + its own DMA
    // descriptors. Stays Blocking: mic_capture_task drives it via read_dma_circular
    // + poll. This is the ONLY place I2S0/DMA_CH1 is claimed — both the voice PTT
    // stream and the SoundLevel meter subscribe to MIC_CH, never re-owning I2S.
    static I2S_RX_DESC: StaticCell<[DmaDescriptor; 8]> = StaticCell::new();
    let i2s_rx = i2s_periph
        .i2s_rx
        .with_din(peripherals.GPIO23)
        .build(I2S_RX_DESC.init([DmaDescriptor::EMPTY; 8]));
    // Mic PCM channel (capture task → consumers) + the DMA capture ring.
    // Channel::new() is const → a plain static; the 8 KB ring needs a StaticCell.
    static MIC_CH: mic_capture::MicChannel = mic_capture::MicChannel::new();
    static MIC_RING: StaticCell<[u8; mic_capture::MIC_RING_LEN]> = StaticCell::new();
    let mic_ring = MIC_RING.init([0u8; mic_capture::MIC_RING_LEN]);
    _spawner.spawn(
        mic_capture::mic_capture_task(i2s_rx, mic_ring, MIC_CH.sender())
            .expect("mic_capture_task token"),
    );
    println!("[AUDIO] I2S RX (mic) ready on GPIO23");

    // Pre-generate beep sound (800Hz, 50ms, stereo 16-bit @ 16kHz = 3200 bytes)
    static BEEP_BUF: StaticCell<[u8; 4000]> = StaticCell::new();
    let beep_storage = BEEP_BUF.init([0u8; 4000]);
    let beep_len = fill_beep_buffer(beep_storage, 800, 16000, 50);
    let beep_buf: &'static [u8] = &beep_storage[..beep_len];
    println!("[AUDIO] I2S OK ({} bytes beep)", beep_len);

    // BOOT button (GPIO9 on the C6, strapping pin with pull-up).
    let mut boot_button = Input::new(
        peripherals.GPIO9,
        InputConfig::default().with_pull(Pull::Up),
    );

    // === OTA foundation: report partition layout + boot slot ===
    let mut flash = esp_storage::FlashStorage::new(peripherals.FLASH);
    let mut config_offset: Option<u32> = None;
    {
        use esp_bootloader_esp_idf::partitions::{self, DataPartitionSubType, PartitionType};
        let mut pt_mem = [0u8; partitions::PARTITION_TABLE_MAX_LEN];
        match partitions::read_partition_table(&mut flash, &mut pt_mem) {
            Ok(pt) => {
                println!("[OTA] partition table: {} entries", pt.len());
                if let Ok(Some(cp)) =
                    pt.find_partition(PartitionType::Data(DataPartitionSubType::Spiffs))
                {
                    config_offset = Some(cp.offset());
                }
                match pt.find_partition(PartitionType::Data(DataPartitionSubType::Ota)) {
                    Ok(Some(od)) => {
                        let region = od.as_embedded_storage(&mut flash);
                        match esp_bootloader_esp_idf::ota::Ota::new(region, 2) {
                            Ok(mut ota) => println!(
                                "[OTA] boot slot {:?}, state {:?}",
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
    let (mut wifi_controller, wifi_interfaces) =
        esp_radio::wifi::new(peripherals.WIFI, Default::default()).expect("WiFi init failed");
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

    use esp_radio::wifi::sta::StationConfig;
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
    let mut wifi_has_creds = !watch_cfg.ssid.is_empty();
    if !wifi_has_creds {
        println!("[WIFI] no credentials - set them in Settings");
    }
    let mut station_config = esp_radio::wifi::Config::Station(
        StationConfig::default()
            .with_ssid(esp_radio::wifi::Ssid::from(watch_cfg.ssid.as_str()))
            .with_password(watch_cfg.pass.as_str().into()),
    );

    // ESP-NOW rides the same radio; usable whenever WiFi is started.
    let mut esp_now = wifi_interfaces.esp_now;

    let net_config = embassy_net::Config::dhcpv4(Default::default());
    // 4 sockets: DHCP + the always-on DNS socket + one transient TCP/UDP
    // (NTP, MQTT, weather, OTA — never concurrent) + one spare.
    static RESOURCES: StaticCell<embassy_net::StackResources<4>> = StaticCell::new();
    let (stack, runner) = embassy_net::new(
        wifi_interfaces.station,
        net_config,
        RESOURCES.init(embassy_net::StackResources::new()),
        12345u64,
    );
    _spawner.spawn(net_task(runner).expect("net_task token"));

    // #58: HA climate session infrastructure. The session runs in its own task
    // (holds WiFi while the Climate screen is open); main.rs drives it via the
    // open/close signals, reads the shared ClimateState for the UI each tick, and
    // releases the WiFi hold on `done` (both Ok + Err arms — see climate_task).
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
    // LP (low-power RISC-V) core status on the power page. No offload yet
    // (task #24 got a RED verdict), so this is a static availability indicator:
    // the LP core is idle at its ~20MHz clock (HP core runs 160MHz). One-shot.
    shell.set_lp_core("idle", 20);
    let mut power_stats = PowerStats::new();
    power_stats.cpu_mhz = 160;
    let mut app_state = AppState::Watchface;
    let mut prev_app_state = app_state;
    let mut snake_game = SnakeGame::new();
    // World Snake shares the SMOLv1 node id so its SNK frames name us.
    let mut world_snake = WorldSnakeApp::new(watch_cfg.node_id);
    let mut game_2048 = Game2048::new();
    let mut tetris_game = TetrisGame::new();
    let mut flappy_game = FlappyGame::new();
    let mut maze_game = MazeGame::new();
    let mut settings_app = SettingsApp::new();
    let mut last_touch_y: u16 = 0;
    let mut last_touch_x: u16 = 0;
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
    shell.render(&mut display);

    let mut next_rtc = Instant::now();
    let mut next_battery = Instant::now();
    let mut last_frame = Instant::now();
    let mut next_flush = Instant::now();
    // "Power down" now only gates the gyro: the accel stays on at 62.5Hz
    // so the QMI8658's hardware pedometer keeps counting in the background.
    let _ = imu.power_down();
    let mut imu_powered = false;
    let mut next_step_poll = Instant::now();
    let mut was_touching = false;

    // Radio state (user intent vs. actual radio state, per the S3 design).
    // DEBUG: auto-enable WiFi at boot while we diagnose the connect issue,
    // so no watchface tap is needed. Revert to `false` once stable.
    let mut wifi_on_request = wifi_has_creds;
    // #58: Climate session lifecycle. climate_active holds WiFi while the screen
    // is open (cleared on session return); climate_running gates the one-shot
    // open-signal so the session spawns once per screen visit.
    let mut climate_active = false;
    let mut energy_active = false;
    // #58 finding-(b): true when the shared session RAISED the WiFi hold (i.e. WiFi
    // was off when a HA screen opened). Cleared on the both-closed transition so we
    // drop WiFi promptly → mesh re-pins ch6, WITHOUT clobbering a manual WiFi-on.
    let mut session_holds_wifi = false;
    let mut climate_running = false;
    // Optimistic setpoint for the Climate detail (oracle-t9 C4/C5/E2).
    let mut climate_pending: Option<ClimatePending> = None;
    // #28 sound-level meter: whether the ADC+METER gate are currently armed, and
    // the decaying peak-hold value (dBFS). Only touched while app_state==Sound.
    let mut meter_on = false;
    let mut meter_peak = mic_dsp::DBFS_FLOOR;
    // "STA radio (PHY) started via set_config" — what ESP-NOW needs, decoupled
    // from WiFi credentials/association (that's `wifi_connected`). Set by either
    // the credentialed connect path OR a MESH toggle-on; the mesh block gates on
    // this, so mesh no longer requires WiFi creds to run.
    let mut radio_started = false;
    let mut wifi_scanned = false;
    let mut wifi_connected = false;
    let mut ntp_synced = false;
    let mut next_ntp_attempt = Instant::now();
    let mut wifi_toggle_request = false;
    let mut last_wifi_idle_check = Instant::now();
    let mut ble_on = false;
    let mut ble_toggle_request = false;
    let mut settings_connect_pending = false;
    let mut wifi_connect_attempts: u8 = 0;
    // SMOLv1 mesh: node id comes from flash config (default 042).
    let mut mesh = SmolMesh::new(watch_cfg.node_id);
    // Mesh Familiar (fleet #57): always-on holder/arbitration state machine,
    // ticked alongside mesh.tick. The creature renders on the watchface.
    let mut familiar = crate::net::familiar::FamState::new(watch_cfg.node_id);
    let mut esp_now_peer_added = false;
    // WiZmote frame sequence (WLED de-dups on it); wraps, monotonic per send.
    let mut wled_seq: u32 = 0;
    // RSSI treasure-hunt game state (fed from the mesh roster while Hunt is open).
    let mut hunt_state = hunt::HuntState::new();
    // Mesh enable flag (MESH chrome dot toggles it). Default OFF (power: the STA
    // radio only comes up when mesh is turned on). Toggling ON starts the radio
    // (below) then the ESP-NOW tick/rx/familiar run; OFF pauses the tick (peer
    // stays registered, radio stays up — a tick-level pause, not a teardown).
    let mut mesh_enabled = false;
    let mut mesh_channel_pinned = false;
    let mut last_mesh_peers: u8 = 0;
    let mut next_diag = Instant::now() + Duration::from_secs(30);
    // Time-sync provenance for the DIAG record (tsrc/tage fields).
    let mut sync_src: &str = "none";
    let mut last_sync = Instant::now();

    loop {
        let touch_held = touch_int.is_low();
        let button_held = boot_button.is_low();

        let tick = if touch_held || button_held {
            Duration::from_millis(16)
        } else if screen_state == 0 {
            Duration::from_secs(30)
        } else if screen_state == 1 {
            // AOD: wake often enough that the minute flip never looks stuck.
            Duration::from_secs(5)
        } else {
            match app_state {
                AppState::Watchface
                | AppState::Launcher
                | AppState::Wled
                | AppState::Hunt
                | AppState::Energy
                | AppState::Climate
                | AppState::Voice
                | AppState::Sound => {
                    // Slint animations (launcher slide, flings) need frame pacing;
                    // otherwise pace by the visible page's live-data cadence.
                    if app_state == AppState::Hunt {
                        // Warmer/colder wants a responsive feel; the RSSI EWMA +
                        // 1.5s trend lag keep 4 Hz from flickering.
                        Duration::from_millis(250)
                    } else if shell.has_active_animations() {
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
                AppState::Settings => Duration::from_millis(100),
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

        if screen_state == 1 {
            // AOD light sleep (#29, now default — tap-wake confirmed on glass):
            // park the HP core in light sleep
            // instead of WFI-idling. Wake on a 60s RTC timer OR touch (GPIO15) OR
            // boot button (GPIO9), both active-low. GPIO wake needs BOTH the pin
            // armed (`wakeup_enable`) AND the GpioWakeupSource trigger in the wake
            // set — the timer-only source would never wake on the pins. The clock
            // is the external PCF85063 (sleep-safe); embassy-time (TIMG0) pauses,
            // so it lags real time by the sleep span — fine, AOD repaints from the
            // RTC minute. `sleep_light` blocks (executor paused → mesh quiesces).
            let timer_wake = TimerWakeupSource::new(core::time::Duration::from_secs(60));
            let gpio_wake = GpioWakeupSource::new();
            let _ = touch_int.wakeup_enable(true, WakeEvent::LowLevel);
            let _ = boot_button.wakeup_enable(true, WakeEvent::LowLevel);
            let t0 = Instant::now();
            rtc_lp.sleep_light(&[&timer_wake, &gpio_wake]);
            let cause = wakeup_cause();
            // Disarm so normal falling-edge IRQ handling resumes.
            let _ = touch_int.wakeup_enable(false, WakeEvent::LowLevel);
            let _ = boot_button.wakeup_enable(false, WakeEvent::LowLevel);
            // embassy-time froze during sleep, so the loop's next_rtc gate won't
            // refresh last_dt — force a read now so the AOD minute repaint and the
            // wall-clock AOD->off (below) both see the real time (#29 DS1).
            if let Ok(dt) = rtc.get_time() {
                last_dt = Some(dt);
            }
            println!(
                "[AOD-SLEEP] woke cause={:?} embassy_lag_ms={}",
                cause,
                (Instant::now() - t0).as_millis()
            );
        } else {
            let _ = select3(
                Timer::after(tick),
                touch_int.wait_for_falling_edge(),
                boot_button.wait_for_falling_edge(),
            )
            .await;
        }

        let now = Instant::now();
        let dt_ms = (now - last_frame).as_millis() as u32;
        last_frame = now;

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
            }
            next_battery = if screen_state == 0 {
                now + Duration::from_secs(600)
            } else {
                now + Duration::from_secs(180)
            };
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
                if let Some(tp) = point {
                    last_touch_x = tp.x;
                    last_touch_y = tp.y;
                }
                if let Some(swipe) = event {
                    swipe_event = Some(swipe.direction);
                    swipe_start_y = swipe.start_y;
                    tap_event = swipe.direction == SwipeDirection::Tap;
                }
            }
        }

        // === Screen sleep/wake state machine ===
        let any_touch = touch_int.is_low();
        if any_touch || swipe_event.is_some() || tap_event || boot_button.is_low() {
            last_interaction = now;
            if screen_state < 3 {
                if screen_state == 0 {
                    display.display_on();
                    Timer::after(Duration::from_millis(20)).await;
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
            if app_state == AppState::Watchface && shell.page() == slint_shell::PAGE_CLOCK {
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

        // === WiFi state machine (one action per iteration) ===
        if wifi_toggle_request && (now - last_wifi_idle_check).as_millis() >= 1000 {
            wifi_toggle_request = false;
            last_wifi_idle_check = now;
            if wifi_has_creds {
                wifi_on_request = !wifi_on_request;
                println!("[WIFI] toggled -> {}", if wifi_on_request { "ON" } else { "OFF" });
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

        if wifi_on_request && !wifi_connected {
            if !radio_started && wifi_controller.set_config(&station_config).is_ok() {
                // Minimum PS until DHCP+NTP are done; Maximum breaks DHCP
                // under BLE coex. Switched to Maximum after first NTP sync.
                let _ = wifi_controller
                    .set_power_saving(esp_radio::wifi::PowerSaveMode::Minimum);
                radio_started = true;
            }
            if radio_started {
                // One-time diagnostic scan: is the AP visible, on what
                // channel, with what auth?
                if !wifi_scanned {
                    wifi_scanned = true;
                    match wifi_controller
                        .scan_async(&esp_radio::wifi::scan::ScanConfig::default())
                        .await
                    {
                        Ok(aps) => {
                            println!("[SCAN] {} networks:", aps.len());
                            for ap in aps.iter().take(12) {
                                println!(
                                    "[SCAN]   {:?} ch{} rssi{} auth={:?}",
                                    ap.ssid, ap.channel, ap.signal_strength, ap.auth_method
                                );
                            }
                        }
                        Err(e) => println!("[SCAN] failed: {e:?}"),
                    }
                }
                match embassy_time::with_timeout(
                    Duration::from_secs(15),
                    wifi_controller.connect_async(),
                )
                .await
                {
                    Ok(Ok(_)) => {
                        println!("[WIFI] connected");
                        wifi_connect_attempts = 0;
                        wifi_connected = true;
                        if settings_connect_pending {
                            settings_app.wifi_state =
                                crate::peripherals::wifi::WifiState::Connected;
                            settings_connect_pending = false;
                        }
                        // NTP happens from the main loop once DHCP lands.
                    }
                    other => {
                        // Transient hotspot errors (AuthenticationExpired etc.)
                        // are common - retry a few times before giving up.
                        wifi_connect_attempts += 1;
                        match other {
                            Ok(Err(e)) => println!(
                                "[WIFI] connect error (attempt {wifi_connect_attempts}/3): {e:?}"
                            ),
                            _ => println!(
                                "[WIFI] connect timeout (attempt {wifi_connect_attempts}/3)"
                            ),
                        }
                        if wifi_connect_attempts >= 3 {
                            wifi_connect_attempts = 0;
                            wifi_on_request = false;
                            if settings_connect_pending {
                                settings_app.wifi_state =
                                    crate::peripherals::wifi::WifiState::Error;
                                settings_connect_pending = false;
                            }
                        }
                    }
                }
            }
            last_wifi_idle_check = now;
        }
        if !wifi_on_request && wifi_connected {
            // esp-radio 0.18 has no controller stop(); full teardown means
            // dropping the controller. Disconnect + PS=Maximum leaves the
            // idle STA cheap enough for v1.
            let _ = wifi_controller.disconnect_async().await;
            println!("[WIFI] disconnected");
            wifi_connected = false;
            last_wifi_idle_check = now;
        }
        // Safety net: radio left on + 5 min idle -> auto-off.
        if wifi_on_request && idle_secs >= 300 && (now - last_wifi_idle_check).as_secs() >= 60 {
            wifi_on_request = false;
            last_wifi_idle_check = now;
        }

        // Detect link loss (AP gone, coex hiccup). wifi_on_request stays
        // true, so the connect branch above re-fires next iteration.
        if wifi_connected && !wifi_controller.is_connected() {
            println!("[WIFI] link lost - will reconnect");
            wifi_connected = false;
        }

        // NTP once DHCP is up; retry with a 10s backoff until it works.
        // After a successful sync the watch follows smol's TIME-SHARE design:
        // WiFi burst done -> drop the association -> pin ESP-NOW to the fixed
        // mesh channel. The watch becomes a mesh time authority.
        if wifi_connected && !ntp_synced && now >= next_ntp_attempt {
            if stack.config_v4().is_some() {
                if let Ok(unix) = ntp_sync(stack, &mut rtc).await {
                    ntp_synced = true;
                    sync_src = "ntp";
                    last_sync = now;
                    mesh.set_time_authoritative(unix, now.as_secs());
                    println!("[NTP] synced - RTC set, mesh authority claimed");
                    // MQTT burst to Home Assistant while the WiFi window is
                    // still open. Fire-and-forget: logs and moves on after at
                    // most ~5s; never blocks the boot/NTP/mesh flow.
                    crate::net::mqtt_ha::publish_burst(stack, batt_pct).await;
                    // Weather fetch in the same WiFi window (fire-and-forget,
                    // bounded at 8s; logs [WX] failed and moves on).
                    if let Some(wx) = crate::net::weather::fetch(stack).await {
                        last_weather = Some((wx.temp_f, wx.code));
                        shell.set_weather(Some(wx.temp_f), wx.code);
                    }
                    wifi_on_request = false; // WiFi burst complete
                } else {
                    println!("[NTP] failed, retrying in 10s");
                }
            }
            next_ntp_attempt = now + Duration::from_secs(10);
        }

        // TIME-SHARE steady state: whenever WiFi is down but the radio is up,
        // pin ESP-NOW to the fleet's fixed channel. Re-pin after any WiFi use.
        if radio_started && !wifi_connected && !mesh_channel_pinned {
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
        if wifi_connected && mesh_channel_pinned {
            mesh_channel_pinned = false; // rides the AP channel while associated
        }

        // === SMOLv1 mesh (ESP-NOW) ===
        if radio_started {
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
                        println!("[MESH] up as node id042");
                    }
                    Err(e) => println!("[MESH] add_peer failed: {e:?}"),
                }
            }
            if esp_now_peer_added && mesh_enabled {
                let now_ms = now.as_millis();
                let uptime_secs = now.as_secs();
                mesh.tick(&mut esp_now, now_ms, uptime_secs);
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
                    mesh.broadcast_diag(&mut esp_now, rec.as_bytes());
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
                        if let Ok(w) =
                            esp_now.send(&esp_radio::esp_now::BROADCAST_ADDRESS, &snk[..n])
                        {
                            let _ = w.wait();
                        }
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
                    mesh.relay_emit(&mut esp_now, tele.as_bytes(), now_ms);
                }
                mesh.relay_retransmit(&mut esp_now, now_ms);
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
                    );
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
                                match config_offset.map(|off| {
                                    peripherals::config::save(&mut flash, off, &watch_cfg)
                                }) {
                                    Some(Ok(())) => {
                                        println!("[CFG] default page {page} saved")
                                    }
                                    _ => println!("[CFG] save failed"),
                                }
                            }
                        }
                        // CFG `U`: store + persist (edge-triggered, as above).
                        Some(MeshEvent::CfgUnits { temp_f, clk_24h }) => {
                            if watch_cfg.units_temp_f != temp_f
                                || watch_cfg.units_clk_24h != clk_24h
                            {
                                watch_cfg.units_temp_f = temp_f;
                                watch_cfg.units_clk_24h = clk_24h;
                                match config_offset.map(|off| {
                                    peripherals::config::save(&mut flash, off, &watch_cfg)
                                }) {
                                    Some(Ok(())) => println!("[CFG] units saved"),
                                    _ => println!("[CFG] save failed"),
                                }
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
                        None => {}
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
                        mesh.broadcast_fam(&mut esp_now, &frame);
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
        // as "Rust Watch" and serves the Battery GATT service. The trouble
        // host owns the controller from then on and cannot be torn down at
        // runtime, so "off" requires a reboot; later presses just log that.
        // (The old raw-HCI scan/device-discovery logging was dropped: the
        // scanner would drive the central role against the same
        // single-connection peripheral host.)
        if ble_toggle_request {
            ble_toggle_request = false;
            if !ble_on {
                ble_on = true;
                crate::peripherals::ble::BLE_START_REQUEST
                    .store(true, core::sync::atomic::Ordering::Relaxed);
                println!("[BLE] GATT server start requested ('Rust Watch')");
            } else {
                println!("[BLE] host can't be stopped at runtime - reboot to disable");
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
            | AppState::Voice
            | AppState::Sound => {
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
                        | AppState::Voice
                        | AppState::Sound
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

                // Mirror overlay open-state into the scene, feed touch, then
                // reconcile any swipe-driven overlay close (Right-swipe / WLED
                // back-chevron) back into app_state. The shell owns page/launcher
                // navigation via swipes internally; WLED is a scene overlay that
                // shares this Slint branch (no framebuffer of its own).
                shell.set_launcher_open(app_state == AppState::Launcher);
                shell.set_wled_open(app_state == AppState::Wled);
                shell.set_hunt_open(app_state == AppState::Hunt);
                shell.set_energy_open(app_state == AppState::Energy);
                shell.set_climate_open(app_state == AppState::Climate);
                shell.set_voice_open(app_state == AppState::Voice);
                shell.set_mic_open(app_state == AppState::Sound);
                shell.handle_touch(touch_point, swipe_event, swipe_start_y);
                app_state = if shell.launcher_open() {
                    AppState::Launcher
                } else if shell.wled_open() {
                    AppState::Wled
                } else if shell.hunt_open() {
                    AppState::Hunt
                } else if shell.energy_open() {
                    AppState::Energy
                } else if shell.climate_open() {
                    AppState::Climate
                } else if shell.voice_open() {
                    AppState::Voice
                } else if shell.mic_open() {
                    AppState::Sound
                } else {
                    AppState::Watchface
                };

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
                        if radio_started && esp_now_peer_added {
                            wled_seq = wled_seq.wrapping_add(1);
                            let frame = wled_wizmote::encode_wizmote(btn, wled_seq, batt_pct);
                            if let Ok(w) =
                                esp_now.send(&esp_radio::esp_now::BROADCAST_ADDRESS, &frame)
                            {
                                let _ = w.wait();
                            }
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
                    climate_active = false;
                    shell.set_climate_open(false);
                    if app_state == AppState::Climate {
                        app_state = AppState::Watchface;
                    }
                }
                // WiFi hold + session start/stop, keyed on "either screen open".
                // When both close, releasing the hold returns the watch to mesh —
                // the unconditional restore (oracle-t10 inv b): however the session
                // ended (Ok close or Err), closing the screen(s) frees WiFi, so it
                // can never be stranded held.
                let climate_session_want = climate_active || energy_active;
                // Voice also needs WiFi (STT upload) but NOT the MQTT session, so it
                // widens the WiFi HOLD without touching the session start/stop. Keyed
                // on app_state==Voice: leaving the screen drops it out of wifi_want →
                // the release arm below frees WiFi + re-pins mesh (never stranded).
                let wifi_want = climate_session_want || app_state == AppState::Voice;
                if wifi_want {
                    wifi_on_request = true;
                    if climate_session_want && wifi_connected && !climate_running {
                        climate_open.signal(());
                        climate_running = true;
                    }
                } else {
                    // Both screens closed → RELEASE the WiFi hold we raised so the
                    // idle path drops WiFi + re-pins mesh ch6 PROMPTLY (finding-b:
                    // don't rely on the 300s idle backstop — it resets on every
                    // interaction, so an active user would keep the mesh off-fleet
                    // indefinitely). Gated on session_holds_wifi → a manual WiFi-on
                    // (toggle then Climate) is preserved. Then end the session.
                    if session_holds_wifi {
                        wifi_on_request = false;
                        session_holds_wifi = false;
                    }
                    if climate_running {
                        climate_close.signal(());
                    }
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
                    if let Some(p) = climate_pending.as_ref() {
                        let mut opt = st.clone();
                        if let Some((_, e)) = opt.entities.get_mut(p.id as usize) {
                            e.set = Some(p.temp);
                        }
                        shell.set_climate(&opt, conn);
                    } else {
                        shell.set_climate(&st, conn);
                    }
                } else {
                    let _ = shell.req.climate_set_temp.take();
                    let _ = shell.req.climate_set_mode.take();
                }
                // Energy screen: push the live EnergyState from the shared session.
                // conn-state: 0 ready · 1 connecting · 2 unreachable (HA LWT offline).
                if app_state == AppState::Energy {
                    let es = climate_energy.lock().await;
                    let conn = if !climate_running {
                        1
                    } else if !es.online {
                        2
                    } else if !es.has_data() {
                        // Session up + LWT online, but no EnergyState frame yet:
                        // stay "connecting" so the UI shows that instead of the
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
                if app_state == AppState::Voice && voice_pressed {
                    use core::sync::atomic::Ordering;
                    // Power the analog mic/ADC path, then arm the capture gate.
                    let _ = audio_codec.enable_adc(mic_capture::MIC_PGA_GAIN);
                    // Flush any chunks left buffered by a prior utterance so stale
                    // audio can't be prepended to this one, then arm + source.
                    let rx = MIC_CH.receiver();
                    while rx.try_receive().is_ok() {}
                    mic_capture::RECORDING.store(true, Ordering::Relaxed);
                    shell.set_voice_state(1); // listening
                    let mut src = mic_capture::MicPcmSource::new(rx);

                    // Release watcher: poll the INT pin (HIGH = no touch = finger up),
                    // then clear RECORDING (ends the source) and flip to "sending".
                    let watch_release = async {
                        loop {
                            if touch_int.is_high() {
                                break;
                            }
                            Timer::after(Duration::from_millis(20)).await;
                        }
                        mic_capture::RECORDING.store(false, Ordering::Relaxed);
                        shell.set_voice_state(2); // sending (STT round-trip in flight)
                    };

                    let (result, ()) =
                        join(voice_stt::stream_utterance(stack, &mut src), watch_release).await;

                    // Ensure the gate is down (belt-and-suspenders), then power the mic off.
                    mic_capture::RECORDING.store(false, Ordering::Relaxed);
                    let _ = audio_codec.disable_adc();

                    match result {
                        Ok(t) if !t.is_empty() => {
                            shell.set_voice_transcript(t.as_str());
                            shell.set_voice_state(3); // result
                        }
                        Ok(_) => {
                            shell.set_voice_error(""); // → page's "No speech heard"
                            shell.set_voice_state(4);
                        }
                        Err(e) => {
                            shell.set_voice_error(e);
                            shell.set_voice_state(4); // error
                        }
                    }
                    shell.request_redraw(); // paint the transcript/error promptly
                }

                // #28 sound-level meter: drain the SHARED capture → dBFS + peak-hold
                // on SoundLevel. Non-blocking (unlike the PTT flow, which parks the
                // loop): update once per tick so the screen stays responsive. Arms
                // the ADC + METER gate on entry, tears them down on close so the
                // codec draws ~0mA when the meter isn't open.
                if app_state == AppState::Sound {
                    if !meter_on {
                        let _ = audio_codec.enable_adc(mic_capture::MIC_PGA_GAIN);
                        mic_capture::METER.store(true, core::sync::atomic::Ordering::Relaxed);
                        meter_peak = mic_dsp::DBFS_FLOOR;
                        meter_on = true;
                    }
                    // Drain all buffered chunks; rms the newest for a live meter.
                    let rx = MIC_CH.receiver();
                    let mut latest: Option<f32> = None;
                    while let Ok(chunk) = rx.try_receive() {
                        let n = chunk.len() / 2;
                        let mut samples = [0i16; mic_capture::MONO_CHUNK / 2];
                        for i in 0..n {
                            samples[i] = i16::from_le_bytes([chunk[2 * i], chunk[2 * i + 1]]);
                        }
                        latest = Some(mic_dsp::rms_dbfs(&samples[..n]));
                    }
                    if let Some(dbfs) = latest {
                        // Peak-hold with slow decay so it tracks down after a transient.
                        meter_peak = (meter_peak - 0.5).max(dbfs).max(mic_dsp::DBFS_FLOOR);
                        shell.set_mic_level(dbfs, meter_peak);
                    }
                } else if meter_on {
                    mic_capture::METER.store(false, core::sync::atomic::Ordering::Relaxed);
                    let _ = audio_codec.disable_adc();
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
                                wifi_on_request,
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
                            shell.set_mesh_rows(watch_cfg.node_id, &rows[..n]);
                            next_flush = now + Duration::from_secs(1);
                        }
                    }
                    _ => {}
                }

                // 1Hz clock push (no-ops until the second actually ticks).
                if let Some(dt) = last_dt.as_ref() {
                    let _ = shell.set_time(dt);
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
                        // connecting). The mesh block gates on radio_started, so
                        // this is what actually lets the mesh come up. The channel
                        // pin (set_channel(MESH_CHANNEL)) rides the existing path.
                        if !radio_started && wifi_controller.set_config(&station_config).is_ok() {
                            let _ = wifi_controller
                                .set_power_saving(esp_radio::wifi::PowerSaveMode::Minimum);
                            radio_started = true;
                            println!("[MESH] STA radio started for ESP-NOW");
                        }
                    } else {
                        // Reflect "off" in the MESH chrome dot immediately; peers
                        // repopulate from HELLOs once re-enabled. Radio stays up
                        // (tick-level pause, not a teardown).
                        last_mesh_peers = 0;
                    }
                    println!("[MESH] toggled -> {}", if mesh_enabled { "ON" } else { "OFF" });
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
                    // If WiFi is up and an OTA_URL was baked in at build time,
                    // try to stage an OTA update first; reboot either way.
                    if wifi_connected && crate::net::ota_http::URL_SET {
                        if let Err(e) = crate::net::ota_http::ota_update(stack, &mut flash).await {
                            println!("[OTA] failed: {e}");
                        }
                    }
                    esp_hal::system::software_reset();
                }
                if let Some(target) = shell.req.launch.take() {
                    shell.set_launcher_open(false);
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
                        energy_active = true;
                        if !wifi_on_request {
                            session_holds_wifi = true; // we're raising the hold
                        }
                        wifi_on_request = true;
                        app_state = AppState::Energy;
                    } else if target == AppState::Climate {
                        // #58: raise the Climate overlay + hold WiFi up. The MQTT
                        // session task starts once WiFi associates (Climate tick
                        // below); released on session return (both Ok + Err).
                        shell.set_climate_open(true);
                        climate_active = true;
                        if !wifi_on_request {
                            session_holds_wifi = true; // we're raising the hold
                        }
                        wifi_on_request = true;
                        app_state = AppState::Climate;
                    } else if target == AppState::Voice {
                        // Voice-to-text (#42): a Slint overlay (scene-resident, no
                        // fb). Open in idle; the PTT flow below drives capture +
                        // transcript. Reset to idle each open so a prior transcript
                        // or error doesn't linger.
                        shell.set_voice_state(0);
                        shell.set_voice_transcript("");
                        shell.set_voice_error("");
                        shell.set_voice_open(true);
                        // STT is WiFi-dependent (HTTP to the LAN bridge). Hold WiFi
                        // up like climate/energy: raise it here, release + restore
                        // mesh on close. The hold is keyed on app_state==Voice in the
                        // WiFi-want block below, so leaving the screen (right-swipe →
                        // reconcile → app_state=Watchface) deterministically frees it
                        // → never strands the mesh. session_holds_wifi guards a manual
                        // WiFi-on (toggle then Voice) so we don't drop it on close.
                        if !wifi_on_request {
                            session_holds_wifi = true; // we're raising the hold
                        }
                        wifi_on_request = true;
                        app_state = AppState::Voice;
                    } else if target == AppState::Sound {
                        // Sound-level meter (#28): a Slint overlay (scene-resident,
                        // no fb). NO WiFi — rms_dbfs is local. The per-tick meter
                        // block below arms the ADC + METER gate on entry and drains
                        // MIC_CH → rms_dbfs → dBFS/peak; tears them down on close.
                        shell.set_mic_open(true);
                        app_state = AppState::Sound;
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
                                // Run the SAME per-app setup the old launcher arm did
                                // (without setup the games boot into garbage).
                                match target {
                                    AppState::Snake => snake_game.setup(),
                                    AppState::WorldSnake => world_snake.setup(),
                                    AppState::Game2048 => {
                                        let fb = fb.as_mut().unwrap();
                                        game_2048.setup();
                                        game_2048.render(fb);
                                        fb.flush(&mut display);
                                    }
                                    AppState::Tetris => tetris_game.setup(),
                                    AppState::Flappy => flappy_game.setup(),
                                    AppState::Maze => maze_game.setup(),
                                    AppState::Settings => {}
                                    _ => {}
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

                // BOOT button toggles the launcher overlay.
                if boot_button.is_low() {
                    let opening = app_state == AppState::Watchface;
                    shell.set_launcher_open(opening);
                    app_state = if opening {
                        AppState::Launcher
                    } else {
                        AppState::Watchface
                    };
                    Timer::after(Duration::from_millis(200)).await;
                }

                // Auto-clear the RAM-busy toast once its window elapses (guarded
                // so we only push the empty string a single time).
                if toast_active && now >= toast_until {
                    shell.set_toast("");
                    toast_active = false;
                }

                // Repaint if the scene is dirty (full-frame, line-streamed).
                // Skip when a launch just switched us into an app this iteration:
                // that app already painted its first frame (e.g. Game2048) and the
                // trailing shell repaint would clobber it.
                if matches!(
                    app_state,
                    AppState::Watchface
                        | AppState::Launcher
                        | AppState::Wled
                        | AppState::Hunt
                        | AppState::Energy
                        | AppState::Climate
                        | AppState::Voice
                        | AppState::Sound
                ) {
                    if screen_state >= 2 {
                        shell.render(&mut display);
                    } else if screen_state == 1 {
                        // AOD: repaint only when the minute changes so the dim
                        // scene isn't driven every wake. last_dt is refreshed at
                        // state >= 1 above; set_time (shell arm) dirtied the scene.
                        if let Some(dt) = last_dt.as_ref() {
                            if dt.minutes != aod_last_minute {
                                aod_last_minute = dt.minutes;
                                shell.render(&mut display);
                            }
                        }
                    }
                }
            }

            AppState::Snake => {
                let Some(fb_ref) = fb.as_mut() else {
                    app_state = AppState::Watchface;
                    continue;
                };
                let prev_score = snake_game.score();
                let input = AppInput {
                    touch: None,
                    swipe: swipe_event,
                    tap: tap_event,
                    accel,
                    dt_ms: dt_ms.max(1),
                };
                match snake_game.update(&input) {
                    AppResult::Continue => {
                        if snake_game.stepped() {
                            snake_game.render(fb_ref);
                            fb_ref.flush(&mut display);
                            // Beep when food eaten via I2S DMA
                            if snake_game.score() > prev_score {
                                // Unmute codec, then raise the amp, then play
                                let _ = audio_codec.unmute();
                                delay.delay_millis(2); // let codec stabilize before enabling amp
                                amp_en.set_high();
                                if let Ok(transfer) = i2s_tx.write_dma(&beep_buf) {
                                    let _ = transfer.wait();
                                }
                                // Lower amp FIRST, then mute codec to avoid pop
                                amp_en.set_low();
                                let _ = audio_codec.mute();
                            }
                        }
                    }
                    AppResult::Exit => {
                        app_state = AppState::Watchface;
                        fb = None;
                        println!("[HEAP] app exit free: {}", esp_alloc::HEAP.free());
                    }
                }
                if boot_button.is_low() {
                    app_state = AppState::Watchface;
                    fb = None;
                    println!("[HEAP] app exit free: {}", esp_alloc::HEAP.free());
                    Timer::after(Duration::from_millis(200)).await;
                }
            }

            AppState::WorldSnake => {
                let Some(fb_ref) = fb.as_mut() else {
                    app_state = AppState::Watchface;
                    continue;
                };
                let input = AppInput {
                    touch: None,
                    swipe: swipe_event,
                    tap: tap_event,
                    accel,
                    dt_ms: dt_ms.max(1),
                };
                world_snake.update(&input);
                // Remote peers dead-reckon between our steps, so repaint on a
                // steady cadence rather than only on local steps.
                if now >= next_flush {
                    world_snake.render(fb_ref);
                    fb_ref.flush(&mut display);
                    next_flush = now + Duration::from_millis(33);
                }
                if boot_button.is_low() {
                    app_state = AppState::Launcher;
                    fb = None;
                    println!("[HEAP] app exit free: {}", esp_alloc::HEAP.free());
                    Timer::after(Duration::from_millis(200)).await;
                }
            }

            AppState::Game2048 => {
                let Some(fb_ref) = fb.as_mut() else {
                    app_state = AppState::Watchface;
                    continue;
                };
                let input = AppInput {
                    touch: None,
                    swipe: swipe_event,
                    tap: tap_event,
                    accel,
                    dt_ms: dt_ms.max(1),
                };
                game_2048.update(&input);
                if swipe_event.is_some() {
                    game_2048.render(fb_ref);
                    fb_ref.flush(&mut display);
                }
                if boot_button.is_low() {
                    app_state = AppState::Launcher;
                    fb = None;
                    println!("[HEAP] app exit free: {}", esp_alloc::HEAP.free());
                    Timer::after(Duration::from_millis(200)).await;
                }
            }

            AppState::Tetris => {
                let Some(fb_ref) = fb.as_mut() else {
                    app_state = AppState::Watchface;
                    continue;
                };
                let input = AppInput {
                    touch: None,
                    swipe: swipe_event,
                    tap: tap_event,
                    accel,
                    dt_ms: dt_ms.max(1),
                };
                tetris_game.update(&input);
                if tetris_game.stepped() || swipe_event.is_some() || tap_event {
                    tetris_game.render(fb_ref);
                    fb_ref.flush(&mut display);
                }
                if boot_button.is_low() {
                    app_state = AppState::Launcher;
                    fb = None;
                    println!("[HEAP] app exit free: {}", esp_alloc::HEAP.free());
                    Timer::after(Duration::from_millis(200)).await;
                }
            }

            AppState::Flappy => {
                let Some(fb_ref) = fb.as_mut() else {
                    app_state = AppState::Watchface;
                    continue;
                };
                let touch_down = touch_int.is_low();
                let fake_touch = if touch_down {
                    Some(crate::peripherals::touch::TouchPoint {
                        x: 200,
                        y: 250,
                        fingers: 1,
                    })
                } else {
                    None
                };
                let input = AppInput {
                    touch: fake_touch,
                    swipe: swipe_event,
                    tap: tap_event,
                    accel,
                    dt_ms: dt_ms.max(1),
                };
                flappy_game.update(&input);
                flappy_game.render(fb_ref);
                if now >= next_flush {
                    fb_ref.flush(&mut display);
                    next_flush = now + Duration::from_millis(33);
                }
                if boot_button.is_low() {
                    app_state = AppState::Launcher;
                    fb = None;
                    println!("[HEAP] app exit free: {}", esp_alloc::HEAP.free());
                    Timer::after(Duration::from_millis(200)).await;
                }
            }

            AppState::Maze => {
                let Some(fb_ref) = fb.as_mut() else {
                    app_state = AppState::Watchface;
                    continue;
                };
                let input = AppInput {
                    touch: None,
                    swipe: swipe_event,
                    tap: tap_event,
                    accel,
                    dt_ms: dt_ms.max(1),
                };
                maze_game.update(&input);
                if now >= next_flush {
                    maze_game.render(fb_ref);
                    fb_ref.flush(&mut display);
                    next_flush = now + Duration::from_millis(33);
                }
                if boot_button.is_low() {
                    app_state = AppState::Launcher;
                    fb = None;
                    println!("[HEAP] app exit free: {}", esp_alloc::HEAP.free());
                    Timer::after(Duration::from_millis(200)).await;
                }
            }

            AppState::Settings => {
                let Some(fb_ref) = fb.as_mut() else {
                    app_state = AppState::Watchface;
                    continue;
                };
                // CONNECT pressed in Settings: persist creds to flash and
                // (re)start WiFi with them.
                use crate::peripherals::wifi::WifiState;
                if settings_app.wifi_state == WifiState::Connecting && !settings_connect_pending {
                    let ssid = settings_app.wifi_config.ssid_str();
                    if ssid.is_empty() {
                        settings_app.wifi_state = WifiState::Error;
                    } else {
                        watch_cfg.ssid.clear();
                        let _ = watch_cfg.ssid.push_str(ssid);
                        watch_cfg.pass.clear();
                        let pw = core::str::from_utf8(
                            &settings_app.wifi_config.password
                                [..settings_app.wifi_config.pass_len],
                        )
                        .unwrap_or("");
                        let _ = watch_cfg.pass.push_str(pw);
                        match config_offset
                            .map(|off| peripherals::config::save(&mut flash, off, &watch_cfg))
                        {
                            Some(Ok(())) => println!("[CFG] credentials saved to flash"),
                            _ => println!("[CFG] save failed"),
                        }
                        station_config = esp_radio::wifi::Config::Station(
                            StationConfig::default()
                                .with_ssid(esp_radio::wifi::Ssid::from(watch_cfg.ssid.as_str()))
                                .with_password(watch_cfg.pass.as_str().into()),
                        );
                        wifi_has_creds = true;
                        radio_started = false;
                        wifi_connected = false;
                        ntp_synced = false;
                        wifi_on_request = true;
                        settings_connect_pending = true;
                    }
                }
                settings_app.update(dt_ms.max(1));
                if tap_event {
                    settings_app.handle_tap(last_touch_x, last_touch_y);
                }
                if let Ok((Some(tp), _)) = touch.poll() {
                    last_touch_x = tp.x;
                    last_touch_y = tp.y;
                }
                settings_app.render(fb_ref);
                if now >= next_flush {
                    fb_ref.flush(&mut display);
                    next_flush = now + Duration::from_millis(50);
                }
                if boot_button.is_low() {
                    app_state = AppState::Launcher;
                    fb = None;
                    println!("[HEAP] app exit free: {}", esp_alloc::HEAP.free());
                    Timer::after(Duration::from_millis(200)).await;
                }
            }

            _ => {
                app_state = AppState::Watchface;
                fb = None;
            }
        }

        // Track the arm we ran so the shell arm can detect a return from an app
        // that painted straight to the panel and force a repaint. Use the
        // pre-dispatch snapshot, not the (possibly mutated) current app_state.
        prev_app_state = dispatched;
    }
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
