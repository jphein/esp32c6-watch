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
