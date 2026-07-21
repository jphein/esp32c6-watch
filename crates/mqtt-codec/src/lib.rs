#![no_std]
//! Minimal hand-rolled MQTT 3.1.1 codec (QoS 0 + the QoS-1 command path).
//! PURE byte-buffer encode/decode — no sockets, no alloc, no async, no deps
//! (`core` + slices only). The socket/session layer that drives these lives in
//! the firmware (`net/mqtt_ha.rs`, `net/mqtt_climate.rs`); this crate is the
//! reusable, host-testable wire codec underneath them.
//!
//! Lifted near-verbatim from smol `rust/clock/src/net/mqtt.rs` (already
//! production on the C3 fleet). Kept byte-identical so watch ↔ smol ↔ broker
//! framing agree; the only change is `#![no_std]` + this crate framing.
//!
//! Scope (a burst/command client, not a resident stack):
//!   * encode: CONNECT (clean OR persistent session, username+password,
//!     keep-alive 0), PUBLISH (QoS 0, optional retain), SUBSCRIBE (QoS 0 or 1),
//!     PUBACK (QoS 1 ack), DISCONNECT.
//!   * decode: [`parse_packet`] pulls the first COMPLETE control packet out of a
//!     TCP byte-stream accumulator, surfacing CONNACK return code, SUBACK, and
//!     inbound PUBLISH (topic + payload + optional packet id for QoS>0).
//!
//! ## #101 command-path safety
//! A persistent session (clean-session cleared) lets the broker QUEUE QoS-1
//! command topics (e.g. `cmd/reset`) for an offline device and redeliver on
//! reconnect. The device MUST [`encode_puback`] each delivered QoS-1 PUBLISH or
//! the broker redelivers it every reconnect → a queued `cmd/reset` = reboot loop
//! (soft-brick). [`encode_subscribe_qos1`] + [`encode_puback`] + the `packet_id`
//! captured by [`parse_packet`] are that safety path.
//!
//! ## MQTT 3.1.1 framing (what the encoders build)
//! Every packet is: byte 0 = `type<<4 | flags`, then a 1–4 byte "remaining
//! length" varint, then variable-header + payload. UTF-8 fields are `u16`
//! length-prefixed.

/// Fixed-length cursor writer over a caller-owned buffer. Every write is bounds-
/// checked; if any overflows, [`Cursor::done`] returns `None` so the caller treats
/// the (too-small) buffer as an encode failure rather than emitting a short packet.
struct Cursor<'a> {
    buf: &'a mut [u8],
    pos: usize,
    ok: bool,
}

impl<'a> Cursor<'a> {
    fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, pos: 0, ok: true }
    }

    fn u8(&mut self, v: u8) {
        if self.pos < self.buf.len() {
            self.buf[self.pos] = v;
            self.pos += 1;
        } else {
            self.ok = false;
        }
    }

    fn bytes(&mut self, s: &[u8]) {
        for &b in s {
            self.u8(b);
        }
    }

    /// A UTF-8 field: `u16` big-endian length then the bytes. Lengths > 65535 can't
    /// occur here (our topics/ids are tiny), so the cast is safe.
    fn str_field(&mut self, s: &[u8]) {
        let n = s.len() as u16;
        self.u8((n >> 8) as u8);
        self.u8(n as u8);
        self.bytes(s);
    }

    /// The MQTT "remaining length" varint (1–4 bytes, 7 bits each, high bit = more).
    fn remaining_len(&mut self, mut len: usize) {
        loop {
            let mut byte = (len % 128) as u8;
            len /= 128;
            if len > 0 {
                byte |= 0x80;
            }
            self.u8(byte);
            if len == 0 {
                break;
            }
        }
    }

    fn done(self) -> Option<usize> {
        self.ok.then_some(self.pos)
    }
}

