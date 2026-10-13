//! #349/#518: the 16-byte SMOL target descriptor, watch-side — emission and
//! the finalize-time suitability check the fleet has had since #349 and this
//! tree never grew. Wire format is `rust/clock/src/net/target.rs`'s, byte for
//! byte (that file is authoritative; this is a port, not a fork):
//!
//! ```text
//! off  size  field            meaning
//!  0    4    magic            "SMLT"
//!  4    1    desc_version     descriptor FORMAT version (1)
//!  5    1    chip             1=C3 · 2=C6 · 3=S3 · 4=C5; 0 = unknown
//!  6    2    features         u16 capability bitset (fleet FEAT_*)
//!  8    1    compat           persistent-state layout namespace + version
//!  9    1    min_from_compat  oldest running `compat` this image installs over
//! 10    2    reserved         0
//! 12    4    checksum         FNV-1a/32 over bytes 0..12
//! ```
//!
//! ## The flavor namespace (#518) — why `compat` is 200 here
//!
//! The descriptor is chip-scoped, and both flavors of a chip share the same
//! per-chip staged line (`smol/ota/staged/esp32s3`), so a chip-only check
//! lets a FLEET image onto a GUI board and vice versa. Both directions close
//! inside the existing wire format by partitioning the `compat` byte:
//! **0–99 = fleet** (currently `NVS_COMPAT = 1`), **200+ = GUI flavor**.
//!
//! - GUI images emit `compat = GUI_COMPAT (200)`, `min_from_compat = 200` —
//!   so a FLEET board refuses a GUI image through its *existing* rule 3
//!   (`running.compat (1) < image.min_from_compat (200)` → `CompatTooOld`),
//!   with zero fleet-side changes.
//! - [`decide_gui`] adds the mirror rule: an image whose `compat < 200` is a
//!   fleet-flavor image and is refused here.
//!
//! ## Why the check is at finalize, on the WRITTEN bytes
//!
//! Same reasoning as the fleet: the slot read-back is what the SHA gate
//! already does, it is transfer-order invariant (the mesh path writes NAK'd
//! windows out of order), and otadata is untouched at that point — a refusal
//! costs a download and nothing else. Before this module the watch leaf
//! checked **no descriptor at all**: the only thing between it and flashing a
//! wrong-architecture image was the numbering accident that watch builds are
//! epoch-scale while fleet builds are small integers. That is not a check.

use esp_println::println;

/// Descriptor magic. "SMLT" has no proper border (no prefix is also a
/// suffix), which is what lets [`DescScan`]'s naive restart be exact.
pub const MAGIC: [u8; 4] = *b"SMLT";
/// Whole record length on the wire.
pub const DESC_LEN: usize = 16;
/// The descriptor FORMAT version this code emits and can read.
pub const DESC_VERSION: u8 = 1;

/// Chip ids — the #349 wire values (`net/target.rs` is authoritative).
pub const CHIP_UNKNOWN: u8 = 0;
pub const CHIP_ESP32C6: u8 = 2;
pub const CHIP_ESP32S3: u8 = 3;
pub const CHIP_ESP32C5: u8 = 4;

/// Fleet FEAT_* bits this flavor truthfully claims: both radios are live on
/// every watch build (WiFi sessions + the SMOLv1 mesh).
const FEAT_WIFI: u16 = 1 << 0;
const FEAT_ESPNOW: u16 = 1 << 1;

/// The GUI flavor's `compat` namespace floor (see the module doc). Everything
/// at or above this is a GUI-flavor image; everything below is fleet.
pub const GUI_COMPAT: u8 = 200;

/// This build's chip id, from the one `board-*` feature that selects it.
pub const SELF_CHIP: u8 = if cfg!(feature = "board-esp32s3-cyd") {
    CHIP_ESP32S3
} else if cfg!(feature = "board-cyd-c5") {
    CHIP_ESP32C5
} else {
    // board-waveshare-c6 — also the default feature set.
    CHIP_ESP32C6
};

/// The decoded identity of an image (or of this build).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TargetId {
    pub desc_version: u8,
    pub chip: u8,
    pub features: u16,
    pub compat: u8,
    pub min_from_compat: u8,
}

/// What THIS image is — the emitted descriptor and [`decide_gui`]'s "running"
/// side are this one value.
pub const SELF: TargetId = TargetId {
    desc_version: DESC_VERSION,
    chip: SELF_CHIP,
    features: FEAT_WIFI | FEAT_ESPNOW,
    compat: GUI_COMPAT,
    min_from_compat: GUI_COMPAT,
};

