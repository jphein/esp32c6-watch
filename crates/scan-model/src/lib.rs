//! Passive 802.15.4 (Zigbee/Thread) scan data model + MAC-header parser (RS2).
//!
//! Pure and host-testable: depends only on `core` + `heapless`, never on
//! esp-hal. The on-target build feeds it `(channel, frame_bytes, rssi)` tuples
//! adapted from the radio's `ReceivedFrame` (RSSI/LQI are passthrough — the
//! radio supplies them, we don't parse them from the PDU). No protocol
//! classifier here: Zigbee-vs-Thread is deferred to v2 (v1 shows "802.15.4 PAN").
//!
//! # Frame-format caveat (v1 — 2006 MHR, best-effort)
//!
//! [`parse_mac_header`] implements the **802.15.4-2003/2006** MAC header only.
//! It does **not** handle 2015 (frame version ≥ 2) frames:
//! - **Sequence-number suppression** (FCF bit 8, 2015): the sequence byte is
//!   absent, so the fixed 3-byte MHR prefix assumed here is off-by-one → the
//!   PAN id / source address parse wrong, or the frame reads as malformed.
//! - **2015 PAN-ID compression** uses a different truth table than the single
//!   bit-6 rule applied here.
//!
//! **Thread uses 2015 frames** (enhanced-acks and some MAC commands suppress
//! the sequence number), so **PAN read-outs on a live Thread network will be
//! wrong or missing** — do not trust them. Zigbee (2006 frames) parses
//! correctly. These are accuracy gaps, never panics: the parser is fully
//! bounds-checked (oracle: 0 panics over 112 adversarial frames). Full 2015
//! parsing is deferred to v2 (tracking issue jphein/esp32c6-watch#1).
#![cfg_attr(not(test), no_std)]

use heapless::index_set::FnvIndexSet;
use heapless::Vec;

/// First 802.15.4 channel in the 2.4 GHz band; the scan sweeps 11..=26.
pub const CH_MIN: u8 = 11;
/// Number of channels (11..=26 inclusive).
pub const CH_COUNT: usize = 16;
/// Max distinct PANs retained (weakest-evicted on overflow).
pub const MAX_PANS: usize = 16;
/// Max distinct source devices tracked per PAN (power-of-two for `FnvIndexSet`).
pub const MAX_DEVICES: usize = 8;

/// 802.15.4 MAC frame type (FCF bits 0..=2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameType {
    Beacon,
    Data,
    Ack,
    MacCommand,
    Other(u8),
}

impl FrameType {
    fn from_fcf(fcf: u16) -> Self {
        match (fcf & 0x7) as u8 {
            0 => FrameType::Beacon,
            1 => FrameType::Data,
            2 => FrameType::Ack,
            3 => FrameType::MacCommand,
            other => FrameType::Other(other),
        }
    }
}

/// The MAC-header fields RS2 keys on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MacHeader {
    pub frame_type: FrameType,
    /// Network PAN id: the source PAN if present, else the destination PAN.
    pub pan_id: Option<u16>,
    /// 16-bit source address (short addr, or the low 16 bits of an extended one).
    pub src_addr: Option<u16>,
}

/// Parse an 802.15.4 **2003/2006** MAC header (see the crate-level frame-format
/// caveat — 2015 / Thread frames are not handled). `frame` starts at the Frame
/// Control Field (MHR) — the caller strips any PHY length byte; the FCS is
/// ignored. Returns `None` if the frame is too short or its addressing fields
/// run past the buffer (malformed).
pub fn parse_mac_header(frame: &[u8]) -> Option<MacHeader> {
    if frame.len() < 3 {
        return None; // need FCF (2 B) + sequence number (1 B)
    }
    let fcf = u16::from_le_bytes([frame[0], frame[1]]);
    let frame_type = FrameType::from_fcf(fcf);
    let dest_mode = ((fcf >> 10) & 0x3) as u8;
    let src_mode = ((fcf >> 14) & 0x3) as u8;
    let pan_compress = (fcf >> 6) & 0x1 == 1;

    let mut off = 3usize; // past FCF + sequence number

    // Destination addressing: PAN id (if any addressing) then the address.
    let mut dest_pan: Option<u16> = None;
    if dest_mode != 0 {
        dest_pan = Some(read_u16(frame, off)?);
        off += 2;
        off += addr_len(dest_mode)?; // skip the dest address
        if off > frame.len() {
            return None; // address ran past the buffer
        }
    }

    // Source addressing: PAN id (unless compressed) then the address.
    let mut src_pan: Option<u16> = None;
    let mut src_addr: Option<u16> = None;
    if src_mode != 0 {
        if pan_compress {
            src_pan = dest_pan; // compression → source PAN == dest PAN
        } else {
            src_pan = Some(read_u16(frame, off)?);
            off += 2;
        }
        match src_mode {
            2 => src_addr = Some(read_u16(frame, off)?), // 16-bit short
            3 => {
                if off + 8 > frame.len() {
                    return None; // 64-bit addr runs past the buffer
                }
                src_addr = Some(read_u16(frame, off)?); // low 16 bits (LE)
            }
            _ => {} // mode 1 reserved → no address
        }
    }

    Some(MacHeader {
        frame_type,
        pan_id: src_pan.or(dest_pan),
        src_addr,
    })
}

