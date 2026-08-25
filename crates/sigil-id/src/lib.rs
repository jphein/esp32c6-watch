//! Deterministic magical device names — a faithful `no_std` port of
//! realm-sigil's `GenerateName` (github.com/jphein/realm-sigil), vendored
//! VERBATIM from smol's `rust/clock/src/net/names.rs` (#34). A device's
//! `(adjective, noun)` matches `sigil.generate_name(hex(seed), realm)` in
//! Go/Python/JS for any `u32` seed, so any device's name is reproducible
//! off-device.
//!
//! ⚠️ CORPUS-DRIFT WARNING — pinned deliberately. This table is copied
//! verbatim from sigil's GENERATED embeds (`go/realms.go` ==
//! `python/realm_sigil/realms.py` == `js/realms.js`; all three byte-identical,
//! 20 adjectives / 20 nouns per realm), via smol's names.rs — the exact
//! snapshot the rest of the fleet runs. If sigil ever re-runs its word-sync,
//! do NOT re-copy: every watch's name — and its per-watch OTA topic — would
//! change. Name stability is the point.
//!
//! On top of the verbatim core, this crate adds the MAC-seed path (issue #34,
//! smol research B2): [`seed_from_mac`], [`node_id_from_mac`] and the
//! lowercase topic-safe [`Sigil`] string ("eldritch-lantern"). All heap-free,
//! zero-alloc, host-testable (`cargo test -p sigil-id`).

#![cfg_attr(not(test), no_std)]

// --- verbatim core: smol rust/clock/src/net/names.rs ------------------------

/// A realm's word corpus. `name = "{adjectives[seed % |A|]} {nouns[(seed>>8) % |N|]}"`.
pub struct Realm {
    pub adjectives: &'static [&'static str],
    pub nouns: &'static [&'static str],
}

/// The `fantasy` realm — verbatim from sigil's generated corpus (20 adj / 20 noun).
pub static FANTASY: Realm = Realm {
    adjectives: &[
        "Arcane", "Blazing", "Celestial", "Draconic", "Eldritch", "Fabled", "Gilded",
        "Hallowed", "Infernal", "Jade", "Kindled", "Luminous", "Mythic", "Noble", "Obsidian",
        "Primal", "Radiant", "Spectral", "Twilight", "Valiant",
    ],
    nouns: &[
        "Aegis", "Beacon", "Crown", "Dominion", "Ember", "Forge", "Grimoire", "Herald",
        "Insignia", "Jewel", "Keystone", "Lantern", "Monolith", "Nexus", "Oracle", "Pinnacle",
        "Quartz", "Relic", "Sigil", "Throne",
    ],
};

/// The `forge` realm — verbatim from realm-sigil `words/realms.json` (14 adj /
/// 14 noun), for naming **BUILDS**, never devices.
///
/// Two namespaces, deliberately kept apart (realm-sigil's own rule): [`FANTASY`]
/// names a *device* — an identity that outlives every flash — while `forge`
/// names a *build* — provenance. "eldritch-lantern is running Bellowed Kiln"
/// only reads unambiguously because the two words can never be confused for
/// each other's kind. They overlap on exactly one word (`forge`, an adjective
/// in one and a noun in the other), which cannot produce a colliding full name.
///
/// Unlike [`FANTASY`] this table is NOT pinned-forever: build names are
/// ephemeral, so a future re-sync from sigil costs nothing but a different word
/// on the next build. Renaming a *device* would change its MQTT topic.
///
/// 14 x 14 = 196 pairs, so two builds ~196 apart may share a name — which is
/// why the short hash is displayed beside it and is the actual identifier. The
/// words exist so a human can tell two builds apart at a glance on a 410 px
/// panel, where seven hex characters all look alike.
pub static FORGE: Realm = Realm {
    adjectives: &[
        "Molten", "Hammered", "Tempered", "Forged", "Glowing", "Smoldering", "Sparking",
        "Kilned", "Ironclad", "Wrought", "Bellowed", "Anvilled", "White-Hot", "Smelted",
    ],
    nouns: &[
        "Forge", "Anvil", "Kiln", "Quench", "Hammer", "Smithy", "Smelter", "Ironheart",
        "Wright", "Crucible", "Bellows", "Mold", "Ingot", "Foundry",
    ],
};

