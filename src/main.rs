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
    gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull},
    i2c::master::{Config as I2cConfig, I2c},
    i2s::master::{Config as I2sConfig, DataFormat, I2s},
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
use crate::apps::{App, AppInput, AppResult, AppState};
use crate::drivers::co5300::Co5300Display;
use crate::net::smol_mesh::{MeshEvent, SmolMesh};
use crate::drivers::framebuffer::Framebuffer;
use crate::drivers::qspi_bus::QspiBus;
use crate::peripherals::audio::{fill_beep_buffer, Es8311};
use crate::peripherals::imu::Qmi8658Imu;
use crate::peripherals::power::Axp2101Power;
use crate::peripherals::power_stats::{DisplayState, PowerStats, WifiMode};
use crate::peripherals::rtc::Pcf85063aRtc;
use crate::peripherals::touch::{Ft3168Touch, SwipeDirection};
use crate::ui::launcher::Launcher;
use crate::ui::pages::{self, Page};
use crate::ui::power_page;
use crate::ui::watchface::WatchFace;

extern crate alloc;

esp_bootloader_esp_idf::esp_app_desc!();

#[embassy_executor::task]
async fn net_task(
    mut runner: embassy_net::Runner<'static, esp_radio::wifi::Interface<'static>>,
) -> ! {
    runner.run().await
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

    // RGB332 framebuffer (~201KB) + app allocations live here. The main heap
    // sits in regular DRAM; the small ROM-reclaimed region (~64KB) is added
    // as a second pool so nothing goes to waste.
    esp_alloc::heap_allocator!(size: 240 * 1024);
    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 56 * 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_interrupt =
        esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_interrupt.software_interrupt0);

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

    let mut fb = Framebuffer::new();
    fb.clear_color(Rgb565::BLACK);
    fb.flush(&mut display);
    println!("[FB] OK (RGB332 in SRAM)");

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
    let (mut wifi_controller, wifi_interfaces) =
        esp_radio::wifi::new(peripherals.WIFI, Default::default()).expect("WiFi init failed");
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

    println!("=== All systems GO! ===");

    // === State ===
    let mut watchface = WatchFace::new();
    watchface.wifi_connected = false;
    watchface.cpu_mhz = 160;
    watchface.brightness = watch_cfg.brightness;
    display.set_brightness(watch_cfg.brightness);
    let mut current_page = Page::Clock;
    let mut power_stats = PowerStats::new();
    power_stats.cpu_mhz = 160;
    let mut app_state = AppState::Watchface;
    let mut snake_game = SnakeGame::new();
    let mut game_2048 = Game2048::new();
    let mut tetris_game = TetrisGame::new();
    let mut flappy_game = FlappyGame::new();
    let mut maze_game = MazeGame::new();
    let mut launcher = Launcher::new();
    let mut settings_app = SettingsApp::new();
    let mut last_touch_y: u16 = 0;
    let mut last_touch_x: u16 = 0;
    let mut accel = (0.0f32, 0.0f32, 0.0f32);
    let mut gyro_data = (0i16, 0i16, 0i16);
    let mut imu_temp: i16 = 250;
    let mut batt_pct: u8 = 0;
    let mut batt_mv: u16 = 0;
    let mut charging = false;
    let mut page_dirty = true;
    let mut last_interaction = Instant::now();
    // 3=bright 2=dim 1=AOD 0=off (see the S3 firmware for the rationale)
    let mut screen_state: u8 = 3;
    let mut aod_last_minute: u8 = 99;

    // Initial render
    if let Ok(pct) = power.get_battery_percent() {
        batt_pct = pct;
        batt_mv = power.get_battery_voltage().unwrap_or(0);
        charging = power.is_charging().unwrap_or(false);
        watchface.update_battery(batt_pct, batt_mv, charging);
        crate::peripherals::ble::BATTERY_PERCENT
            .store(batt_pct, core::sync::atomic::Ordering::Relaxed);
    }
    if let Ok(dt) = rtc.get_time() {
        watchface.update_time(dt.hours, dt.minutes, dt.seconds);
        watchface.update_date(dt.day, dt.month, dt.year);
    }
    watchface.force_redraw();
    let _ = watchface.render(&mut fb);
    fb.flush(&mut display);

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
    let mut wifi_started = false;
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
    let mut esp_now_peer_added = false;
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
            Duration::from_secs(10)
        } else {
            match app_state {
                AppState::Watchface => match current_page {
                    Page::Clock => {
                        if watchface.gyro_enabled {
                            Duration::from_millis(33)
                        } else {
                            Duration::from_secs(1)
                        }
                    }
                    Page::Sensors => Duration::from_millis(100),
                    Page::System => Duration::from_secs(2),
                    Page::Power => Duration::from_secs(1),
                },
                AppState::Launcher | AppState::Settings => Duration::from_millis(100),
                _ => Duration::from_millis(33),
            }
        };

        let _ = select3(
            Timer::after(tick),
            touch_int.wait_for_falling_edge(),
            boot_button.wait_for_falling_edge(),
        )
        .await;

        let now = Instant::now();
        let dt_ms = (now - last_frame).as_millis() as u32;
        last_frame = now;

        // === IMU gating ===
        let need_imu = screen_state >= 2
            && (watchface.gyro_enabled
                || app_state == AppState::Maze
                || app_state == AppState::Tetris
                || app_state == AppState::Flappy
                || (app_state == AppState::Watchface && current_page == Page::Sensors));
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
                watchface.update_accel(a.x, a.y, a.z);
            }
            if let Ok(g) = imu.read_gyro() {
                gyro_data = ((g.x * 10.0) as i16, (g.y * 10.0) as i16, (g.z * 10.0) as i16);
            }
            if let Ok(t) = imu.read_temperature() {
                imu_temp = (t * 10.0) as i16;
            }
        }

        // === RTC 1Hz ===
        if screen_state >= 2 && now >= next_rtc {
            if let Ok(dt) = rtc.get_time() {
                watchface.update_time(dt.hours, dt.minutes, dt.seconds);
                watchface.update_date(dt.day, dt.month, dt.year);
            }
            next_rtc = now + Duration::from_secs(1);
        }

        // === Pedometer ===
        // The hardware step counter runs even while the IMU is "powered
        // down" (gyro off, accel on). One cheap 3-byte I2C read per minute.
        if now >= next_step_poll {
            if let Ok(steps) = imu.read_step_count() {
                watchface.steps = steps;
            }
            next_step_poll = now + Duration::from_secs(60);
        }

        // === Battery ===
        if now >= next_battery {
            if let Ok(pct) = power.get_battery_percent() {
                batt_pct = pct;
                batt_mv = power.get_battery_voltage().unwrap_or(0);
                charging = power.is_charging().unwrap_or(false);
                watchface.update_battery(batt_pct, batt_mv, charging);
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
        let mut tap_event = false;
        let int_low = touch_int.is_low();
        let touch_active = screen_state >= 2 && (int_low || was_touching);
        was_touching = int_low;
        if touch_active {
            if let Ok((point, event)) = touch.poll() {
                if let Some(tp) = point {
                    last_touch_x = tp.x;
                    last_touch_y = tp.y;
                }
                if let Some(swipe) = event {
                    swipe_event = Some(swipe.direction);
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
                display.set_brightness(watchface.brightness);
                screen_state = 3;
                next_flush = now;
                if app_state == AppState::Watchface {
                    watchface.force_redraw();
                    page_dirty = true;
                }
            }
        }
        let idle_secs = (now - last_interaction).as_secs();
        if idle_secs >= 180 && screen_state > 0 {
            display.set_brightness(0x00);
            display.display_off();
            screen_state = 0;
        } else if idle_secs >= 15 && screen_state > 1 {
            if app_state == AppState::Watchface && current_page == Page::Clock {
                display.set_brightness(0x18);
                screen_state = 1;
                aod_last_minute = 99;
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
            wifi_on_request = !wifi_on_request && wifi_has_creds;
            wifi_toggle_request = false;
            last_wifi_idle_check = now;
            println!("[WIFI] toggled -> {}", if wifi_on_request { "ON" } else { "OFF" });
        } else if wifi_toggle_request {
            wifi_toggle_request = false;
        }

        if wifi_on_request && !wifi_connected {
            if !wifi_started && wifi_controller.set_config(&station_config).is_ok() {
                // Minimum PS until DHCP+NTP are done; Maximum breaks DHCP
                // under BLE coex. Switched to Maximum after first NTP sync.
                let _ = wifi_controller
                    .set_power_saving(esp_radio::wifi::PowerSaveMode::Minimum);
                wifi_started = true;
            }
            if wifi_started {
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
                        watchface.wifi_connected = true;
                        watchface.force_redraw();
                        page_dirty = true;
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
                            watchface.wifi_connected = false;
                            watchface.force_redraw();
                            page_dirty = true;
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
            watchface.wifi_connected = false;
            watchface.force_redraw();
            page_dirty = true;
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
            watchface.wifi_connected = false;
            watchface.force_redraw();
            page_dirty = true;
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
                        watchface.weather_temp_f = Some(wx.temp_f);
                        watchface.weather_code = wx.code;
                        watchface.force_redraw();
                        page_dirty = true;
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
        if wifi_started && !wifi_connected && !mesh_channel_pinned {
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
        if wifi_started {
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
            if esp_now_peer_added {
                let now_ms = now.as_millis();
                let uptime_secs = now.as_secs();
                mesh.tick(&mut esp_now, now_ms, uptime_secs);
                let peers = mesh.peer_count(now_ms) as u8;
                if peers != last_mesh_peers {
                    last_mesh_peers = peers;
                    watchface.mesh_peers = peers;
                    watchface.force_redraw();
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
                while let Some(rx) = esp_now.receive() {
                    if let Some(MeshEvent::TimeAdopted { unix, from_id }) = mesh.handle_rx(
                        &mut esp_now,
                        rx.info.src_address,
                        rx.data(),
                        now_ms,
                        uptime_secs,
                    ) {
                        let (h, m, s) = set_rtc_from_unix(&mut rtc, unix);
                        sync_src = "mesh";
                        last_sync = now;
                        println!(
                            "[MESH] RTC set from mesh (id{from_id}): {h:02}:{m:02}:{s:02}"
                        );
                        if let Ok(dt) = rtc.get_time() {
                            watchface.update_time(dt.hours, dt.minutes, dt.seconds);
                            watchface.update_date(dt.day, dt.month, dt.year);
                        }
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
            watchface.ble_on = ble_on;
            power_stats.ble_on = ble_on;
            watchface.force_redraw();
            page_dirty = true;
        }

        // === AOD ===
        if screen_state == 1 {
            if let Ok(dt) = rtc.get_time() {
                if dt.minutes != aod_last_minute {
                    aod_last_minute = dt.minutes;
                    watchface.update_time(dt.hours, dt.minutes, dt.seconds);
                    if let Ok(pct) = power.get_battery_percent() {
                        watchface.update_battery(pct, batt_mv, charging);
                        crate::peripherals::ble::BATTERY_PERCENT
                            .store(pct, core::sync::atomic::Ordering::Relaxed);
                    }
                    let _ = watchface.render_aod(&mut fb);
                    fb.flush(&mut display);
                }
            }
            continue;
        }
        if screen_state == 0 {
            continue;
        }

        // === App state machine ===
        match app_state {
            AppState::Watchface => {
                // Swipe left/right switches pages (no slide animation on C6).
                match swipe_event {
                    Some(SwipeDirection::Left) => {
                        current_page = current_page.next();
                        page_dirty = true;
                    }
                    Some(SwipeDirection::Right) => {
                        current_page = current_page.prev();
                        page_dirty = true;
                    }
                    _ => {}
                }

                let mut need_flush = false;
                if page_dirty {
                    fb.clear_color(current_page.color());
                    match current_page {
                        Page::Clock => {
                            watchface.force_redraw();
                        }
                        Page::System => {
                            let _ = pages::draw_system_page(&mut fb, batt_mv, batt_pct, charging);
                        }
                        Page::Power => {
                            update_power_stats(
                                &mut power_stats,
                                screen_state,
                                imu_powered,
                                wifi_connected,
                                wifi_on_request,
                                watchface.brightness,
                                batt_mv,
                                batt_pct,
                                charging,
                            );
                            let _ = power_page::draw_power_page(&mut fb, &power_stats);
                        }
                        _ => {}
                    }
                    page_dirty = false;
                    need_flush = true;
                }
                match current_page {
                    Page::Clock => {
                        if watchface.needs_render() {
                            let _ = watchface.render(&mut fb);
                            need_flush = true;
                        }
                    }
                    Page::Sensors => {
                        let ax = (accel.0 * 100.0) as i16;
                        let ay = (accel.1 * 100.0) as i16;
                        let az = (accel.2 * 100.0) as i16;
                        fb.clear_color(current_page.color());
                        let _ = pages::draw_sensors_page(
                            &mut fb, ax, ay, az, gyro_data.0, gyro_data.1, gyro_data.2, imu_temp,
                        );
                        need_flush = true;
                    }
                    Page::Power => {
                        if now >= next_flush {
                            update_power_stats(
                                &mut power_stats,
                                screen_state,
                                imu_powered,
                                wifi_connected,
                                wifi_on_request,
                                watchface.brightness,
                                batt_mv,
                                batt_pct,
                                charging,
                            );
                            let _ = power_page::draw_power_page(&mut fb, &power_stats);
                            need_flush = true;
                            next_flush = now + Duration::from_secs(1);
                        }
                    }
                    Page::System => {}
                }
                if need_flush {
                    fb.flush(&mut display);
                    next_flush = now;
                }

                // Tap dispatch on the Clock page.
                if current_page == Page::Clock {
                    if let Some(bri) = WatchFace::brightness_from_tap(last_touch_x, last_touch_y) {
                        if (touch_int.is_low() || tap_event) && bri != watchface.brightness {
                            watchface.brightness = bri;
                            display.set_brightness(bri);
                            watchface.force_redraw();
                            page_dirty = true;
                        }
                    } else if tap_event {
                        if WatchFace::is_ble_zone(last_touch_x, last_touch_y) {
                            ble_toggle_request = true;
                            watchface.force_redraw();
                            page_dirty = true;
                        } else if WatchFace::is_wifi_zone(last_touch_x, last_touch_y) {
                            wifi_toggle_request = true;
                            watchface.force_redraw();
                            page_dirty = true;
                        } else if WatchFace::is_cpu_zone(last_touch_x, last_touch_y) {
                            watchface.cycle_cpu();
                            let actual =
                                crate::peripherals::cpu_clock::set_cpu_mhz(watchface.cpu_mhz);
                            watchface.cpu_mhz = actual;
                            power_stats.cpu_mhz = actual;
                            watchface.force_redraw();
                            page_dirty = true;
                        } else if WatchFace::is_apps_zone(last_touch_x, last_touch_y) {
                            app_state = AppState::Launcher;
                        } else if WatchFace::is_gyro_zone(last_touch_y) {
                            let enabled = watchface.toggle_gyro();
                            println!("Gyro: {}", if enabled { "ON" } else { "OFF" });
                        }
                    }
                }

                if current_page == Page::Power
                    && tap_event
                    && power_page::is_reboot_zone(last_touch_x, last_touch_y)
                {
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

                if let Some(SwipeDirection::Up) = swipe_event {
                    if current_page == Page::Clock {
                        app_state = AppState::Launcher;
                    }
                }
                if boot_button.is_low() {
                    app_state = AppState::Launcher;
                    Timer::after(Duration::from_millis(200)).await;
                }
            }

            AppState::Snake => {
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
                            snake_game.render(&mut fb);
                            fb.flush(&mut display);
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
                        watchface.force_redraw();
                        page_dirty = true;
                    }
                }
                if boot_button.is_low() {
                    app_state = AppState::Watchface;
                    watchface.force_redraw();
                    page_dirty = true;
                    Timer::after(Duration::from_millis(200)).await;
                }
            }

            AppState::Launcher => {
                if let Ok((point, _)) = touch.poll() {
                    if let Some(tp) = point {
                        last_touch_y = tp.y;
                    }
                }
                if let Some(new_state) = launcher.update(swipe_event, tap_event, last_touch_y) {
                    app_state = new_state;
                    match app_state {
                        AppState::Snake => snake_game.setup(),
                        AppState::Game2048 => {
                            game_2048.setup();
                            game_2048.render(&mut fb);
                            fb.flush(&mut display);
                        }
                        AppState::Tetris => tetris_game.setup(),
                        AppState::Flappy => flappy_game.setup(),
                        AppState::Maze => maze_game.setup(),
                        AppState::Settings => {}
                        AppState::Watchface => {
                            watchface.force_redraw();
                            page_dirty = true;
                        }
                        _ => {}
                    }
                } else {
                    launcher.render(&mut fb);
                    fb.flush(&mut display);
                }
                if boot_button.is_low() {
                    app_state = AppState::Watchface;
                    watchface.force_redraw();
                    page_dirty = true;
                    Timer::after(Duration::from_millis(200)).await;
                }
            }

            AppState::Game2048 => {
                let input = AppInput {
                    touch: None,
                    swipe: swipe_event,
                    tap: tap_event,
                    accel,
                    dt_ms: dt_ms.max(1),
                };
                game_2048.update(&input);
                if swipe_event.is_some() {
                    game_2048.render(&mut fb);
                    fb.flush(&mut display);
                }
                if boot_button.is_low() {
                    app_state = AppState::Launcher;
                    Timer::after(Duration::from_millis(200)).await;
                }
            }

            AppState::Tetris => {
                let input = AppInput {
                    touch: None,
                    swipe: swipe_event,
                    tap: tap_event,
                    accel,
                    dt_ms: dt_ms.max(1),
                };
                tetris_game.update(&input);
                if tetris_game.stepped() || swipe_event.is_some() || tap_event {
                    tetris_game.render(&mut fb);
                    fb.flush(&mut display);
                }
                if boot_button.is_low() {
                    app_state = AppState::Launcher;
                    Timer::after(Duration::from_millis(200)).await;
                }
            }

            AppState::Flappy => {
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
                flappy_game.render(&mut fb);
                if now >= next_flush {
                    fb.flush(&mut display);
                    next_flush = now + Duration::from_millis(33);
                }
                if boot_button.is_low() {
                    app_state = AppState::Launcher;
                    page_dirty = true;
                    Timer::after(Duration::from_millis(200)).await;
                }
            }

            AppState::Maze => {
                let input = AppInput {
                    touch: None,
                    swipe: swipe_event,
                    tap: tap_event,
                    accel,
                    dt_ms: dt_ms.max(1),
                };
                maze_game.update(&input);
                if now >= next_flush {
                    maze_game.render(&mut fb);
                    fb.flush(&mut display);
                    next_flush = now + Duration::from_millis(33);
                }
                if boot_button.is_low() {
                    app_state = AppState::Launcher;
                    Timer::after(Duration::from_millis(200)).await;
                }
            }

            AppState::Settings => {
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
                        wifi_started = false;
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
                settings_app.render(&mut fb);
                if now >= next_flush {
                    fb.flush(&mut display);
                    next_flush = now + Duration::from_millis(50);
                }
                if boot_button.is_low() {
                    app_state = AppState::Launcher;
                    Timer::after(Duration::from_millis(200)).await;
                }
            }

            _ => {
                app_state = AppState::Watchface;
            }
        }
    }
}
