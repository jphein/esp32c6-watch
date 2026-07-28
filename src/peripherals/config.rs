//! Persistent watch configuration in the `config` flash partition.
//!
//! One small fixed-layout record (smol-style: a versioned, checksummed
//! struct, not a filesystem): magic, node id, brightness, WiFi SSID +
//! password. Stored at the start of the `config` (spiffs-subtype) partition;
//! esp-storage's `Storage` impl handles the read-modify-write erase.

use embedded_storage::{ReadStorage, Storage};
use esp_println::println;

/// Byte offset of the BACKUP record slot inside the `config` partition (64KB —
/// the record itself is ~112B at the start). One flash sector (4KB) past the
/// primary so the two live in independent erase units: `save` rewrites primary
/// then backup, so at every instant at least one slot holds a valid record. A
/// freeze/power-loss mid-erase (the single-copy hole: one bad save wiped WiFi
/// creds + theme + the BLE boot bit in one stroke) now costs at most ONE save
/// of history, never the whole config.
const BACKUP_SLOT: u32 = 0x1000;

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
/// v4 record (#46, BLE bit): v3 + one RADIOS FLAGS byte (offset 109), appended
/// before the checksum. Only bit 0 (BLE-on-at-boot) is defined; bits 1..7 are
/// RESERVED for the coordinated #44/#45/#46 persistence migration (mesh/WiFi/
/// mic-gain etc.) so those can land in this same byte WITHOUT another magic
/// bump. Older records still load (flags default 0 = BLE off, the pre-v4
/// behavior); the first save rewrites v4 in place.
const MAGIC_V4: [u8; 6] = *b"SWCFG4";
const REC_LEN_V4: usize = 6 + 1 + 1 + 1 + 32 + 1 + 64 + 1 + 1 + 1 + 1 + 2;
/// v5 record (#46 completion, v0.9.0): v4 + one MIC-GAIN byte (offset 110 — the
/// Sound-app digital-gain step INDEX into `mic_capture::GAIN_STEPS_*`, clamped
/// at apply), appended before the checksum. The flag half of the migration
/// spends v4's reserved radios-flags bits IN PLACE (offset 109): bit 1 mesh-on,
/// bit 2 wifi FORCED-OFF intent, bit 3 touch-sound MUTED — the OFF/muted bits
/// are inverted so a v4 record's zero bits decode to the correct defaults
/// (mesh off · wifi auto · touch sound ON). Older records still load (defaults
/// for every new field); the first save rewrites v5 in place.
const MAGIC_V5: [u8; 6] = *b"SWCFG5";
const REC_LEN_V5: usize = 6 + 1 + 1 + 1 + 32 + 1 + 64 + 1 + 1 + 1 + 1 + 1 + 2;
/// v6 record (#59, v0.12.0): v5 + a VOLUME byte (offset 110... no — 111) and
/// FOUR button-mapping bytes, appended before the checksum.
///   111: volume — bits 0-3 level (0..15), bit 4 = muted (bits 5-7 reserved)
///   112: BOOT short-press action · 113: BOOT long-press action
///   114: PWRON short-press action · 115: PWRON long-press action
/// (each a [`ButtonAction`] u8; unknown values clamp to `None` at load). v5's
/// mic-gain byte at offset 110 is unchanged. Older records still load (volume
/// + mapping take their defaults); the first save rewrites v6 in place.
const MAGIC_V6: [u8; 6] = *b"SWCFG6";
const REC_LEN_V6: usize = 6 + 1 + 1 + 1 + 32 + 1 + 64 + 1 + 1 + 1 + 1 + 1 + 1 + 4 + 2;

/// v7 record (#read-aloud): v6 + a READ-ALOUD MODE byte at offset 116, before
/// the checksum.
///   116: [`SpeakMode`] as u8 — 0 off · 1 on-demand · 2 auto
/// Older records still load and take the default (on-demand), so an OTA never
/// makes a watch start talking on its own; the first save rewrites v7 in place.
const MAGIC_V7: [u8; 6] = *b"SWCFG7";
const REC_LEN_V7: usize = REC_LEN_V6 + 1;

/// Volume byte (offset 111, v6): step level is the low nibble; bit 4 is mute.
const VOL_LEVEL_MASK: u8 = 0x0F;
const VOL_MUTED_BIT: u8 = 0x10;
/// Speaker volume step range (0..=15) — scaled to the ES8311 0x32 register at
/// apply (see `audio_out::vol_to_reg`).
pub const VOL_MAX: u8 = 15;

