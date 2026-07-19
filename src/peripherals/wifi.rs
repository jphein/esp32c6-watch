// WiFi module - async connection + NTP time sync
// Uses esp-radio + embassy-net

// NOTE: WiFi initialization requires significant resources:
// - ~300KB heap for WiFi buffers
// - Timer group for radio stack
// - Dedicated async tasks for network processing
//
// WiFi will be initialized on-demand (from Settings page)
// not at boot, to save power and memory.

pub struct WifiConfig {
    pub ssid: [u8; 32],
    pub ssid_len: usize,
    pub password: [u8; 64],
    pub pass_len: usize,
}

impl WifiConfig {
    pub fn new() -> Self {
        Self {
            ssid: [0; 32], ssid_len: 0,
            password: [0; 64], pass_len: 0,
        }
    }

    pub fn set_ssid(&mut self, s: &str) {
        let bytes = s.as_bytes();
        let len = bytes.len().min(32);
        self.ssid[..len].copy_from_slice(&bytes[..len]);
        self.ssid_len = len;
    }

    pub fn set_password(&mut self, p: &str) {
        let bytes = p.as_bytes();
        let len = bytes.len().min(64);
        self.password[..len].copy_from_slice(&bytes[..len]);
        self.pass_len = len;
    }

    pub fn ssid_str(&self) -> &str {
        core::str::from_utf8(&self.ssid[..self.ssid_len]).unwrap_or("")
    }
}

// WiFi connection state
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum WifiState {
    Disconnected,
    Connecting,
    Connected,
    Error,
}

/// Scan results (SSID + signal strength)
pub struct ScanResult {
    pub ssid: [u8; 32],
    pub ssid_len: usize,
    pub rssi: i8,
}

impl ScanResult {
    pub fn ssid_str(&self) -> &str {
        core::str::from_utf8(&self.ssid[..self.ssid_len]).unwrap_or("?")
    }
}

// TODO: Implement actual WiFi scan using esp-radio wifi_controller.scan()
// The full implementation requires:
// 1. esp_radio::init_wifi() with RNG + radio clocks
// 2. embassy_net::Stack with DHCP config
// 3. Spawned network task for packet processing
// 4. NTP client for time sync
//
// This will be wired up when we add a Settings page with WiFi config UI.
