//! WLED WiZmote linked-remote frame encoder (smol #25 port).
//!
//! Impersonates a WLED "WiZmote" ESP-NOW remote: broadcast the 13-byte WiZmote
//! frame and a WLED controller that has this device's MAC set as its "Linked
//! Remote" reacts — on / off / preset / dim / nightlight. Pure + panic-free +
//! no_std (host-testable); the firmware later broadcasts the returned bytes via
//! `esp_now.send(BROADCAST, &frame)` (that wiring is deferred).
//!
//! Pairing (WLED 0.14+): Config → Sync Interfaces → enable "ESP-NOW", set
//! "Linked Remote" MAC = this device's ESP-NOW/STA MAC.
#![cfg_attr(not(test), no_std)]

/// A WiZmote button. `Preset(n)` is clamped to 1..=4 at encode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WledButton {
    On,
    Off,
    Night,
    BrightUp,
    BrightDown,
    Preset(u8),
}

impl WledButton {
    /// `(program_byte, button_code)` per WLED `remote.cpp` `WizMoteMessageStructure`.
    /// `program` is `0x91` for ON, `0x81` for everything else; button codes:
    /// `ON=1, OFF=2, NIGHT=3, BRIGHT_DOWN=8, BRIGHT_UP=9`, presets = `15 + n`
    /// (1→16 .. 4→19).
    const fn codes(self) -> (u8, u8) {
        match self {
            WledButton::On => (0x91, 1),
            WledButton::Off => (0x81, 2),
            WledButton::Night => (0x81, 3),
            WledButton::BrightDown => (0x81, 8),
            WledButton::BrightUp => (0x81, 9),
            // clamp 1..=4 then +15 → 16..=19; saturating so no overflow/panic.
            WledButton::Preset(n) => (
                0x81,
                15u8.saturating_add(if n < 1 {
                    1
                } else if n > 4 {
                    4
                } else {
                    n
                }),
            ),
        }
    }
}

/// Encode the 13-byte WiZmote frame. Fixed array literal — no alloc, no runtime
/// indexing on external data → total / panic-free. Layout:
/// `program | seq[4] LE | dt1=0x20 | button | dt2=0x01 | batLevel | 0 0 0 0`.
///
/// `seq` should increment per emit (WLED de-dups repeats by sequence); `bat_level`
/// is the remote's battery 0..=100 (cosmetic in WLED), clamped here.
pub fn encode_wizmote(btn: WledButton, seq: u32, bat_level: u8) -> [u8; 13] {
    let (program, button) = btn.codes();
    let s = seq.to_le_bytes(); // LSB-first
    [
        program, s[0], s[1], s[2], s[3], 0x20, button, 0x01, bat_level.min(100), 0, 0, 0, 0,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn programs_and_button_codes() {
        assert_eq!(WledButton::On.codes(), (0x91, 1));
        assert_eq!(WledButton::Off.codes(), (0x81, 2));
        assert_eq!(WledButton::Night.codes(), (0x81, 3));
        assert_eq!(WledButton::BrightDown.codes(), (0x81, 8));
        assert_eq!(WledButton::BrightUp.codes(), (0x81, 9));
    }

    #[test]
    fn presets_map_16_to_19_and_clamp() {
        assert_eq!(WledButton::Preset(1).codes(), (0x81, 16));
        assert_eq!(WledButton::Preset(2).codes(), (0x81, 17));
        assert_eq!(WledButton::Preset(3).codes(), (0x81, 18));
        assert_eq!(WledButton::Preset(4).codes(), (0x81, 19));
        // out-of-range clamps into 1..=4 → 16..=19, never panics.
        assert_eq!(WledButton::Preset(0).codes(), (0x81, 16));
        assert_eq!(WledButton::Preset(9).codes(), (0x81, 19));
    }

    #[test]
    fn frame_layout_and_seq_le() {
        let f = encode_wizmote(WledButton::On, 0x04030201, 100);
        assert_eq!(f.len(), 13);
        assert_eq!(f[0], 0x91); // program (ON)
        assert_eq!(&f[1..5], &[0x01, 0x02, 0x03, 0x04]); // seq little-endian
        assert_eq!(f[5], 0x20); // dt1
        assert_eq!(f[6], 1); // button (ON)
        assert_eq!(f[7], 0x01); // dt2
        assert_eq!(f[8], 100); // battery
        assert_eq!(&f[9..13], &[0, 0, 0, 0]); // tail
    }

    #[test]
    fn battery_clamped_to_100() {
        let f = encode_wizmote(WledButton::Off, 0, 250);
        assert_eq!(f[0], 0x81);
        assert_eq!(f[6], 2);
        assert_eq!(f[8], 100); // 250 → 100
    }

    #[test]
    fn seq_wraps_cleanly() {
        let f = encode_wizmote(WledButton::Preset(3), u32::MAX, 50);
        assert_eq!(&f[1..5], &[0xff, 0xff, 0xff, 0xff]);
        assert_eq!(f[6], 18); // preset 3 → 15+3
        assert_eq!(f[8], 50);
    }
}