/// CONNECT with a PERSISTENT session (clean-session CLEARED), username + password.
/// Connect flags `0xC0` = username|password (clean-session bit cleared). A
/// persistent session lets the broker QUEUE QoS-1 command topics between our
/// per-flush connections and redeliver on reconnect (#101). NB: `0xC0`, NOT `0x82`
/// — `0x82` would clear the PASSWORD flag (0x40) → broker rejects CONNECT.
/// Keep-alive is 0 (disabled): a sub-second burst is far under any broker timeout.
pub fn encode_connect(out: &mut [u8], client_id: &[u8], user: &[u8], pass: &[u8]) -> Option<usize> {
    // variable header = 2+4 (proto name "MQTT") + 1 (level) + 1 (flags) + 2 (keepalive)
    let var = 10;
    let payload = (2 + client_id.len()) + (2 + user.len()) + (2 + pass.len());
    let mut c = Cursor::new(out);
    c.u8(0x10); // CONNECT
    c.remaining_len(var + payload);
    c.str_field(b"MQTT");
    c.u8(0x04); // protocol level 4 (3.1.1)
    c.u8(0xC0); // flags: username | password (clean-session CLEARED → persistent, #101)
    c.u8(0x00); // keep-alive MSB
    c.u8(0x00); // keep-alive LSB (0 = disabled)
    c.str_field(client_id);
    c.str_field(user);
    c.str_field(pass);
    c.done()
}

/// PUBLISH, QoS 0 (so no packet identifier). `retain` sets the RETAIN flag — used
/// for retained discovery/config topics but NOT transient telemetry.
pub fn encode_publish(out: &mut [u8], topic: &[u8], payload: &[u8], retain: bool) -> Option<usize> {
    let remaining = (2 + topic.len()) + payload.len();
    let mut c = Cursor::new(out);
    c.u8(0x30 | if retain { 0x01 } else { 0x00 }); // PUBLISH, QoS 0
    c.remaining_len(remaining);
    c.str_field(topic);
    c.bytes(payload);
    c.done()
}

/// SUBSCRIBE (one topic filter, requested QoS 0). Byte 0 is `0x82` — the low nibble
/// `0b0010` is mandatory for SUBSCRIBE. `packet_id` must be non-zero.
pub fn encode_subscribe(out: &mut [u8], packet_id: u16, topic: &[u8]) -> Option<usize> {
    encode_subscribe_qos(out, packet_id, topic, 0)
}

/// #101: SUBSCRIBE at requested QoS 1 — for transient COMMAND topics. With a
/// persistent session, a QoS-1 subscription makes the broker QUEUE a one-shot
/// command for our session and redeliver it on the next reconnect (a QoS-0 sub is
/// never queued for an offline client). The receiver MUST PUBACK the delivered
/// QoS-1 PUBLISH (see [`encode_puback`]) or the broker redelivers it every
/// reconnect → reboot loop.
pub fn encode_subscribe_qos1(out: &mut [u8], packet_id: u16, topic: &[u8]) -> Option<usize> {
    encode_subscribe_qos(out, packet_id, topic, 1)
}

fn encode_subscribe_qos(out: &mut [u8], packet_id: u16, topic: &[u8], qos: u8) -> Option<usize> {
    let remaining = 2 + (2 + topic.len()) + 1; // packet id + topic filter + requested-QoS byte
    let mut c = Cursor::new(out);
    c.u8(0x82);
    c.remaining_len(remaining);
    c.u8((packet_id >> 8) as u8);
    c.u8(packet_id as u8);
    c.str_field(topic);
    c.u8(qos); // requested QoS (0 for downlinks; 1 for the #101 command topics)
    c.done()
}

/// #101: PUBACK — acknowledge a received QoS-1 PUBLISH so the broker drops it from
/// our persistent session's outbound queue. WITHOUT this ack the broker redelivers
/// the message on every reconnect (a queued `cmd/reset` → reboot every flush =
/// soft-brick). Fixed 4 bytes: `0x40`, remaining-len 2, then the 2-byte packet id.
pub fn encode_puback(out: &mut [u8], packet_id: u16) -> Option<usize> {
    let mut c = Cursor::new(out);
    c.u8(0x40); // PUBACK
    c.u8(0x02); // remaining length = 2 (just the packet id)
    c.u8((packet_id >> 8) as u8);
    c.u8(packet_id as u8);
    c.done()
}

