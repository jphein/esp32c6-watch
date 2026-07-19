//! Power consumption estimator and runtime budget tracker.
//!
//! This is intentionally a very lightweight module. The goal is to give the
//! user a live, honest-ish read-out of where their milliamperes are going
//! WITHOUT the diagnostic itself becoming a major drain. All state is plain
//! POD that we update from the main loop in-place — no extra timers, no extra
//! I2C polling beyond what the firmware already does, no additional tasks.
//!
//! The mA numbers are rough typicals at 3.7 V nominal, taken from:
//!   * ESP32-S3 datasheet (§5 Electrical Characteristics)
//!   * CO5300 AMOLED panel reference + AMOLED curve (current ~ luminance)
//!   * AXP2101 + QMI8658 + FT3168 + ES8311 datasheets
//!   * empirical measurements from the Waveshare C reference firmware
//!
//! They are *estimates* displayed for diagnosis, not precise telemetry.
//! For real numbers, use a USB power meter or tap the battery lead.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayState {
    Off,      //   0 mA  (AMOLED fully off + MCU in sleep/idle)
    Aod,      //  ~8 mA  (minimal HH:MM at ~10% brightness on black)
    Dim,      // ~25 mA  (mid brightness)
    Bright,   // ~70 mA  (full bright, typical UI content)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WifiMode {
    Off,          //   0 mA
    PowerSave,    // ~20 mA  (STA connected, DTIM sleep between beacons)
    Active,       // ~90 mA  (actively TX/RX, no PS or during handshake)
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PowerStats {
    pub display: Option<DisplayState>,
    pub wifi: Option<WifiMode>,
    pub ble_on: bool,        // BLE advertising / connected
    pub imu_on: bool,
    pub audio_on: bool,      // codec + PA amplifier
    pub sd_on: bool,         // SD card active / inserted+powered
    pub cpu_mhz: u16,        // current CPU clock (reported by main.rs)
    pub brightness: u8,      // 0x00..0xFF display brightness
    pub battery_mv: u16,
    pub battery_pct: u8,
    pub charging: bool,
}

impl PowerStats {
    pub const fn new() -> Self {
        Self {
            display: None,
            wifi: None,
            ble_on: false,
            imu_on: false,
            audio_on: false,
            sd_on: false,
            cpu_mhz: 160,
            brightness: 0xA0,
            battery_mv: 0,
            battery_pct: 0,
            charging: false,
        }
    }

    /// Baseline ESP32-S3 + PSRAM + AXP2101 housekeeping, in mA.
    /// Scales roughly linearly with CPU frequency.
    pub fn base_ma(&self) -> u16 {
        // 240 MHz ≈ 55 mA, 160 MHz ≈ 38 mA, 80 MHz ≈ 22 mA, 40 MHz ≈ 14 mA.
        match self.cpu_mhz {
            240 => 55,
            160 => 38,
            80 => 22,
            40 => 14,
            _ => 38,
        }
    }

    pub fn display_ma(&self) -> u16 {
        match self.display {
            None | Some(DisplayState::Off) => 0,
            Some(DisplayState::Aod) => 8,
            Some(DisplayState::Dim) => 25,
            Some(DisplayState::Bright) => {
                // AMOLED current scales roughly linearly with brightness.
                // 0xFF ≈ 80 mA, 0x00 ≈ 5 mA (panel logic still on).
                5 + (self.brightness as u16 * 75 / 255)
            }
        }
    }

    pub fn wifi_ma(&self) -> u16 {
        match self.wifi {
            None | Some(WifiMode::Off) => 0,
            Some(WifiMode::PowerSave) => 20,
            Some(WifiMode::Active) => 90,
        }
    }

    pub fn ble_ma(&self) -> u16 { if self.ble_on { 15 } else { 0 } }
    pub fn imu_ma(&self) -> u16 { if self.imu_on { 2 } else { 0 } }
    pub fn audio_ma(&self) -> u16 { if self.audio_on { 25 } else { 0 } }
    pub fn sd_ma(&self) -> u16 { if self.sd_on { 30 } else { 0 } }

    pub fn total_ma(&self) -> u16 {
        self.base_ma()
            + self.display_ma()
            + self.wifi_ma()
            + self.ble_ma()
            + self.imu_ma()
            + self.audio_ma()
            + self.sd_ma()
    }

    /// Full-charge runtime (100%→0%) in hours at the current load.
    pub fn full_runtime_hours(&self, capacity_mah: u16) -> u16 {
        let load = self.total_ma().max(1);
        ((capacity_mah as u32 / load as u32) as u16).min(999)
    }

    /// Remaining runtime in hours based on the actual battery percentage.
    pub fn estimated_hours(&self, capacity_mah: u16) -> u16 {
        let load = self.total_ma().max(1);
        let usable = (capacity_mah as u32) * (self.battery_pct as u32) / 100;
        ((usable / load as u32) as u16).min(999)
    }
}