/// Name a BUILD from its git short hash, e.g. `"d8f228e"` -> `("Bellowed", "Kiln")`.
///
/// The seed is the hash parsed as hex, matching realm-sigil's `parse_hex(hash)`
/// in all four languages, so the name is independently checkable. There is **no
/// `sigil` CLI** — verify against the corpus itself:
///
/// ```text
/// python3 -c "
/// import json; d=json.load(open('$HOME/Projects/realm-sigil/words/realms.json'))['forge']
/// s=int('d7cdcee',16)
/// print(d['adjectives'][s%14], d['nouns'][(s>>8)%14])"   # -> Molten Forge
/// ```
///
/// Two boundaries for whoever checks: strip a trailing `*` first (a dirty build's
/// hash is a CONTENT hash that no generator can re-derive from git), and never
/// paste a full 40-char SHA — this returns `None` above 8 hex chars while the Go,
/// Python and JS implementations will happily name it, giving a different answer.
/// 8 chars is also the widest safe value: an old JS consumer using `>>` instead
/// of BigInt coerces to int32 and breaks for seeds >= 2^31, which is about half
/// of all 8-char hashes. A 7-char git short hash is < 2^28 and safe everywhere.
///
/// Returns `None` for a non-hex or empty string rather than guessing, so a build
/// with no git info reports that fact instead of a confident wrong name.
///
/// Accepts up to 8 hex chars (a u32); longer input is refused rather than
/// truncated, because silently using a *different* seed than the caller's hash
/// would break exactly the cross-tool agreement this exists to provide.
pub fn build_name_for_hash(hash: &str) -> Option<(&'static str, &'static str)> {
    if hash.is_empty() || hash.len() > 8 {
        return None;
    }
    let mut seed: u32 = 0;
    for b in hash.as_bytes() {
        let digit = match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            b'A'..=b'F' => b - b'A' + 10,
            _ => return None,
        };
        seed = (seed << 4) | digit as u32;
    }
    Some(name_for_seed(seed, &FORGE))
}

/// The realm every unit agrees on (sigil's `realm` string). LOCKED to fantasy,
/// matching the smol fleet — repoint it (and paste another realm's table from
/// sigil's generated source) to re-theme every device's name at once.
pub const REALM: &Realm = &FANTASY;

/// Knuth multiplicative-hash constant (2^32 / φ, rounded to odd). Spreads an 8-bit
/// id across all 32 seed bits — see [`seed_from_id`].
const GOLDEN_U32: u32 = 2_654_435_761;

/// Faithful port of sigil's index math: `adj = A[seed % |A|]`,
/// `noun = N[(seed >> 8) % |N|]`. Uses the list LENGTH (not a hard-coded 20),
/// exactly like sigil's Go/Python source. `(seed >> 8)` still leaves 24 bits
/// for the noun. Matches sigil for any `u32` seed.
#[inline]
pub fn name_for_seed(seed: u32, realm: &'static Realm) -> (&'static str, &'static str) {
    let adj = realm.adjectives[(seed as usize) % realm.adjectives.len()];
    let noun = realm.nouns[((seed >> 8) as usize) % realm.nouns.len()];
    (adj, noun)
}

/// Spread an 8-bit id across 32 bits so BOTH the adjective (`% |A|`) and the noun
/// (`(>>8) % |N|`) vary between adjacent ids. WITHOUT this every id < 256 has
/// `(seed >> 8) == 0` and shares noun index 0 — all nodes would get the same noun.
/// Off-device parity: `(id * 2654435761) & 0xFFFFFFFF`, which on-device is
/// exactly `wrapping_mul`.
#[inline]
pub fn seed_from_id(id: u8) -> u32 {
    (id as u32).wrapping_mul(GOLDEN_U32)
}

/// A node's `(adjective, noun)` from its logical id. Both mesh ends call this with
/// the id carried in the frame to get an identical name. `.1` is the noun; `.0`
/// is the adjective.
#[inline]
pub fn name_for_id(id: u8) -> (&'static str, &'static str) {
    name_for_seed(seed_from_id(id), REALM)
}

// --- MAC-seed extensions (issue #34; smol research B2) -----------------------

/// Zero-config seed from the factory MAC's low 32 bits — smol research B2,
/// verbatim. The 3-byte OUI (`98:A3:16` across this fleet) is constant and
/// carries no entropy, so it is skipped. On-device the MAC comes from
/// `esp_hal::efuse::base_mac_address()` (esp-hal 1.1, `unstable` feature).
#[inline]
pub fn seed_from_mac(mac: [u8; 6]) -> u32 {
    u32::from_be_bytes([mac[2], mac[3], mac[4], mac[5]]) // skip constant OUI
}

