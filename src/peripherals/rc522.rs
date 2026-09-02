//! smol #540: the scry station's MFRC522 NFC reader — poll/debounce driver.
//!
//! Hardware: an MFRC522 (in practice FM175xx clone silicon — version reads
//! 0x82) on the ES3C28P's P3 expanded-IO jack, SPI3 via the GPIO matrix on
//! the board's free-pool pins (`board::RC522_*`: SCK=14 MOSI=21 MISO=2 CS=3),
//! 5 MHz mode 0, RST tied high (soft reset over SPI), IRQ unconnected —
//! polled. Full harness doc: `labels/scry/rc522-s3cyd-wiring.md`; wiring
//! proven by the labels spike (version-register read over the real harness,
//! 2026-09-01).
//!
//! ## Behavior (#540's contract)
//! Poll WUPA at [`POLL_MS`] cadence — WUPA, not REQA, because a card we HLTA'd
//! after reading stays in HALT while it sits on the pad, and REQA cannot see a
//! halted card: presence tracking would read "removed" for a card that never
//! moved. One [`Tap`] event per tag-presence: re-arms when the tag leaves the
//! field ([`ABSENT_POLLS_CLEAR`] consecutive misses) or after [`REARM_MS`] of
//! continuous presence. UID only, uppercase colon-hex (the `/tap/<UID>` wire
//! shape) — NDEF is the phones' business, the station needs identity.
//!
//! ## Dark-degrade
//! Construction NEVER fails the boot: a version-register sanity miss (reader
//! unplugged, harness fault) logs once and leaves the driver inert, the
//! `ws2812.rs` posture — a scry build on reader-less hardware still runs.

use embedded_hal_bus::spi::ExclusiveDevice;
use esp_hal::delay::Delay;
use esp_hal::gpio::{Level, Output, OutputConfig};
use esp_hal::peripherals::{GPIO14, GPIO2, GPIO21, GPIO3, SPI3};
use esp_hal::spi::master::{Config as SpiConfig, Spi};
use esp_hal::spi::Mode;
use esp_hal::time::Rate;
use esp_hal::Blocking;
use esp_println::println;
use mfrc522::comm::blocking::spi::{DummyDelay, SpiInterface};
use mfrc522::{Initialized, Mfrc522};

/// Poll cadence. 6–7 Hz sits inside #540's 5–10 Hz ask; a WUPA round trip is
/// sub-ms at 5 MHz, so the cost is invisible to the main loop.
const POLL_MS: u64 = 150;
/// Consecutive missed polls before the tag counts as REMOVED (re-arms the
/// event). 4 × 150 ms — long enough to ride out a single anticollision
/// hiccup, short enough that lift-and-retap feels instant.
const ABSENT_POLLS_CLEAR: u8 = 4;
/// A tag parked on the pad re-fires after this long (#540: "re-arm on removal
/// or 30 s").
const REARM_MS: u64 = 30_000;
/// Version-register values accepted by the sanity check: genuine MFRC522
/// (0x91/0x92), older NXP (0x88/0x90), and the FM175xx clone family (0x82 —
/// what JP's unit actually reads; labels spike verdict).
const KNOWN_VERSIONS: [u8; 5] = [0x82, 0x88, 0x90, 0x91, 0x92];

/// Longest ISO14443A UID is 10 bytes ("triple size").
const UID_MAX: usize = 10;
/// "AA:BB:…" for 10 bytes = 29 chars; 32 rounds up.
pub const UID_STR_CAP: usize = 32;

type Reader = Mfrc522<
    SpiInterface<ExclusiveDevice<Spi<'static, Blocking>, Output<'static>, Delay>, DummyDelay>,
    Initialized,
>;

/// One debounced tap: the UID in the `/tap/<UID>` wire shape
/// (uppercase colon-hex).
pub struct Tap {
    pub uid: heapless::String<UID_STR_CAP>,
}

pub struct Scry {
    /// `None` = dark-degraded (no reader answered the sanity check).
    reader: Option<Reader>,
    /// The tag currently in the field (raw UID + length), if any.
    current: Option<([u8; UID_MAX], usize)>,
    /// When the CURRENT presence last fired an event (for the 30 s re-arm).
    fired_at_ms: u64,
    /// Consecutive polls with no tag answering.
    misses: u8,
    last_poll_ms: u64,
}

