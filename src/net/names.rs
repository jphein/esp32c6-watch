//! Deterministic magical node names — a faithful `no_std` port of realm-sigil's
//! `GenerateName`, copied from smol's `rust/clock/src/net/names.rs` so the watch
//! renders the *same* realm name for a peer id as the rest of the fleet.
//!
//! Names NEVER go on the wire: the only peer identity in SMOLv1 frames is the
//! 3-digit id, and both mesh ends derive the identical `(adjective, noun)` from
//! it. Pure integer math over a static string table — no heap, no float.
//!
//! The corpus is pinned verbatim from sigil's generated embeds (20 adjectives /
//! 20 nouns, `fantasy` realm — the locked realm for smol). Do not edit words in
//! place or every node's name changes.

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

/// The realm every smol unit agrees on. LOCKED to fantasy, matching the fleet.
pub const REALM: &Realm = &FANTASY;

/// Knuth multiplicative-hash constant (2^32 / φ, rounded to odd). Spreads an
/// 8-bit id across all 32 seed bits — see [`seed_from_id`].
const GOLDEN_U32: u32 = 2_654_435_761;

/// Faithful port of sigil's index math: `adj = A[seed % |A|]`,
/// `noun = N[(seed >> 8) % |N|]`. Matches sigil for any `u32` seed.
#[inline]
pub fn name_for_seed(seed: u32, realm: &'static Realm) -> (&'static str, &'static str) {
    let adj = realm.adjectives[(seed as usize) % realm.adjectives.len()];
    let noun = realm.nouns[((seed >> 8) as usize) % realm.nouns.len()];
    (adj, noun)
}

/// Spread an 8-bit id across 32 bits so BOTH the adjective (`% |A|`) and the
/// noun (`(>>8) % |N|`) vary between adjacent ids. Off-device parity:
/// `(id * 2654435761) & 0xFFFFFFFF`.
#[inline]
pub fn seed_from_id(id: u8) -> u32 {
    (id as u32).wrapping_mul(GOLDEN_U32)
}

/// A node's `(adjective, noun)` from its logical id. Both mesh ends call this
/// with the id carried in the frame to get an identical name.
#[inline]
pub fn name_for_id(id: u8) -> (&'static str, &'static str) {
    name_for_seed(seed_from_id(id), REALM)
}