/// A device's `(adjective, noun)` straight from its efuse MAC.
#[inline]
pub fn name_for_mac(mac: [u8; 6]) -> (&'static str, &'static str) {
    name_for_seed(seed_from_mac(mac), REALM)
}

/// Fold the efuse MAC to a mesh node id: XOR of the four [`seed_from_mac`]
/// bytes (documented convention — smol has no node-id-from-MAC precedent, so
/// this is the simplest fold that separates the fleet). 0 and 255 are remapped
/// to 1 / 254 (reserved/broadcast-adjacent). The config sentinel 42 is NOT
/// special-cased here: a derived 42 would be a legitimately-chosen id, and the
/// fleet check below proves neither watch lands on it.
///
/// Fleet (host-tested below): `…A7:2F:E4` → 122, `…A5:A7:F8` → 236.
#[inline]
pub fn node_id_from_mac(mac: [u8; 6]) -> u8 {
    let s = seed_from_mac(mac);
    match (s ^ (s >> 8) ^ (s >> 16) ^ (s >> 24)) as u8 {
        0 => 1,
        255 => 254,
        id => id,
    }
}

/// Longest possible sigil: longest adjective (9, "Celestial") + `-` +
/// longest noun (8, e.g. "Dominion") = 18; padded for slack. Exhaustively
/// verified by the `all_sigils_fit_and_are_topic_safe` host test.
pub const SIGIL_MAX: usize = 20;

/// A lowercase hyphenated sigil string ("eldritch-lantern") in a fixed
/// buffer — zero-alloc, `Copy`, safe to embed in a `static`. Topic-safe by
/// construction (ASCII lowercase + `-`; no MQTT wildcards or separators).
#[derive(Clone, Copy)]
pub struct Sigil {
    buf: [u8; SIGIL_MAX],
    len: u8,
}

impl Sigil {
    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.len as usize]).unwrap_or("")
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len as usize]
    }
}

impl core::fmt::Display for Sigil {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Format a seed's name as a lowercase sigil ("eldritch-lantern"). Bounded
/// writes (a corpus word that ever outgrew [`SIGIL_MAX`] would truncate, not
/// panic — and the host test would catch it first).
pub fn sigil_for_seed(seed: u32) -> Sigil {
    let (adj, noun) = name_for_seed(seed, REALM);
    let mut buf = [0u8; SIGIL_MAX];
    let mut len = 0usize;
    for &b in adj.as_bytes().iter().chain(b"-").chain(noun.as_bytes()) {
        if len == SIGIL_MAX {
            break;
        }
        buf[len] = b.to_ascii_lowercase();
        len += 1;
    }
    Sigil { buf, len: len as u8 }
}

/// A device's lowercase sigil straight from its efuse MAC.
#[inline]
pub fn sigil_for_mac(mac: [u8; 6]) -> Sigil {
    sigil_for_seed(seed_from_mac(mac))
}

// --- Known-fleet node-id → sigil (issue #35, watch-to-watch ping) ------------

/// The known watch fleet: `(node id, sigil)`, both derived from the efuse base
/// MAC (id = [`node_id_from_mac`], sigil = [`sigil_for_mac`]). A hand-kept
/// table is required because the id is a lossy XOR *fold* of the MAC — it
/// cannot be inverted back to a seed, so id-only frames (PING/PINGACK) need
/// this lookup to greet a peer by its TRUE per-device sigil (#34), not the
/// unrelated id-seeded roster name. Parity with the derivation is host-tested
/// (`fleet_node_sigils`), so a row can never silently drift from its MAC.
pub const FLEET_NODES: &[(u8, &str)] = &[
    (122, "eldritch-lantern"), // 98:A3:16:A7:2F:E4 (id DERIVED from MAC)
    (236, "mythic-throne"),    // 98:A3:16:A5:A7:F8 (id DERIVED from MAC)
    // NM-CYD-C5 (smol convergence target). ⚠️ id 176 is smol's ALLOCATION,
    // provisioned via the config-id override (config id != 42 beats the fold) —
    // the MAC fold derives 121, and both facts are host-tested below so neither
    // can silently drift. The SIGIL is still MAC-derived like every device.
    (176, "arcane-beacon"),    // 3C:DC:75:99:8D:18 (id ALLOCATED, not derived)
    // ESP32-S3 CYD (emberburrito's board, smol target). Same allocated-id
    // contract as the C5: smol allocated 162, the fold derives 150, sigil is
    // MAC-derived. Both host-tested below.
    (162, "eldritch-insignia"), // 14:C1:9F:D1:C8:10 (id ALLOCATED, not derived)
];

/// The known fleet's sigil for a mesh node id ([`FLEET_NODES`]); `None` for
/// ids outside the fleet (callers fall back to the frame's source MAC via
/// [`sigil_for_mac`], or to the id-seeded roster name).
pub fn sigil_for_node(id: u8) -> Option<&'static str> {
    FLEET_NODES
        .iter()
        .find(|(n, _)| *n == id)
        .map(|(_, sigil)| *sigil)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fleet efuse MACs (base MAC, `esp_hal::efuse::base_mac_address()`).
    const WATCH_A: [u8; 6] = [0x98, 0xA3, 0x16, 0xA7, 0x2F, 0xE4];
    const WATCH_B: [u8; 6] = [0x98, 0xA3, 0x16, 0xA5, 0xA7, 0xF8];