impl Scry {
    /// Build SPI3 on the P3 pins and probe the reader. The construction is
    /// the labels spike's, verbatim — it is the metal-proven pairing of
    /// mfrc522 0.8 / embedded-hal-bus 0.3 / esp-hal 1.1.x on this harness.
    pub fn new(
        spi3: SPI3<'static>,
        sck: GPIO14<'static>,
        mosi: GPIO21<'static>,
        miso: GPIO2<'static>,
        cs: GPIO3<'static>,
    ) -> Self {
        let spi = Spi::new(
            spi3,
            SpiConfig::default()
                .with_frequency(Rate::from_mhz(5))
                .with_mode(Mode::_0),
        )
        .expect("SPI3 config is static and valid")
        .with_sck(sck)
        .with_mosi(mosi)
        .with_miso(miso);
        let cs = Output::new(cs, Level::High, OutputConfig::default());
        let dev = ExclusiveDevice::new(spi, cs, Delay::new())
            .expect("CS pin can never be busy at construction");
        let reader = match Mfrc522::new(SpiInterface::new(dev)).init() {
            Ok(mut r) => match r.version() {
                Ok(v) if KNOWN_VERSIONS.contains(&v) => {
                    println!("[SCRY] RC522 version {v:#04x} - reader up");
                    Some(r)
                }
                Ok(v) => {
                    // 0x00/0xFF = floating MISO (nothing plugged into P3).
                    println!("[SCRY] RC522 version {v:#04x} unknown - scry inert (reader absent/miswired?)");
                    None
                }
                Err(_) => {
                    println!("[SCRY] RC522 version read failed - scry inert");
                    None
                }
            },
            Err(_) => {
                println!("[SCRY] RC522 init failed - scry inert");
                None
            }
        };
        Self {
            reader,
            current: None,
            fired_at_ms: 0,
            misses: 0,
            last_poll_ms: 0,
        }
    }

    /// True when the sanity check found a live reader at boot.
    pub fn present(&self) -> bool {
        self.reader.is_some()
    }

    /// One poll step; call every main-loop tick, it self-paces to [`POLL_MS`].
    /// Returns a [`Tap`] on the debounced edge (new tag, tag re-tapped after
    /// removal, or the same tag after [`REARM_MS`] of continuous presence).
    pub fn service(&mut self, now_ms: u64) -> Option<Tap> {
        let reader = self.reader.as_mut()?;
        if now_ms.saturating_sub(self.last_poll_ms) < POLL_MS {
            return None;
        }
        self.last_poll_ms = now_ms;

        // WUPA (not REQA — see the module doc): wakes IDLE and HALT alike.
        let Ok(atqa) = reader.wupa() else {
            // No tag answered. Debounced removal → re-arm.
            self.misses = self.misses.saturating_add(1);
            if self.misses >= ABSENT_POLLS_CLEAR && self.current.is_some() {
                self.current = None;
            }
            return None;
        };
        self.misses = 0;
        let Ok(uid) = reader.select(&atqa) else {
            // Anticollision fell over mid-read (tag leaving the field, or two
            // tags) — not a miss for debounce purposes, just no event.
            return None;
        };
        let bytes = uid.as_bytes();
        let mut raw = [0u8; UID_MAX];
        let n = bytes.len().min(UID_MAX);
        raw[..n].copy_from_slice(&bytes[..n]);
        // Put the card back to HALT so it stops answering anticollision while
        // parked; WUPA still sees it next poll (presence keeps tracking).
        let _ = reader.hlta();

        let same = self.current == Some((raw, n));
        if same && now_ms.saturating_sub(self.fired_at_ms) < REARM_MS {
            return None; // one event per presence
        }
        self.current = Some((raw, n));
        self.fired_at_ms = now_ms;

        let mut s: heapless::String<UID_STR_CAP> = heapless::String::new();
        for (i, b) in raw[..n].iter().enumerate() {
            use core::fmt::Write as _;
            let _ = write!(s, "{}{:02X}", if i == 0 { "" } else { ":" }, b);
        }
        println!("[SCRY] tap uid={s}");
        Some(Tap { uid: s })
    }
}