/// A mapped button action (#59). One per {BOOT,PWRON}×{short,long} slot. `u8`
/// repr so it round-trips through the config record; unknown bytes clamp to
/// `None` on load. Kept in sync with the hub's cycle labels (`action_label`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ButtonAction {
    None,
    VolUp,
    VolDown,
    Mute,
    PowerMenu,
    Shutdown,
    Launcher,
    Ping,
    Voice,
    /// Read the newest notification aloud (#read-aloud). The on-demand
    /// trigger for [`SpeakMode::OnDemand`] until the shade grows a speaker
    /// control — mapping a button needs no .slint change.
    Speak,
}

impl ButtonAction {
    /// Total variants — the cycle-picker wraps through `0..COUNT`.
    pub const COUNT: u8 = 10;

    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Decode a stored byte; anything out of range is a safe `None`.
    pub const fn from_u8(b: u8) -> Self {
        match b {
            1 => ButtonAction::VolUp,
            2 => ButtonAction::VolDown,
            3 => ButtonAction::Mute,
            4 => ButtonAction::PowerMenu,
            5 => ButtonAction::Shutdown,
            6 => ButtonAction::Launcher,
            7 => ButtonAction::Ping,
            8 => ButtonAction::Voice,
            9 => ButtonAction::Speak,
            _ => ButtonAction::None,
        }
    }

    /// Next action in the cycle (hub tap-to-change picker), wrapping.
    pub const fn next(self) -> Self {
        Self::from_u8((self.as_u8() + 1) % Self::COUNT)
    }

    /// Short label for the hub row + the UI push.
    pub const fn label(self) -> &'static str {
        match self {
            ButtonAction::None => "None",
            ButtonAction::VolUp => "Volume +",
            ButtonAction::VolDown => "Volume \u{2212}",
            ButtonAction::Mute => "Mute",
            ButtonAction::PowerMenu => "Power menu",
            ButtonAction::Shutdown => "Power off",
            ButtonAction::Launcher => "Launcher",
            ButtonAction::Ping => "Ping",
            ButtonAction::Voice => "Voice",
            ButtonAction::Speak => "Read aloud",
        }
    }
}

/// Units flags bit 0: 24-hour clock (CFG `U` value `..|24`).
const UNITS_CLK_24H: u8 = 0x01;
/// Units flags bit 1: temperature in Fahrenheit (CFG `U` value `F|..`).
const UNITS_TEMP_F: u8 = 0x02;

/// Radios flags bit 0 (#46): start BLE advertising at boot (persisted toggle —
/// keeps the Bermuda/HA room-tracking registration alive across OTA reboots).
const RADIO_BLE_ON: u8 = 0x01;
/// Radios flags bit 1 (#46 completion): start the SMOLv1 mesh at boot.
const RADIO_MESH_ON: u8 = 0x02;
/// Radios flags bit 2 (#46 completion): WiFi intent FORCED-OFF (inverted so
/// the pre-v5 zero bit keeps the shipped auto-connect boot burst).
const RADIO_WIFI_OFF: u8 = 0x04;
/// Radios flags bit 3 (#49): touch sound MUTED (inverted — default ON).
const TOUCH_SOUND_OFF: u8 = 0x08;

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
    /// Start BLE advertising at boot (#46 BLE bit — persisted watchface
    /// toggle; radios flags bit 0). Default false = the pre-v4 behavior.
    pub ble_on: bool,
    /// Start the SMOLv1 mesh at boot (radios flags bit 1). Default false.
    pub mesh_on: bool,
    /// WiFi intent is FORCED-OFF: skip the credentialed auto-connect boot
    /// burst and ignore the on-demand raises' default-on chrome state (radios
    /// flags bit 2, stored inverted). Default false = auto (pre-v5 behavior).
    pub wifi_off: bool,
    /// Play the subtle tick on every tap (#49; radios flags bit 3, stored
    /// inverted as "muted"). Default TRUE.
    pub touch_sound: bool,
    /// Sound-app digital mic-gain step INDEX into `mic_capture::GAIN_STEPS_*`
    /// (offset 110, v5). Clamped to the table at apply. Default 0 (= 0 dB).
    pub mic_gain: u8,
    /// Speaker volume step 0..=[`VOL_MAX`] (offset 111 low nibble, v6). Scaled
    /// to the ES8311 master-volume register at apply. Default 11 (≈ the old
    /// hard-coded ~80% unmute level).
    pub volume: u8,
    /// Speaker muted (offset 111 bit 4, v6). Independent of the step level so
    /// unmuting restores the prior volume. Default false.
    pub muted: bool,
    /// Button map (#59, v6): BOOT/PWRON × short/long → action. Defaults give
    /// BOOT tap = Volume+, BOOT hold = Launcher, POWER tap = Volume−, POWER
    /// hold = Power menu.
    pub boot_short: ButtonAction,
    pub boot_long: ButtonAction,
    pub pwron_short: ButtonAction,
    pub pwron_long: ButtonAction,
    /// Read notifications aloud (#read-aloud, v7). Defaults to on-demand.
    pub speak: SpeakMode,
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
            ble_on: false,
            mesh_on: false,
            wifi_off: false,
            touch_sound: true,
            mic_gain: 0,
            volume: 11,
            muted: false,
            boot_short: ButtonAction::VolUp,
            boot_long: ButtonAction::Launcher,
            pwron_short: ButtonAction::VolDown,
            pwron_long: ButtonAction::PowerMenu,
            // On-demand, not Auto — see SpeakMode's docs for why.
            speak: SpeakMode::OnDemand,
        }
    }
}

