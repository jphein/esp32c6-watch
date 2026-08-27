//! Per-device SIGIL IDENTITY (#34): a stable name + node id derived from the
//! factory efuse MAC — zero-config, unique per chip, survives reflash and OTA
//! (nothing writes the efuse block).
//!
//! Derivation (all in `crates/sigil-id`, host-tested):
//! - seed = the MAC's low 4 bytes big-endian (smol research B2 convention;
//!   the 3-byte OUI is fleet-constant, no entropy),
//! - name = realm-sigil fantasy-realm `(adjective, noun)` for that seed,
//!   lowercased to a topic-safe sigil ("eldritch-lantern"),
//! - node id = XOR fold of the same 4 bytes (0/255 remapped).
//!
//! Fleet, from the two efuse base MACs:
//! - `98:A3:16:A7:2F:E4` → `eldritch-lantern`, node id 122
//! - `98:A3:16:A5:A7:F8` → `mythic-throne`,    node id 236
//!
//! The identity is computed ONCE (LazyLock over a plain efuse register read)
//! and lives in a `static`, so the MQTT paths and the BLE advertiser borrow
//! `&'static str`s from it directly. Consumers: main.rs (node-id 42-sentinel
//! arbitration + boot log), both MQTT paths (per-watch OTA topic
//! `watch/<sigil>/ota` + per-device client ids), the System page, and the BLE
//! advertised name.

/// The BUILD stamp, tagged so tooling can read the sigil out of the **image it
/// is about to flash** rather than recomputing it and possibly disagreeing.
///
/// `grep -a -o 'WSIGIL:[^\x00]*' watch.bin` on any image — a build directory, an
/// OTA payload, a downloaded artifact — answers "what is in this file?" with no
/// git, no toolchain and no trust in a log line. The alternative (tooling that
/// re-derives the name from the working tree) is a second implementation that
/// can disagree with the binary, which is the failure this whole change exists
/// to end.
///
/// `#[used]` is load-bearing: without it fat LTO drops an otherwise-unreferenced
/// static and the marker silently vanishes from the image. Costs 45 B of
/// `.rodata` in flash (worst case, measured) and zero RAM.
///
/// ⚠️ **`#[used]` lowers to `llvm.used`, which stops LLVM's DCE but NOT the ELF
/// linker's `--gc-sections`.** The marker therefore survives because nothing in
/// this project passes that flag — the only link args are `-Tlinkall.x` and the
/// error-handling script. #67 (ROM ceiling, ~6.9 KB free before
/// `widen_rom_region`) makes adding `--gc-sections` an attractive future diet,
/// and the failure would be SILENT: flash/OTA would simply stop printing a sigil
/// and every image would look like a pre-stamp build. `tools/preflight.sh`
/// therefore greps the built ELF for `WSIGIL:` and fails — a check on the
/// shipped bytes, which cannot rot the way this comment can.
#[used]
static BUILD_STAMP: &str = concat!(
    "WSIGIL:",
    env!("BUILD_SIGIL"),
    "|",
    env!("BUILD_HASH"),
    "|v",
    env!("CARGO_PKG_VERSION"),
    // Push-OTA build epoch (unix-seconds, "0" when unset) — the same value
    // baked into ota_http::BUILD_EPOCH, but here made greppable so a publisher
    // can verify an image's baked epoch before announcing (prevents the
    // announce>baked self-reinstall loop; build.rs emits OTA_BUILD_MARK).
    // Appended AFTER the version so existing WSIGIL parsers (fields 1-3) and the
    // `v[0-9.]*`-terminated sigil grep are unaffected.
    "|OTA=",
    env!("OTA_BUILD_MARK"),
    // Explicit terminator: a Rust `&str` literal is NOT NUL-terminated, so a
    // reader scanning for "not NUL" would run past the end into whatever
    // .rodata the linker placed next and report garbage as part of the version.
    "\0",
);

use embassy_sync::lazy_lock::LazyLock;

/// `watch/` + sigil (≤ [`sigil_id::SIGIL_MAX`]) + `/ota`.
const OTA_TOPIC_CAP: usize = 32;

pub struct SigilIdentity {
    /// The factory base MAC (efuse), for logs/debug.
    pub mac: [u8; 6],
    /// Lowercase hyphenated sigil, e.g. "eldritch-lantern".
    pub sigil: sigil_id::Sigil,
    /// MAC-derived mesh node id. Only *used* when the config id is the
    /// never-explicitly-chosen 42 default (the "unset" sentinel) — an
    /// explicitly set config id ≠ 42 wins. Arbitrated in main.rs.
    pub node_id: u8,
    /// Per-watch push-OTA topic `watch/<sigil>/ota`, subscribed alongside the
    /// fleet-wide `watch/ota/announce` by both MQTT paths.
    pub ota_topic: heapless::String<OTA_TOPIC_CAP>,
}

static IDENTITY: LazyLock<SigilIdentity> = LazyLock::new(|| {
    let mac: [u8; 6] = esp_hal::efuse::base_mac_address()
        .as_bytes()
        .try_into()
        .unwrap_or([0; 6]);
    let sigil = sigil_id::sigil_for_mac(mac);
    let mut ota_topic = heapless::String::new();
    // Infallible: 6 ("watch/") + SIGIL_MAX (20) + 4 ("/ota") = 30 ≤ 32, and the
    // longest real sigil is 18 (host-tested in sigil-id). Bounded regardless.
    let _ = ota_topic.push_str("watch/");
    let _ = ota_topic.push_str(sigil.as_str());
    let _ = ota_topic.push_str("/ota");
    SigilIdentity {
        mac,
        sigil,
        node_id: sigil_id::node_id_from_mac(mac),
        ota_topic,
    }
});

/// The device's sigil identity — computed on first use, cached in a `static`.
pub fn get() -> &'static SigilIdentity {
    IDENTITY.get()
}
