//! Persistent watch configuration in the `config` flash partition.
//!
//! One small fixed-layout record (smol-style: a versioned, checksummed
//! struct, not a filesystem): magic, node id, brightness, WiFi SSID +
//! password. Stored at the start of the `config` (spiffs-subtype) partition;
//! esp-storage's `Storage` impl handles the read-modify-write erase.

use embedded_storage::{ReadStorage, Storage};
use esp_storage::FlashStorage;

const MAGIC: [u8; 6] = *b"SWCFG1";
const REC_LEN: usize = 6 + 1 + 1 + 1 + 32 + 1 + 64 + 2;

pub struct WatchConfig {
    pub node_id: u8,
    pub brightness: u8,
    pub ssid: heapless::String<32>,
    pub pass: heapless::String<64>,
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            node_id: 42,
            brightness: 0xD0,
            ssid: heapless::String::new(),
            pass: heapless::String::new(),
        }
    }
}

fn checksum(buf: &[u8]) -> u16 {
    buf.iter().map(|&b| b as u16).fold(0u16, u16::wrapping_add)
}

pub fn load(flash: &mut FlashStorage<'_>, offset: u32) -> Option<WatchConfig> {
    let mut buf = [0u8; REC_LEN];
    flash.read(offset, &mut buf).ok()?;
    if buf[..6] != MAGIC {
        return None;
    }
    let stored = u16::from_le_bytes([buf[REC_LEN - 2], buf[REC_LEN - 1]]);
    if stored != checksum(&buf[..REC_LEN - 2]) {
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
    Some(WatchConfig {
        node_id,
        brightness,
        ssid,
        pass,
    })
}

pub fn save(flash: &mut FlashStorage<'_>, offset: u32, cfg: &WatchConfig) -> Result<(), ()> {
    let mut buf = [0u8; REC_LEN];
    buf[..6].copy_from_slice(&MAGIC);
    buf[6] = cfg.node_id;
    buf[7] = cfg.brightness;
    let sb = cfg.ssid.as_bytes();
    buf[8] = sb.len().min(32) as u8;
    buf[9..9 + sb.len().min(32)].copy_from_slice(&sb[..sb.len().min(32)]);
    let pb = cfg.pass.as_bytes();
    buf[41] = pb.len().min(64) as u8;
    buf[42..42 + pb.len().min(64)].copy_from_slice(&pb[..pb.len().min(64)]);
    let sum = checksum(&buf[..REC_LEN - 2]);
    buf[REC_LEN - 2..].copy_from_slice(&sum.to_le_bytes());
    flash.write(offset, &buf).map_err(|_| ())
}