const fn fnv1a32(bytes: &[u8], len: usize) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    let mut i = 0;
    while i < len {
        h ^= bytes[i] as u32;
        h = h.wrapping_mul(0x0100_0193);
        i += 1;
    }
    h
}

impl TargetId {
    pub const fn encode(self) -> [u8; DESC_LEN] {
        let mut d = [0u8; DESC_LEN];
        d[0] = MAGIC[0];
        d[1] = MAGIC[1];
        d[2] = MAGIC[2];
        d[3] = MAGIC[3];
        d[4] = self.desc_version;
        d[5] = self.chip;
        d[6] = (self.features & 0xff) as u8;
        d[7] = (self.features >> 8) as u8;
        d[8] = self.compat;
        d[9] = self.min_from_compat;
        // d[10..12] reserved = 0
        let c = fnv1a32(&d, 12);
        d[12] = (c & 0xff) as u8;
        d[13] = ((c >> 8) & 0xff) as u8;
        d[14] = ((c >> 16) & 0xff) as u8;
        d[15] = ((c >> 24) & 0xff) as u8;
        d
    }
}

/// Decode a 16-byte candidate. `Some` iff magic + checksum hold. A version
/// newer than ours still DECODES (so the caller can name `DescVersion` as the
/// refusal instead of the misleading `Absent`).
pub fn decode(raw: &[u8]) -> Option<TargetId> {
    if raw.len() < DESC_LEN || raw[0..4] != MAGIC {
        return None;
    }
    let stored = u32::from_le_bytes([raw[12], raw[13], raw[14], raw[15]]);
    if stored != fnv1a32(raw, 12) {
        return None;
    }
    Some(TargetId {
        desc_version: raw[4],
        chip: raw[5],
        features: (raw[6] as u16) | ((raw[7] as u16) << 8),
        compat: raw[8],
        min_from_compat: raw[9],
    })
}

/// The descriptor embedded in this image, for other boards (and our own
/// finalize check after an OTA) to read. `#[used]` + `no_mangle` keep it
/// through DCE; [`self_desc_present`] is the genuine runtime reference that
/// keeps `--gc-sections` honest — the fleet's exact arrangement.
#[used]
#[unsafe(no_mangle)]
pub static SMOL_TARGET_DESC: [u8; DESC_LEN] = SELF.encode();

/// Reads the embedded descriptor back and confirms it decodes to [`SELF`].
/// Called once on the OTA path; its real job is to be a runtime reference to
/// [`SMOL_TARGET_DESC`] so the linker cannot drop the bytes the guard needs.
pub fn self_desc_present() -> bool {
    let mut raw = [0u8; DESC_LEN];
    for (i, b) in raw.iter_mut().enumerate() {
        // Volatile so the read is not const-folded away with the reference.
        *b = unsafe { core::ptr::read_volatile(SMOL_TARGET_DESC.as_ptr().add(i)) };
    }
    decode(&raw) == Some(SELF)
}

// The WLED anti-lesson, kept from the fleet: the emitted bytes are decoded
// back at COMPILE time and compared field-by-field to the value the checker
// uses — a literal smuggled into either side fails the build rather than
// silently disabling the guard.
const _: () = {
    let d = SELF.encode();
    assert!(d[0] == MAGIC[0] && d[1] == MAGIC[1] && d[2] == MAGIC[2] && d[3] == MAGIC[3]);
    assert!(d[4] == DESC_VERSION);
    assert!(d[5] == SELF_CHIP);
    assert!(d[8] == GUI_COMPAT);
    assert!(d[9] == GUI_COMPAT);
    assert!(SELF_CHIP != CHIP_UNKNOWN);
    // The flavor namespace only works if we actually live in it.
    assert!(GUI_COMPAT >= 200);
};

/// Why an image was judged unsuitable for THIS board. Terse stable labels —
/// they ride refusal log lines and (mesh path) the abort NAK reason.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TargetReject {
    /// No valid descriptor anywhere in the image. An image that will not say
    /// what it is for cannot be judged suitable (pre-#518 watch artifacts and
    /// hand-rolled images land here; monotonicity already blocks old builds).
    Absent,
    /// Two DIFFERENT checksummed records — an image that says two things is
    /// as unjudgeable as one that says nothing.
    Ambiguous,
    /// Descriptor format newer than ours — unreadable, therefore unjudgeable.
    DescVersion,
    /// Built for different silicon. The bootloader would catch this too, but
    /// only by boot-looping into rollback; here it is a diagnosis instead.
    Chip,
    /// A fleet-flavor image (`compat < GUI_COMPAT`) on a GUI board — right
    /// chip, wrong firmware family (#518's flavor hole, closed).
    FleetFlavor,
}

