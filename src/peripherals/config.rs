//! Persistent watch configuration in the `config` flash partition.
//!
//! One small fixed-layout record (smol-style: a versioned, checksummed
//! struct, not a filesystem): magic, node id, brightness, WiFi SSID +
//! password. Stored at the start of the `config` (spiffs-subtype) partition;
//! esp-storage's `Storage` impl handles the read-modify-write erase.

use embedded_storage::{ReadStorage, Storage};
use esp_storage::FlashStorage;

/// v1 record: node id, brightness, WiFi creds.
const MAGIC_V1: [u8; 6] = *b"SWCFG1";
const REC_LEN_V1: usize = 6 + 1 + 1 + 1 + 32 + 1 + 64 + 2;
/// v2 record (SMOLv1 CFG channel, keys `S`/`U`): v1 + default watchface page
/// + display-units flags, appended before the checksum. A v1 record still
/// loads (defaults for the new fields), so stored WiFi creds survive the
/// upgrade; the first save rewrites it as v2 in place.
const MAGIC_V2: [u8; 6] = *b"SWCFG2";
const REC_LEN_V2: usize = 6 + 1 + 1 + 1 + 32 + 1 + 64 + 1 + 1 + 2;
/// v3 record: v2 + theme scheme byte (0..3), appended before the checksum. A
/// v1/v2 record still loads (theme takes the default — 2 = Amber), so WiFi creds
/// + page + units survive the upgrade; the first save rewrites it as v3 in place.
const MAGIC_V3: [u8; 6] = *b"SWCFG3";
const REC_LEN_V3: usize = 6 + 1 + 1 + 1 + 32 + 1 + 64 + 1 + 1 + 1 + 2;

/// Units flags bit 0: 24-hour clock (CFG `U` value `..|24`).
const UNITS_CLK_24H: u8 = 0x01;
/// Units flags bit 1: temperature in Fahrenheit (CFG `U` value `F|..`).
const UNITS_TEMP_F: u8 = 0x02;

pub struct WatchConfig {
    pub node_id: u8,
    pub brightness: u8,
    pub ssid: heapless::String<32>,
    pub pass: heapless::String<64>,
    /// Boot default watchface page (CFG key `S`), clamped 0..=3 at apply.
    pub default_page: u8,
    /// °F (vs °C) display temperature (CFG key `U`).
    pub units_temp_f: bool,
    /// 24-hour (vs 12-hour) clock (CFG key `U`).
    pub units_clk_24h: bool,
    /// Active theme scheme: 0 Midnight · 1 Paper · 2 Amber · 3 Violet.
    pub theme: u8,
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            node_id: 42,
            brightness: 0xD0,
            ssid: heapless::String::new(),
            pass: heapless::String::new(),
            default_page: 0,
            // Fleet defaults (smol units.rs `Units::default`): °F + 12h.
            units_temp_f: true,
            units_clk_24h: false,
            // Amber by default (JP 2026-07-23). Applies to fresh devices AND
            // v1/v2 records (which lack the theme byte and take this default);
            // an explicit picker choice persists as v3 and wins.
            theme: 2,
        }
    }
}

fn checksum(buf: &[u8]) -> u16 {
    buf.iter().map(|&b| b as u16).fold(0u16, u16::wrapping_add)
}

pub fn load(flash: &mut FlashStorage<'_>, offset: u32) -> Option<WatchConfig> {
    let mut buf = [0u8; REC_LEN_V3];
    flash.read(offset, &mut buf).ok()?;
    // v2plus = has default_page + units (v2 & v3); v3 = also has the theme byte.
    let (rec_len, v2plus, v3) = if buf[..6] == MAGIC_V3 {
        (REC_LEN_V3, true, true)
    } else if buf[..6] == MAGIC_V2 {
        (REC_LEN_V2, true, false)
    } else if buf[..6] == MAGIC_V1 {
        (REC_LEN_V1, false, false)
    } else {
        return None;
    };
    let stored = u16::from_le_bytes([buf[rec_len - 2], buf[rec_len - 1]]);
    if stored != checksum(&buf[..rec_len - 2]) {
        return None;
    }
    let node_id = buf[6];
    let brightness = buf[7];
    let ssid_len = (buf[8] as usize).min(32);
    let ssid_bytes = &buf[9..9 + ssid_len];
    let pass_len = (buf[41] as usize).min(64);
    let pass_bytes = &buf[42..42 + pass_len];
    let mut ssid = heapless::String::new();
    let _ = ssid.push_str(core::str::from_utf8(ssid_bytes).unwrap_or(""));
    let mut pass = heapless::String::new();
    let _ = pass.push_str(core::str::from_utf8(pass_bytes).unwrap_or(""));
    let defaults = WatchConfig::default();
    let (default_page, units_temp_f, units_clk_24h) = if v2plus {
        let flags = buf[107];
        (
            buf[106].min(3),
            flags & UNITS_TEMP_F != 0,
            flags & UNITS_CLK_24H != 0,
        )
    } else {
        (
            defaults.default_page,
            defaults.units_temp_f,
            defaults.units_clk_24h,
        )
    };
    let theme = if v3 { buf[108].min(3) } else { defaults.theme };
    Some(WatchConfig {
        node_id,
        brightness,
        ssid,
        pass,
        default_page,
        units_temp_f,
        units_clk_24h,
        theme,
    })
}

pub fn save(flash: &mut FlashStorage<'_>, offset: u32, cfg: &WatchConfig) -> Result<(), ()> {
    let mut buf = [0u8; REC_LEN_V3];
    buf[..6].copy_from_slice(&MAGIC_V3);
    buf[6] = cfg.node_id;
    buf[7] = cfg.brightness;
    let sb = cfg.ssid.as_bytes();
    buf[8] = sb.len().min(32) as u8;
    buf[9..9 + sb.len().min(32)].copy_from_slice(&sb[..sb.len().min(32)]);
    let pb = cfg.pass.as_bytes();
    buf[41] = pb.len().min(64) as u8;
    buf[42..42 + pb.len().min(64)].copy_from_slice(&pb[..pb.len().min(64)]);
    buf[106] = cfg.default_page.min(3);
    buf[107] = (if cfg.units_clk_24h { UNITS_CLK_24H } else { 0 })
        | (if cfg.units_temp_f { UNITS_TEMP_F } else { 0 });
    buf[108] = cfg.theme.min(3);
    let sum = checksum(&buf[..REC_LEN_V3 - 2]);
    buf[REC_LEN_V3 - 2..].copy_from_slice(&sum.to_le_bytes());
    flash.write(offset, &buf).map_err(|_| ())
}