/// Read notifications aloud through the TTS bridge (#read-aloud).
///
/// Default is [`OnDemand`](SpeakMode::OnDemand), deliberately not `Auto`:
/// speaking an utterance parks the main loop for seconds (a notification
/// arriving mid-game would freeze a framebuffer app), the watch is worn in
/// rooms with other people, and the ping chime already provides the ambient
/// "something arrived" cue. Speech should be asked for, not assumed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SpeakMode {
    /// Never speak. The TTS path is inert.
    Off = 0,
    /// Speak only when the user asks (speaker control on the notification card).
    OnDemand = 1,
    /// Speak on arrival — but only when every gate in `should_auto_speak` holds
    /// (screen on · watchface/shade only · not muted · nothing already speaking).
    Auto = 2,
}

impl SpeakMode {
    pub const fn from_u8(b: u8) -> Self {
        match b {
            0 => SpeakMode::Off,
            2 => SpeakMode::Auto,
            // Unknown/corrupt values fall back to the default rather than to
            // Auto — an unreadable byte must never make the watch talk.
            _ => SpeakMode::OnDemand,
        }
    }
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
    /// May a notification be spoken at all in this mode?
    pub const fn enabled(self) -> bool {
        !matches!(self, SpeakMode::Off)
    }
}

fn checksum(buf: &[u8]) -> u16 {
    buf.iter().map(|&b| b as u16).fold(0u16, u16::wrapping_add)
}

/// Load the config: primary slot first, then the backup mirror (a torn primary
/// write — freeze/power-loss mid-save — falls back to the previous save's
/// values instead of factory defaults). The next `save` re-heals both slots.
pub fn load(flash: &mut impl ReadStorage, offset: u32) -> Option<WatchConfig> {
    if let Some(cfg) = load_slot(flash, offset) {
        return Some(cfg);
    }
    let fallback = load_slot(flash, offset + BACKUP_SLOT);
    if fallback.is_some() {
        println!("[CFG] primary record invalid - recovered from backup slot");
    }
    fallback
}