    /// Cross-language parity for the BUILD namespace: these pairs come from
    /// realm-sigil's own generator over `words/realms.json` forge, so a drift in
    /// this vendored table fails here instead of silently renaming builds.
    #[test]
    fn forge_build_names_match_sigil() {
        assert_eq!(build_name_for_hash("d8f228e"), Some(("Bellowed", "Kiln")));
        assert_eq!(build_name_for_hash("0000000"), Some(("Molten", "Forge")));
        // Case-insensitive: git prints lowercase, humans paste either.
        assert_eq!(build_name_for_hash("D8F228E"), build_name_for_hash("d8f228e"));
    }

    /// A missing or malformed hash must be REPORTED, not guessed. A build with
    /// no git info that confidently displays "Molten Forge" is worse than one
    /// that says it does not know — the whole point is trusting the label.
    #[test]
    fn forge_refuses_what_it_cannot_name() {
        assert_eq!(build_name_for_hash(""), None);
        assert_eq!(build_name_for_hash("nothex!"), None);
        // 9 chars would overflow the u32 seed; refuse rather than truncate to a
        // seed the caller never asked for.
        assert_eq!(build_name_for_hash("d8f228e00"), None);
        assert_eq!(build_name_for_hash("d8f228e0"), Some(name_for_seed(0xd8f228e0, &FORGE)));
    }

    /// The device and build namespaces must not produce the same full name, or
    /// "X is running Y" stops being readable. Overlap on single words is fine
    /// (and expected: `forge`); a colliding PAIR is not.
    #[test]
    fn build_names_never_collide_with_device_names() {
        // A nested scan, not a HashSet: this crate is `no_std` (deliberately —
        // it links into firmware), so the test prelude has no `std` collections
        // and no `Iterator`. 196 x 256 comparisons is nothing at test time.
        let mut id = 0u16;
        while id <= u8::MAX as u16 {
            let (da, dn) = name_for_id(id as u8);
            let mut ai = 0;
            while ai < FORGE.adjectives.len() {
                let mut ni = 0;
                while ni < FORGE.nouns.len() {
                    assert!(
                        !(FORGE.adjectives[ai] == da && FORGE.nouns[ni] == dn),
                        "build name is also a device name"
                    );
                    ni += 1;
                }
                ai += 1;
            }
            id += 1;
        }
    }

    #[test]
    fn smol_parity_known_seeds() {
        assert_eq!(name_for_seed(0, REALM), ("Arcane", "Aegis"));
        assert_eq!(name_for_seed(1, REALM), ("Blazing", "Aegis"));
        assert_eq!(name_for_seed(0x100, REALM), ("Radiant", "Beacon")); // 256%20=16, 1%20=1
        // seed_from_id(42) = 42 * 2654435761 mod 2^32 = 4112119562
        assert_eq!(seed_from_id(42), 4_112_119_562);
        assert_eq!(name_for_id(42), ("Celestial", "Herald"));
    }