/// DISCONNECT — a clean goodbye so the broker doesn't fire a will / log a drop.
pub fn encode_disconnect(out: &mut [u8]) -> Option<usize> {
    let mut c = Cursor::new(out);
    c.u8(0xE0);
    c.u8(0x00);
    c.done()
}

/// A decoded inbound control packet (only the kinds a burst/command client cares about).
pub enum Incoming<'a> {
    /// CONNACK — `return_code` 0 = accepted; 5 = not authorized (bad/absent creds).
    ConnAck { return_code: u8 },
    /// SUBACK — the broker accepted our subscription (payload QoS not inspected).
    SubAck,
    /// An inbound PUBLISH: `payload` borrows the accumulator; the caller copies it
    /// out before advancing. `packet_id` is `Some` for a QoS>0 delivery (the caller
    /// MUST PUBACK it — #101); `None` at QoS0.
    Publish { topic: &'a [u8], payload: &'a [u8], packet_id: Option<u16> },
    /// Any other packet type (PINGRESP, etc.) — skipped, but still consumed.
    Other,
}

/// Decode the MQTT "remaining length" varint from the front of `buf`. Returns
/// `(value, bytes_consumed)`, or `None` if the varint is incomplete or malformed
/// (> 4 bytes — a protocol violation).
fn decode_remaining_len(buf: &[u8]) -> Option<(usize, usize)> {
    let mut multiplier = 1usize;
    let mut value = 0usize;
    let mut i = 0usize;
    loop {
        if i >= buf.len() || i >= 4 {
            return None;
        }
        let byte = buf[i];
        value += (byte & 0x7F) as usize * multiplier;
        i += 1;
        if byte & 0x80 == 0 {
            return Some((value, i));
        }
        multiplier *= 128;
    }
}

/// Pull the FIRST complete control packet out of a TCP byte-stream accumulator.
/// Returns `(Incoming, total_bytes)` where `total_bytes` is how many bytes to drop
/// from the front of `buf` once handled; `None` if `buf` doesn't yet hold a whole
/// packet (caller should read more). Never panics on a truncated/garbled buffer —
/// an over-long field just yields [`Incoming::Other`] with the packet consumed.
pub fn parse_packet(buf: &[u8]) -> Option<(Incoming<'_>, usize)> {
    if buf.len() < 2 {
        return None;
    }
    let header = buf[0];
    let ptype = header >> 4;
    let (remaining, rl_bytes) = decode_remaining_len(&buf[1..])?;
    let total = 1 + rl_bytes + remaining;
    if buf.len() < total {
        return None; // packet not fully arrived yet
    }
    let body = &buf[1 + rl_bytes..total];

    let inc = match ptype {
        2 => Incoming::ConnAck {
            // CONNACK body = [ack flags, return code]; guard the length.
            return_code: if body.len() >= 2 { body[1] } else { 0xFF },
        },
        9 => Incoming::SubAck,
        3 => {
            // PUBLISH body = topic (u16 len + bytes) [+ packet id if QoS>0] + payload.
            let qos = (header >> 1) & 0x03;
            if body.len() < 2 {
                return Some((Incoming::Other, total));
            }
            let tlen = ((body[0] as usize) << 8) | body[1] as usize;
            let mut off = 2 + tlen;
            if off + if qos > 0 { 2 } else { 0 } > body.len() {
                return Some((Incoming::Other, total));
            }
            let topic = &body[2..off];
            // #101: capture the packet id of a QoS>0 PUBLISH so the caller can PUBACK it (the bounds
            // check above guarantees `body[off]`/`body[off+1]` exist when qos>0). QoS0 → None.
            let packet_id = if qos > 0 {
                let pid = ((body[off] as u16) << 8) | body[off + 1] as u16;
                off += 2;
                Some(pid)
            } else {
                None
            };
            Incoming::Publish { topic, payload: &body[off..], packet_id }
        }
        _ => Incoming::Other,
    };
    Some((inc, total))
}