fn load_slot(flash: &mut impl ReadStorage, offset: u32) -> Option<WatchConfig> {
    let mut buf = [0u8; REC_LEN_V7];
    flash.read(offset, &mut buf).ok()?;
    // v2plus = has default_page + units (v2+); v3plus = also the theme byte;
    // v4plus = also the radios flags byte; v5plus = also the mic-gain byte;
    // v6plus = also the volume + button-map bytes; v7 = also the speak byte.
    let (rec_len, v2plus, v3plus, v4plus, v5plus, v6plus, v7) = if buf[..6] == MAGIC_V7 {
        (REC_LEN_V7, true, true, true, true, true, true)
    } else if buf[..6] == MAGIC_V6 {
        (REC_LEN_V6, true, true, true, true, true, false)
    } else if buf[..6] == MAGIC_V5 {
        (REC_LEN_V5, true, true, true, true, false, false)
    } else if buf[..6] == MAGIC_V4 {
        (REC_LEN_V4, true, true, true, false, false, false)
    } else if buf[..6] == MAGIC_V3 {
        (REC_LEN_V3, true, true, false, false, false, false)
    } else if buf[..6] == MAGIC_V2 {
        (REC_LEN_V2, true, false, false, false, false, false)
    } else if buf[..6] == MAGIC_V1 {
        (REC_LEN_V1, false, false, false, false, false, false)
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
    let theme = if v3plus { buf[108].min(3) } else { defaults.theme };
    // Radios flags (offset 109): bits 1..3 were RESERVED-as-zero in v4, so a
    // v4 record decodes them straight to the defaults (mesh off · wifi auto ·
    // touch sound on) — no magic bump needed for the flag half of v5.
    let (ble_on, mesh_on, wifi_off, touch_sound) = if v4plus {
        let flags = buf[109];
        (
            flags & RADIO_BLE_ON != 0,
            flags & RADIO_MESH_ON != 0,
            flags & RADIO_WIFI_OFF != 0,
            flags & TOUCH_SOUND_OFF == 0,
        )
    } else {
        (
            defaults.ble_on,
            defaults.mesh_on,
            defaults.wifi_off,
            defaults.touch_sound,
        )
    };
    let mic_gain = if v5plus { buf[110] } else { defaults.mic_gain };
    let (volume, muted, boot_short, boot_long, pwron_short, pwron_long) = if v6plus {
        let v = buf[111];
        (
            (v & VOL_LEVEL_MASK).min(VOL_MAX),
            v & VOL_MUTED_BIT != 0,
            ButtonAction::from_u8(buf[112]),
            ButtonAction::from_u8(buf[113]),
            ButtonAction::from_u8(buf[114]),
            ButtonAction::from_u8(buf[115]),
        )
    } else {
        (
            defaults.volume,
            defaults.muted,
            defaults.boot_short,
            defaults.boot_long,
            defaults.pwron_short,
            defaults.pwron_long,
        )
    };
    // v7: read-aloud mode. Pre-v7 records take the default (on-demand) so an
    // OTA can never turn a quiet watch into a talking one.
    let speak = if v7 { SpeakMode::from_u8(buf[116]) } else { defaults.speak };
    Some(WatchConfig {
        node_id,
        brightness,
        ssid,
        pass,
        default_page,
        units_temp_f,
        units_clk_24h,
        theme,
        ble_on,
        mesh_on,
        wifi_off,
        touch_sound,
        mic_gain,
        volume,
        muted,
        boot_short,
        boot_long,
        pwron_short,
        pwron_long,
        speak,
    })
}

/// Save the config to BOTH slots, primary first. Ordering is the atomicity:
/// while the primary sector is being erased/rewritten the backup still holds
/// the previous valid record, and vice versa — a freeze at any point leaves at
/// least one loadable slot. `Ok` = the primary landed; the backup mirror is
/// best-effort (its failure is logged, not fatal — the primary already holds
/// the new record).
pub fn save(flash: &mut impl Storage, offset: u32, cfg: &WatchConfig) -> Result<(), ()> {
    let res = save_slot(flash, offset, cfg);
    if res.is_ok() && save_slot(flash, offset + BACKUP_SLOT, cfg).is_err() {
        println!("[CFG] backup-slot mirror write failed (primary OK)");
    }
    res
}

fn save_slot(flash: &mut impl Storage, offset: u32, cfg: &WatchConfig) -> Result<(), ()> {
    let mut buf = [0u8; REC_LEN_V7];
    buf[..6].copy_from_slice(&MAGIC_V7);
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
    // Radios flags (offset 109): bit 0 BLE · bit 1 mesh · bit 2 wifi
    // forced-off · bit 3 touch-sound muted; bits 4..7 reserved (MAGIC_V5 docs).
    buf[109] = (if cfg.ble_on { RADIO_BLE_ON } else { 0 })
        | (if cfg.mesh_on { RADIO_MESH_ON } else { 0 })
        | (if cfg.wifi_off { RADIO_WIFI_OFF } else { 0 })
        | (if cfg.touch_sound { 0 } else { TOUCH_SOUND_OFF });
    // Mic-gain step index (offset 110, v5) — clamped again at apply.
    buf[110] = cfg.mic_gain;
    // Volume (offset 111, v6): level nibble + mute bit.
    buf[111] = (cfg.volume.min(VOL_MAX) & VOL_LEVEL_MASK)
        | (if cfg.muted { VOL_MUTED_BIT } else { 0 });
    // Button map (offsets 112-115, v6).
    buf[112] = cfg.boot_short.as_u8();
    buf[113] = cfg.boot_long.as_u8();
    buf[114] = cfg.pwron_short.as_u8();
    buf[115] = cfg.pwron_long.as_u8();
    // Read-aloud mode (offset 116, v7).
    buf[116] = cfg.speak.as_u8();
    let sum = checksum(&buf[..REC_LEN_V7 - 2]);
    buf[REC_LEN_V7 - 2..].copy_from_slice(&sum.to_le_bytes());
    flash.write(offset, &buf).map_err(|_| ())
}