    /// The two fleet watches: seeds, names, sigils, node ids — and separation.
    #[test]
    fn fleet_macs() {
        assert_eq!(seed_from_mac(WATCH_A), 0x16A7_2FE4);
        assert_eq!(seed_from_mac(WATCH_B), 0x16A5_A7F8);
        assert_eq!(name_for_mac(WATCH_A), ("Eldritch", "Lantern"));
        assert_eq!(name_for_mac(WATCH_B), ("Mythic", "Throne"));
        assert_eq!(sigil_for_mac(WATCH_A).as_str(), "eldritch-lantern");
        assert_eq!(sigil_for_mac(WATCH_B).as_str(), "mythic-throne");
        assert_eq!(node_id_from_mac(WATCH_A), 122);
        assert_eq!(node_id_from_mac(WATCH_B), 236);
        // De-collision guarantees: distinct, and neither lands back on the
        // config "unset" sentinel (42) or the remapped reserveds (0/255).
        assert_ne!(node_id_from_mac(WATCH_A), node_id_from_mac(WATCH_B));
        for mac in [WATCH_A, WATCH_B] {
            assert!(![0, 42, 255].contains(&node_id_from_mac(mac)));
        }
    }

    /// Every (adjective, noun) combination fits SIGIL_MAX and is MQTT-topic-
    /// and-BLE-name safe (ASCII lowercase + '-', no wildcards/separators).
    #[test]
    fn all_sigils_fit_and_are_topic_safe() {
        for a in 0..REALM.adjectives.len() as u32 {
            for n in 0..REALM.nouns.len() as u32 {
                let seed = a | (n << 8); // adj = seed%20, noun = (seed>>8)%20
                let s = sigil_for_seed(seed);
                let (adj, noun) = name_for_seed(seed, REALM);
                assert_eq!(s.as_str().len(), adj.len() + 1 + noun.len());
                assert!(s
                    .as_str()
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b == b'-'));
            }
        }
    }

    /// FLEET_NODES parity (#35): every table row must match what the fleet
    /// derivation says for its MAC — id via the XOR fold, sigil via the
    /// MAC seed — so the hand-kept lookup can never drift from reality.
    #[test]
    fn fleet_node_sigils() {
        for (mac, id, sigil) in [
            (WATCH_A, 122u8, "eldritch-lantern"),
            (WATCH_B, 236u8, "mythic-throne"),
        ] {
            assert_eq!(node_id_from_mac(mac), id);
            assert_eq!(sigil_for_mac(mac).as_str(), sigil);
            assert_eq!(sigil_for_node(id), Some(sigil));
        }
        // The CYD-C5's row has a DIFFERENT contract and its own assertions: the
        // sigil is MAC-derived like everyone's, but the node id is smol's
        // ALLOCATION (config-provisioned), NOT the fold. Both halves are pinned:
        // if the fold ever changes, the 121 assertion catches it; if someone
        // "fixes" the table to the derived id, the 176 assertion catches that.
        for (mac, alloc_id, fold_id, sigil) in [
            ([0x3Cu8, 0xDC, 0x75, 0x99, 0x8D, 0x18], 176u8, 121u8, "arcane-beacon"),
            ([0x14u8, 0xC1, 0x9F, 0xD1, 0xC8, 0x10], 162u8, 150u8, "eldritch-insignia"),
        ] {
            assert_eq!(sigil_for_mac(mac).as_str(), sigil);
            assert_eq!(node_id_from_mac(mac), fold_id, "the fold's answer (NOT the roster id)");
            assert_eq!(sigil_for_node(alloc_id), Some(sigil));
            assert_eq!(sigil_for_node(fold_id), None, "the derived id is deliberately absent");
        }
        // Off-fleet ids resolve to None (callers fall back to the MAC path).
        assert_eq!(sigil_for_node(42), None);
        assert_eq!(sigil_for_node(0), None);
    }

    /// The 0/255 remap in the node-id fold.
    #[test]
    fn node_id_reserved_remap() {
        // mac[2..6] chosen so the XOR fold hits 0 and 255 exactly.
        let zero_fold = [0x98, 0xA3, 0xAA, 0xAA, 0xAA, 0xAA]; // AA^AA^AA^AA = 0
        assert_eq!(node_id_from_mac(zero_fold), 1);
        let ff_fold = [0x98, 0xA3, 0xFF, 0x00, 0x00, 0x00]; // FF^00^00^00 = FF
        assert_eq!(node_id_from_mac(ff_fold), 254);
    }
}