/// Read a little-endian u16 at `off`, or `None` if it runs past `buf`.
fn read_u16(buf: &[u8], off: usize) -> Option<u16> {
    if off + 2 > buf.len() {
        return None;
    }
    Some(u16::from_le_bytes([buf[off], buf[off + 1]]))
}

/// Byte length of an 802.15.4 address for its addressing mode
/// (2 = short/16-bit, 3 = extended/64-bit). `None` for the reserved mode 1.
fn addr_len(mode: u8) -> Option<usize> {
    match mode {
        2 => Some(2),
        3 => Some(8),
        _ => None,
    }
}

/// Per-channel activity summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelStat {
    pub frames: u32,
    /// Strongest RSSI seen on this channel; `i8::MIN` when nothing heard.
    pub peak_rssi: i8,
}

impl Default for ChannelStat {
    fn default() -> Self {
        ChannelStat { frames: 0, peak_rssi: i8::MIN }
    }
}

/// One discovered 802.15.4 PAN (network).
#[derive(Debug, Clone)]
pub struct PanEntry {
    pub pan_id: u16,
    pub channel: u8,
    pub last_rssi: i8,
    /// Smoothed RSSI (integer EWMA, alpha = 1/4).
    pub rssi_ewma: i16,
    pub frames: u32,
    pub beacons: u32,
    pub devices: FnvIndexSet<u16, MAX_DEVICES>,
}

impl PanEntry {
    fn new(pan_id: u16, channel: u8, rssi: i8) -> Self {
        PanEntry {
            pan_id,
            channel,
            last_rssi: rssi,
            rssi_ewma: rssi as i16,
            frames: 0,
            beacons: 0,
            devices: FnvIndexSet::new(),
        }
    }

    fn record(&mut self, channel: u8, rssi: i8, is_beacon: bool, src: Option<u16>) {
        self.channel = channel;
        self.last_rssi = rssi;
        self.rssi_ewma += (rssi as i16 - self.rssi_ewma) / 4;
        self.frames = self.frames.saturating_add(1);
        if is_beacon {
            self.beacons = self.beacons.saturating_add(1);
        }
        if let Some(s) = src {
            let _ = self.devices.insert(s); // full/dup → silently ignored
        }
    }
}

/// Rolling passive-scan state: discovered PANs + per-channel stats.
pub struct ScanState {
    pub pans: Vec<PanEntry, MAX_PANS>,
    pub channels: [ChannelStat; CH_COUNT],
    pub total: u32,
}

impl Default for ScanState {
    fn default() -> Self {
        Self::new()
    }
}

impl ScanState {
    pub const fn new() -> Self {
        ScanState {
            pans: Vec::new(),
            channels: [ChannelStat { frames: 0, peak_rssi: i8::MIN }; CH_COUNT],
            total: 0,
        }
    }

