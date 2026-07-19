//! BLE advertising control via raw HCI commands.
//!
//! esp-radio's `BleConnector` is a low-level HCI transport — it doesn't
//! provide a GATT server or high-level advertising API. For the smartwatch
//! we only need the device to be discoverable (advertising its name), so
//! we send 3 HCI commands directly:
//!
//!   1. LE Set Advertising Parameters (slow interval = power-friendly)
//!   2. LE Set Advertising Data (Flags + Complete Local Name)
//!   3. LE Set Advertising Enable (on / off)
//!
//! The VHCI interface in ESP-IDF expects H4 transport framing, so every
//! command is prefixed with 0x01 (HCI Command Packet).

use embedded_io::Write;

/// Start BLE advertising as "Rust Watch".
/// Sends HCI commands synchronously via the BleConnector's Write impl.
pub fn start_advertising<W: Write>(hci: &mut W) -> Result<(), W::Error> {
    // 1) LE Set Advertising Parameters
    //    Opcode 0x2006, 15 bytes of params
    //    Interval: 0x0800 (1.28s) — slow to save power
    //    Type: ADV_IND (connectable, undirected)
    //    Channels: all 3 (37, 38, 39)
    hci.write_all(&[
        0x01,                   // H4: HCI command
        0x06, 0x20,             // opcode: LE Set Advertising Parameters
        15,                     // param length
        0x00, 0x08,             // interval min: 0x0800 (1280 * 0.625ms = 800ms)
        0x00, 0x08,             // interval max: 0x0800
        0x00,                   // type: ADV_IND
        0x00,                   // own addr type: public
        0x00,                   // peer addr type
        0, 0, 0, 0, 0, 0,      // peer addr (unused)
        0x07,                   // channel map: all
        0x00,                   // filter policy: any
    ])?;

    // 2) LE Set Advertising Data
    //    Opcode 0x2008, always 32 bytes of param (1 len + 31 data)
    let name = b"Rust Watch";
    let flags_len: u8 = 3;      // AD: [len=2, type=0x01 Flags, val=0x06]
    let name_ad_len: u8 = 1 + name.len() as u8; // [type + name bytes]
    let sig_octets = flags_len + 1 + name_ad_len; // total significant

    let mut cmd = [0u8; 36]; // 1 (H4) + 2 (opcode) + 1 (plen) + 32 (data) = 36
    cmd[0] = 0x01;                          // H4
    cmd[1] = 0x08; cmd[2] = 0x20;          // opcode: LE Set Advertising Data
    cmd[3] = 32;                            // param length (always 32)
    cmd[4] = sig_octets;                    // significant octets count
    // Flags AD structure
    cmd[5] = 2;                             // length of this AD
    cmd[6] = 0x01;                          // AD type: Flags
    cmd[7] = 0x06;                          // General Discoverable + BR/EDR Not Supported
    // Complete Local Name AD structure
    cmd[8] = name_ad_len;
    cmd[9] = 0x09;                          // AD type: Complete Local Name
    cmd[10..10 + name.len()].copy_from_slice(name);
    // Remaining bytes are zero (padding)
    hci.write_all(&cmd)?;

    // 3) LE Set Advertising Enable
    //    Opcode 0x200A, 1 byte param = 0x01 (enable)
    hci.write_all(&[
        0x01,           // H4
        0x0A, 0x20,     // opcode: LE Set Advertising Enable
        1,              // param length
        0x01,           // enable
    ])?;

    Ok(())
}

/// Stop BLE advertising.
pub fn stop_advertising<W: Write>(hci: &mut W) -> Result<(), W::Error> {
    hci.write_all(&[
        0x01,           // H4
        0x0A, 0x20,     // opcode: LE Set Advertising Enable
        1,              // param length
        0x00,           // disable
    ])?;
    Ok(())
}

/// Start an active BLE scan (observer role). Advertising reports arrive as
/// HCI LE Meta events, parsed with [`parse_adv_report`].
pub fn start_scan<W: Write>(hci: &mut W) -> Result<(), W::Error> {
    // LE Set Scan Parameters (0x200B): active scan (so peers send scan
    // responses carrying their names), interval 96*0.625ms, window 48*0.625ms,
    // public own-address, accept-all filter policy.
    hci.write_all(&[
        0x01,       // H4
        0x0B, 0x20, // opcode
        7,          // param length
        0x01,       // active scan
        0x60, 0x00, // interval
        0x30, 0x00, // window
        0x00,       // own address type: public
        0x00,       // filter policy: accept all
    ])?;
    // LE Set Scan Enable (0x200C): enable, controller filters duplicates.
    hci.write_all(&[0x01, 0x0C, 0x20, 2, 0x01, 0x01])?;
    Ok(())
}

/// Stop the BLE scan.
pub fn stop_scan<W: Write>(hci: &mut W) -> Result<(), W::Error> {
    hci.write_all(&[0x01, 0x0C, 0x20, 2, 0x00, 0x01])?;
    Ok(())
}

/// A device heard during scanning.
pub struct AdvReport {
    pub addr: [u8; 6],
    pub rssi: i8,
    /// Complete or shortened local name from the AD payload, if present.
    pub name: heapless::String<32>,
}

/// Parse one HCI packet as an LE Advertising Report event (0x3E / subevent
/// 0x02). Accepts packets with or without the leading H4 0x04 byte. Returns
/// the first report in the packet.
pub fn parse_adv_report(pkt: &[u8]) -> Option<AdvReport> {
    // Skip the H4 HCI-event prefix if present.
    let evt = if pkt.first() == Some(&0x04) { &pkt[1..] } else { pkt };
    // evt: [0x3E, plen, subevt=0x02, num, evt_type, addr_type, addr[6], dlen, data..., rssi]
    if evt.len() < 12 || evt[0] != 0x3E || evt[2] != 0x02 {
        return None;
    }
    let mut addr = [0u8; 6];
    addr.copy_from_slice(&evt[6..12]);
    addr.reverse(); // HCI is little-endian; display MSB-first
    let data_len = *evt.get(12)? as usize;
    let data = evt.get(13..13 + data_len)?;
    let rssi = *evt.get(13 + data_len)? as i8;

    // Walk the AD structures for a local name (0x09 complete / 0x08 short).
    let mut name = heapless::String::new();
    let mut i = 0;
    while i + 1 < data.len() {
        let len = data[i] as usize;
        if len == 0 || i + 1 + len > data.len() {
            break;
        }
        let ad_type = data[i + 1];
        if ad_type == 0x09 || ad_type == 0x08 {
            for &b in &data[i + 2..i + 1 + len] {
                if name.push(b as char).is_err() {
                    break;
                }
            }
            break;
        }
        i += 1 + len;
    }
    Some(AdvReport { addr, rssi, name })
}