impl TargetReject {
    pub const fn label(self) -> &'static str {
        match self {
            TargetReject::Absent => "tgt-absent",
            TargetReject::Ambiguous => "tgt-ambiguous",
            TargetReject::DescVersion => "tgt-descver",
            TargetReject::Chip => "tgt-chip",
            TargetReject::FleetFlavor => "tgt-flavor",
        }
    }
}

/// The GUI board's suitability verdict on a fully-written image. Fleet
/// `decide()` rules 1–2 verbatim, plus the flavor rule; the fleet's compat
/// rule 3 is subsumed by it (any GUI-namespace image can install over any
/// GUI-namespace state today — revisit when the watch has a real persistent
/// migration to gate), and rule 4 (recovery-feature loss) is deferred with
/// it — a GUI image always claims both radios, see [`SELF`].
pub fn decide_gui(image: TargetId) -> Result<(), TargetReject> {
    if image.desc_version > DESC_VERSION {
        return Err(TargetReject::DescVersion);
    }
    if image.chip != SELF.chip {
        return Err(TargetReject::Chip);
    }
    if image.compat < GUI_COMPAT {
        return Err(TargetReject::FleetFlavor);
    }
    Ok(())
}

/// Incremental descriptor scanner: feed the image bytes IN ORDER (the HTTP
/// stream, or the mesh finalize's slot read-back), then [`verdict`]. Records
/// spanning feed boundaries are caught by carrying a `DESC_LEN - 1` tail.
///
/// [`verdict`]: DescScan::verdict
pub struct DescScan {
    tail: [u8; DESC_LEN - 1],
    tail_len: usize,
    found: Option<TargetId>,
    ambiguous: bool,
}

impl DescScan {
    pub const fn new() -> Self {
        Self {
            tail: [0; DESC_LEN - 1],
            tail_len: 0,
            found: None,
            ambiguous: false,
        }
    }

    fn note(&mut self, id: TargetId) {
        match self.found {
            None => self.found = Some(id),
            // The same record seen twice (tail overlap, or a genuine second
            // copy of identical bytes) is idempotent; a DIFFERENT one is not.
            Some(prev) if prev != id => self.ambiguous = true,
            _ => {}
        }
    }

    pub fn feed(&mut self, chunk: &[u8]) {
        // Window = carried tail + this chunk. A fixed stack buffer sized for
        // the 4 KB feeds both call sites use; larger chunks fall back to a
        // split feed.
        if chunk.len() > 4096 {
            let (a, b) = chunk.split_at(chunk.len() / 2);
            self.feed(a);
            self.feed(b);
            return;
        }
        let mut win = [0u8; (DESC_LEN - 1) + 4096];
        win[..self.tail_len].copy_from_slice(&self.tail[..self.tail_len]);
        win[self.tail_len..self.tail_len + chunk.len()].copy_from_slice(chunk);
        let len = self.tail_len + chunk.len();

        let mut i = 0;
        while i + DESC_LEN <= len {
            if win[i..i + 4] == MAGIC {
                // decode() checks magic + checksum; a non-checksumming SMLT
                // (e.g. the checker's own constant) is simply not a record.
                if let Some(id) = decode(&win[i..i + DESC_LEN]) {
                    self.note(id);
                }
            }
            i += 1;
        }
        // Carry the last DESC_LEN-1 bytes so a record straddling this feed
        // boundary is seen whole on the next feed.
        let keep = len.min(DESC_LEN - 1);
        self.tail[..keep].copy_from_slice(&win[len - keep..len]);
        self.tail_len = keep;
    }

    /// The scan's verdict over everything fed so far.
    pub fn verdict(&self) -> Result<TargetId, TargetReject> {
        if self.ambiguous {
            return Err(TargetReject::Ambiguous);
        }
        self.found.ok_or(TargetReject::Absent)
    }
}

/// The shared finalize gate both OTA paths call once the image bytes are all
/// seen: scan verdict → [`decide_gui`]. `Ok` = safe to flip otadata. Logs the
/// refusal with its stable label; the caller turns it into its own path's
/// refusal shape (`refused:` string / mesh abort).
pub fn gate_written_image(scan: &DescScan) -> Result<(), TargetReject> {
    let id = match scan.verdict() {
        Ok(id) => id,
        Err(why) => {
            println!("[OTA] target descriptor refusal: {}", why.label());
            return Err(why);
        }
    };
    if let Err(why) = decide_gui(id) {
        println!(
            "[OTA] image unsuitable ({}): chip={} compat={} (self: chip={} GUI_COMPAT={})",
            why.label(),
            id.chip,
            id.compat,
            SELF.chip,
            GUI_COMPAT
        );
        return Err(why);
    }
    Ok(())
}