    /// Fold one received frame into the model. `channel` is the 802.15.4 channel
    /// (11..=26), `frame` the MAC frame (MHR onward), `rssi` in dBm. A malformed
    /// header still counts toward the channel's energy but creates no PAN.
    pub fn fold_frame(&mut self, channel: u8, frame: &[u8], rssi: i8) {
        self.total = self.total.saturating_add(1);

        // Channel energy — counted whether or not the header parses.
        let in_band = channel >= CH_MIN && ((channel - CH_MIN) as usize) < CH_COUNT;
        if in_band {
            let cs = &mut self.channels[(channel - CH_MIN) as usize];
            cs.frames = cs.frames.saturating_add(1);
            if rssi > cs.peak_rssi {
                cs.peak_rssi = rssi;
            }
        } else {
            // Off-band channel (caller bug): count it in `total` but don't
            // attribute it to a PAN — a PAN can't sit on a non-existent channel,
            // and this keeps `PanEntry.channel` from ever recording nonsense.
            return;
        }

        // PAN accounting needs a parseable header carrying a PAN id.
        let Some(hdr) = parse_mac_header(frame) else {
            return;
        };
        let Some(pan) = hdr.pan_id else {
            return;
        };
        let is_beacon = hdr.frame_type == FrameType::Beacon;

        if let Some(e) = self.pans.iter_mut().find(|e| e.pan_id == pan) {
            e.record(channel, rssi, is_beacon, hdr.src_addr);
            return;
        }

        // New PAN: append, or (when full) replace the weakest — but only if this
        // newcomer is stronger, so the retained set stays the strongest MAX_PANS.
        let mut entry = PanEntry::new(pan, channel, rssi);
        entry.record(channel, rssi, is_beacon, hdr.src_addr);
        if let Err(entry) = self.pans.push(entry) {
            if let Some((idx, weakest_ewma)) = self
                .pans
                .iter()
                .enumerate()
                .map(|(i, e)| (i, e.rssi_ewma))
                .min_by_key(|&(_, ewma)| ewma)
            {
                if entry.rssi_ewma > weakest_ewma {
                    self.pans[idx] = entry;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- canned frames (little-endian FCF; see IEEE 802.15.4 MHR) ----

    // Beacon: FCF=0x8000 (type=Beacon, dest-mode=none, src-mode=short, no
    // PAN-compress) · seq · srcPAN=0x1234 · srcAddr=0xABCD · (payload).
    fn beacon() -> [u8; 8] {
        [0x00, 0x80, 0x01, 0x34, 0x12, 0xCD, 0xAB, 0x00]
    }

    // Data: FCF=0x8841 (type=Data, dest-mode=short, src-mode=short,
    // PAN-compress=1) · seq · destPAN=0x1234 · destAddr=0x0000 · srcAddr=0xBEEF.
    fn data() -> [u8; 9] {
        [0x41, 0x88, 0x02, 0x34, 0x12, 0x00, 0x00, 0xEF, 0xBE]
    }

    #[test]
    fn parse_beacon() {
        let h = parse_mac_header(&beacon()).expect("beacon parses");
        assert_eq!(h.frame_type, FrameType::Beacon);
        assert_eq!(h.pan_id, Some(0x1234));
        assert_eq!(h.src_addr, Some(0xABCD));
    }

    #[test]
    fn parse_data_pan_compressed() {
        let h = parse_mac_header(&data()).expect("data parses");
        assert_eq!(h.frame_type, FrameType::Data);
        assert_eq!(h.pan_id, Some(0x1234)); // compressed → src PAN == dest PAN
        assert_eq!(h.src_addr, Some(0xBEEF));
    }

    #[test]
    fn parse_rejects_malformed() {
        assert_eq!(parse_mac_header(&[0x41, 0x88]), None); // too short (< FCF+seq)
        // dest-mode=short claims a PAN at off 3, but the buffer ends there:
        assert_eq!(parse_mac_header(&[0x41, 0x88, 0x02, 0x34]), None);
    }

    #[test]
    fn fold_creates_pan_and_channel_stat() {
        let mut s = ScanState::new();
        s.fold_frame(11, &beacon(), -55);
        assert_eq!(s.total, 1);
        assert_eq!(s.channels[0].frames, 1);
        assert_eq!(s.channels[0].peak_rssi, -55);
        assert_eq!(s.pans.len(), 1);
        let p = &s.pans[0];
        assert_eq!(p.pan_id, 0x1234);
        assert_eq!(p.channel, 11);
        assert_eq!(p.frames, 1);
        assert_eq!(p.beacons, 1);
        assert!(p.devices.contains(&0xABCD));
    }

    #[test]
    fn fold_dedups_same_pan_unions_devices() {
        let mut s = ScanState::new();
        s.fold_frame(11, &beacon(), -50); // pan 0x1234, dev 0xABCD, beacon
        s.fold_frame(11, &data(), -60); // pan 0x1234, dev 0xBEEF, data
        assert_eq!(s.pans.len(), 1, "same PAN dedups to one entry");
        let p = &s.pans[0];
        assert_eq!(p.frames, 2);
        assert_eq!(p.beacons, 1, "only the beacon counts as a beacon");
        assert_eq!(p.devices.len(), 2);
        assert!(p.devices.contains(&0xABCD) && p.devices.contains(&0xBEEF));
    }

    #[test]
    fn fold_malformed_counts_channel_not_pan() {
        let mut s = ScanState::new();
        s.fold_frame(15, &[0x41, 0x88], -70); // malformed
        assert_eq!(s.total, 1);
        assert_eq!(s.channels[4].frames, 1, "energy still counts on the channel");
        assert!(s.pans.is_empty(), "no PAN from a malformed header");
    }

    #[test]
    fn fold_out_of_band_channel_counts_total_only() {
        let mut s = ScanState::new();
        s.fold_frame(99, &beacon(), -50); // above ch 26 (caller bug)
        s.fold_frame(3, &beacon(), -50); //  below ch 11
        assert_eq!(s.total, 2, "off-band frames still bump total");
        assert!(s.pans.is_empty(), "no PAN attributed to an off-band channel");
        assert!(s.channels.iter().all(|c| c.frames == 0), "no channel stat touched");
    }

    #[test]
    fn channel_peak_tracks_max() {
        let mut s = ScanState::new();
        s.fold_frame(20, &beacon(), -70);
        s.fold_frame(20, &beacon(), -50);
        s.fold_frame(20, &beacon(), -60);
        assert_eq!(s.channels[9].frames, 3);
        assert_eq!(s.channels[9].peak_rssi, -50);
    }

    // Build a beacon on an arbitrary PAN id (src addr = pan for variety).
    fn beacon_pan(pan: u16) -> [u8; 8] {
        let [pl, ph] = pan.to_le_bytes();
        [0x00, 0x80, 0x01, pl, ph, pl, ph, 0x00]
    }

    #[test]
    fn fold_caps_pans_evicting_weakest() {
        let mut s = ScanState::new();
        // 16 PANs, each stronger than the last (-80.. -65).
        for i in 0..MAX_PANS as u16 {
            let rssi = -80 + i as i8;
            s.fold_frame(11, &beacon_pan(0x100 + i), rssi);
        }
        assert_eq!(s.pans.len(), MAX_PANS);
        // A 17th PAN stronger than the weakest (-80) evicts it.
        s.fold_frame(11, &beacon_pan(0xF00), -40);
        assert_eq!(s.pans.len(), MAX_PANS, "still capped at MAX_PANS");
        assert!(s.pans.iter().any(|p| p.pan_id == 0xF00), "strong newcomer admitted");
        assert!(!s.pans.iter().any(|p| p.pan_id == 0x100), "weakest (0x100) evicted");
        // A weaker-than-all newcomer is dropped, not admitted.
        s.fold_frame(11, &beacon_pan(0xF01), -120);
        assert_eq!(s.pans.len(), MAX_PANS);
        assert!(!s.pans.iter().any(|p| p.pan_id == 0xF01), "weak newcomer dropped");
    }

    #[test]
    fn device_set_caps_without_panic() {
        let mut s = ScanState::new();
        // 12 distinct source devices on one PAN; the set holds MAX_DEVICES.
        for dev in 0..12u16 {
            // data frame on pan 0x1234 with src = dev
            let [pl, ph] = 0x1234u16.to_le_bytes();
            let [sl, sh] = dev.to_le_bytes();
            let frame = [0x41, 0x88, 0x02, pl, ph, 0x00, 0x00, sl, sh];
            s.fold_frame(11, &frame, -50);
        }
        assert_eq!(s.pans.len(), 1);
        assert_eq!(s.pans[0].devices.len(), MAX_DEVICES);
        assert_eq!(s.pans[0].frames, 12);
    }

    #[test]
    fn ewma_smooths_toward_new() {
        let mut s = ScanState::new();
        s.fold_frame(11, &beacon(), -40); // seed ewma = -40
        s.fold_frame(11, &beacon(), -80); // -40 + (-80 - -40)/4 = -50
        assert_eq!(s.pans[0].rssi_ewma, -50);
        assert_eq!(s.pans[0].last_rssi, -80);
    }
}
