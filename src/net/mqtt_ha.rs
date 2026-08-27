//! Fire-and-forget MQTT 3.1.1 publish burst to Home Assistant.
//!
//! Hand-rolled MQTT rather than a crate: mcutie 0.4 wants to own a
//! never-returning reconnect task (and hard-codes port 1883 behind a DNS
//! lookup), which fights the watch's short WiFi burst model, and the
//! remaining crates either drag in an old embedded-io-async or are overkill
//! for three QoS-0 publishes. CONNECT + CONNACK + PUBLISHx3 + DISCONNECT is
//! ~150 lines with no new dependencies.
//!
//! Called once per WiFi window, right after NTP sync succeeds and before the
//! firmware drops the association. Any failure logs `[MQTT] failed: ...` and
//! returns; the boot/NTP/mesh flow is never blocked for more than ~5s.

use embassy_net::{tcp::TcpSocket, Ipv4Address, Stack};
use embassy_time::{with_timeout, Duration, Instant, Timer};
use esp_println::println;
use heapless::Vec;

/// Broker as "ip:port". Set MQTT_BROKER at build time to override.
/// `pub(crate)` so the bidirectional climate session ([`crate::net::mqtt_climate`])
/// reuses the same broker address.
pub(crate) const BROKER: &str = match option_env!("MQTT_BROKER") {
    Some(s) => s,
    None => "192.168.1.10:1883",
};
const USER: Option<&str> = option_env!("MQTT_USER");
const PASS: Option<&str> = option_env!("MQTT_PASS");

/// Client-id prefix; the per-device sigil is appended (#34). Both watches
/// connecting under one fixed id ("smolwatch042") meant an MQTT 3.1.1 session
/// takeover — the broker evicts the sibling mid-window — whenever their
/// bursts overlapped. `smolwatch-<sigil>` keeps the fleet collision-free.
pub(crate) const CLIENT_ID_PREFIX: &str = "smolwatch-";
/// prefix (10) + sigil (≤ 20) + "-clim" (5) headroom for the climate session.
pub(crate) const CLIENT_ID_CAP: usize = 40;
const KEEPALIVE_SECS: u16 = 30;

/// HA discovery config + state topics are built PER-DEVICE from the sigil in
/// `burst()` (#492) — a shared literal made two watches collide on one HA entity.

/// Largest single packet we build (discovery config is ~330 bytes).
/// `pub(crate)` so the climate session reuses the shared packet builders.
pub(crate) const PKT_CAP: usize = 512;

/// How long the burst lingers for a retained push-OTA announce after
/// subscribing (a present retained message arrives ~immediately after SUBACK;
/// this only costs the full window when there is NO announce). Must stay under
/// the socket's 2s inactivity timeout so an empty wait cancels cleanly and the
/// DISCONNECT still goes out.
const ANNOUNCE_WAIT: Duration = Duration::from_millis(1500);

/// Publish the HA discovery config, battery percent, and uptime to the
/// broker, then check the retained push-OTA announce (`watch/ota/announce`)
/// — the boot burst is the once-per-boot MQTT window that makes a pushed
/// update reach a watch with no screen-held session up. Never fails the
/// caller: logs `[MQTT] published` or `[MQTT] failed: <reason>` and returns.
///
/// The outer bound must EXCEED the sum of the inner ones or it steals their
/// diagnosis: at 8 s, a 2 s connect plus a 5 s handshake already fills 7 s, so a
/// healthy-but-slow broker would be cut off here and reported as
/// `timeout (12s)` — hiding the specific reason the inner bounds would have named.
/// A generic timeout that pre-empts a specific error is strictly worse than the
/// specific error. 12 s = 2 (connect) + 5 (handshake) + 5 (publishes + DISCONNECT).
///
/// Raising it does NOT slow the common failure path: the inner bounds still fail
/// fast, so an unreachable broker still gives up in ~2 s. This only widens the
/// backstop for the pathological case.
pub async fn publish_burst(stack: Stack<'static>, batt_pct: u8) {
    // One retry on a TRANSIENT failure. The first connect of a boot burst can
    // race the just-completed association settling (observed on the S3-CYD:
    // a lone `tcp connect` fail while the C6 reaches the same broker fine),
    // and the burst is otherwise single-shot until the next ready-edge. Only
    // transient reasons retry; a deterministic auth/parse failure (bad creds,
    // bad broker string) would just fail again, so it gives up immediately.
    // The success path is unchanged — the retry only fires after a failure.
    // Off the mesh channel, the single radio time-slices STA<->ESP-NOW and a
    // tight TCP connect starves (s3-cyd 2026-08-26). Widen the whole burst
    // budget in that case ONLY; the common co-channel path is unchanged.
    let off_channel = off_mesh_channel();
    let budget = if off_channel { BURST_BUDGET_OFFCH } else { BURST_BUDGET };
    for attempt in 0..2 {
        match with_timeout(budget, burst(stack, batt_pct, off_channel)).await {
            Ok(Ok(())) => {
                println!("[MQTT] published");
                return;
            }
            Ok(Err(reason)) => {
                let transient = matches!(reason, "tcp connect" | "tcp read" | "tcp write");
                if transient && attempt == 0 {
                    println!("[MQTT] {reason} - retrying once");
                    Timer::after(BURST_RETRY_DELAY).await;
                    continue;
                }
                println!("[MQTT] failed: {reason}");
                return;
            }
            Err(_) => {
                println!("[MQTT] failed: timeout (12s, outer backstop)");
                return;
            }
        }
    }
}

/// Delay before the single burst retry — long enough for a racing association
/// / DHCP to settle, short enough to stay inside the boot-burst window.
const BURST_RETRY_DELAY: Duration = Duration::from_millis(750);

/// Whole-burst backstop. Must stay > CONNECT_TIMEOUT + HANDSHAKE_TIMEOUT so it
/// never pre-empts the specific error those produce.
const BURST_BUDGET: Duration = Duration::from_secs(12);
/// Off-mesh-channel budget: the time-sliced connect + handshake need room.
/// Must exceed CONNECT_TIMEOUT_OFFCH (8) + HANDSHAKE_TIMEOUT_OFFCH (8) + the
/// publish/DISCONNECT tail. Only used when associated off the mesh channel.
const BURST_BUDGET_OFFCH: Duration = Duration::from_secs(24);
/// Connect / handshake bounds when associated off the mesh channel — the
/// sliced radio delays SYN/SYN-ACK and CONNACK well past the co-channel values.
const CONNECT_TIMEOUT_OFFCH: Duration = Duration::from_secs(8);
const HANDSHAKE_TIMEOUT_OFFCH: Duration = Duration::from_secs(8);

/// True when associated on a channel other than the mesh's elected one (both
/// known and different). Neither known -> false (assume co-channel / no penalty).
fn off_mesh_channel() -> bool {
    let mesh = crate::net::net_task::preferred_channel();
    match crate::net::net_task::landed_channel() {
        Some(landed) if mesh != 0 && landed != mesh => true,
        _ => false,
    }
}
/// TCP-connect bound, kept tight: if the broker is down the SYN goes unanswered, and
/// `connect` would otherwise block the single-threaded executor for the whole
/// handshake window during the boot NTP burst.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// Post-connect idle bound for CONNACK and the publishes. Deliberately longer than
/// the connect bound: a broker that authenticates (MQTT_USER/MQTT_PASS) may take
/// well over 2 s to return CONNACK and still be entirely healthy — which is what
/// `[MQTT] failed: tcp read` was reporting. Matches `mqtt_climate`'s value.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

async fn burst(
    stack: Stack<'static>,
    batt_pct: u8,
    off_channel: bool,
) -> Result<(), &'static str> {
    let connect_to = if off_channel { CONNECT_TIMEOUT_OFFCH } else { CONNECT_TIMEOUT };
    let handshake_to = if off_channel { HANDSHAKE_TIMEOUT_OFFCH } else { HANDSHAKE_TIMEOUT };
    let (ip, port) = parse_broker(BROKER).ok_or("bad MQTT_BROKER (want ip:port)")?;

    let mut rx_buf = [0u8; 256];
    let mut tx_buf = [0u8; 1024];
    let mut socket = TcpSocket::new(stack, &mut rx_buf, &mut tx_buf);
    // SPLIT timeouts, because one value cannot serve both phases — and using one
    // was the bug behind `[MQTT] failed: tcp read`.
    //
    // `set_timeout` is the socket's INACTIVITY timeout: it governs the connect AND
    // every subsequent read. At 2 s it fast-failed a doomed connect (good) and also
    // gave the broker only 2 s to return CONNACK (bad). `tcp read` is reachable only
    // AFTER a successful connect, so the error string was already saying the
    // handshake timed out rather than that the broker was unreachable — the comment
    // that used to live here blamed a roam-VLAN routing problem, which JP has since
    // confirmed cannot apply: the `admin` SSID reaches every subnet.
    //
    // `mqtt_climate` already solved this and this is its shape: bound `connect`
    // tightly with an explicit `with_timeout` so an unreachable broker cannot block
    // the single-threaded executor, and give the post-connect handshake its own
    // longer window. An authenticating broker doing a credential lookup can easily
    // exceed 2 s for CONNACK while being perfectly healthy.
    socket.set_timeout(Some(handshake_to));

    match with_timeout(connect_to, socket.connect((ip, port))).await {
        Ok(Ok(())) => {}
        Ok(Err(_)) => return Err("tcp connect"),
        Err(_) => return Err("tcp connect: timeout"),
    }

    // CONNECT -> CONNACK
    let mut client_id: heapless::String<CLIENT_ID_CAP> = heapless::String::new();
    let _ = client_id.push_str(CLIENT_ID_PREFIX);
    let _ = client_id.push_str(crate::net::sigil::get().sigil.as_str());
    let mut pkt: Vec<u8, PKT_CAP> = Vec::new();
    build_connect(&mut pkt, client_id.as_str())?;
    write_all(&mut socket, &pkt).await?;

    let mut ack = [0u8; 4];
    read_exact(&mut socket, &mut ack).await?;
    if ack[0] != 0x20 || ack[1] != 0x02 {
        return Err("bad CONNACK");
    }
    if ack[3] != 0x00 {
        return Err("broker refused connection (check MQTT_USER/MQTT_PASS)");
    }

    // Per-device HA discovery + state topics (#492), keyed on the device sigil.
    // The client-id is already per-device (#34), but the discovery config,
    // unique_id, device identifier and state topics were a shared literal
    // ("smolwatch042"/"smolwatch/battery") — so two watches published to ONE HA
    // entity and overwrote each other's battery. Keying every topic + id on the
    // sigil (topic-safe by construction: lowercase + '-', host-tested) makes each
    // watch its own HA device. Model is board-relative via board::CHIP_NAME, so
    // the C6 claims the reserved "smol ESP32-C6 Watch" string and the S3/C5 read
    // true. State topics MUST match the discovery's state_topic or HA shows an
    // unavailable entity.
    let sigil = crate::net::sigil::get().sigil.as_str();
    let mut batt_topic: heapless::String<48> = heapless::String::new();
    let _ = batt_topic.push_str("smolwatch-");
    let _ = batt_topic.push_str(sigil);
    let _ = batt_topic.push_str("/battery");
    let mut uptime_topic: heapless::String<48> = heapless::String::new();
    let _ = uptime_topic.push_str("smolwatch-");
    let _ = uptime_topic.push_str(sigil);
    let _ = uptime_topic.push_str("/uptime");
    let mut disc_topic: heapless::String<80> = heapless::String::new();
    let _ = disc_topic.push_str("homeassistant/sensor/smolwatch-");
    let _ = disc_topic.push_str(sigil);
    let _ = disc_topic.push_str("/battery/config");
    // Discovery JSON, built per-device. HA renders "<device name> <entity name>",
    // so the entity name is the bare "Battery".
    let mut disc: heapless::String<384> = heapless::String::new();
    let _ = disc.push_str(r#"{"name":"Battery","state_topic":""#);
    let _ = disc.push_str(batt_topic.as_str());
    let _ = disc.push_str(r#"","unit_of_measurement":"%","device_class":"battery","unique_id":"smolwatch_"#);
    let _ = disc.push_str(sigil);
    let _ = disc.push_str(r#"_battery","device":{"identifiers":["smolwatch-"#);
    let _ = disc.push_str(sigil);
    let _ = disc.push_str(r#""],"name":"smol watch "#);
    let _ = disc.push_str(sigil);
    let _ = disc.push_str(r#"","model":"smol "#);
    let _ = disc.push_str(crate::board::CHIP_NAME);
    let _ = disc.push_str(r#" Watch","manufacturer":"jphein"}}"#);

    // Discovery config (retained) + state topics (QoS 0).
    publish(&mut socket, disc_topic.as_str(), disc.as_bytes(), true).await?;

    let mut num = [0u8; 20];
    let batt = fmt_u64(batt_pct as u64, &mut num);
    publish(&mut socket, batt_topic.as_str(), batt, false).await?;

    let mut num = [0u8; 20];
    let uptime = fmt_u64(Instant::now().as_secs(), &mut num);
    publish(&mut socket, uptime_topic.as_str(), uptime, false).await?;

    // Push-OTA window: SUBSCRIBE the retained announce topic and linger
    // ANNOUNCE_WAIT for the broker's immediate retained delivery. Placed after
    // the telemetry publishes so a broken announce path can never cost them.
    check_ota_announce(&mut socket).await?;

    // DISCONNECT, then flush so everything hits the wire before close.
    write_all(&mut socket, &[0xE0, 0x00]).await?;
    socket.flush().await.map_err(|_| "tcp flush")?;
    socket.close();
    Ok(())
}

/// SUBSCRIBE the fleet `watch/ota/announce` AND the per-watch
/// `watch/<sigil>/ota` (#34) at QoS 0, then wait [`ANNOUNCE_WAIT`] for
/// retained announce PUBLISHes. A delivered announce is fed through
/// [`crate::net::ota_http::handle_announce`] (gate + post for main.rs); no
/// retained message just times the wait out — that is the common case and not
/// an error. Frame decode reuses [`crate::net::mqtt_climate::read_frame`].
///
/// Notifications (#32) ride the same window: `watch/notify` (fleet) +
/// `watch/<sigil>/notify` are subscribed too, so a RETAINED notify reaches
/// the wrist on the next boot/NTP burst — offline pickup without waiting for
/// an HA screen to open a full session. (The ring's duplicate-of-newest
/// guard absorbs the re-delivery on every subsequent window.)
async fn check_ota_announce(socket: &mut TcpSocket<'_>) -> Result<(), &'static str> {
    let (notify_fleet, notify_device) = crate::net::mqtt_climate::notify_topics();
    let topics = [
        crate::net::ota_http::ANNOUNCE_TOPIC,
        crate::net::sigil::get().ota_topic.as_str(),
        notify_fleet,
        notify_device,
    ];

    // SUBSCRIBE (packet id 1, QoS 0) -> SUBACK.
    let mut remaining = 2usize; // packet id
    for t in topics {
        remaining += 2 + t.len() + 1;
    }
    let mut pkt: Vec<u8, PKT_CAP> = Vec::new();
    push(&mut pkt, &[0x82])?; // SUBSCRIBE, reserved flags 0b0010
    push_remaining_len(&mut pkt, remaining)?;
    push(&mut pkt, &[0x00, 0x01])?; // packet identifier = 1
    for t in topics {
        push_str(&mut pkt, t)?;
        push(&mut pkt, &[0x00])?; // requested QoS 0
    }
    write_all(socket, &pkt).await?;

    // Largest expected frame: a notify — topic ≤35 + `NOTIFY|title|body`
    // ≤ 7+32+1+96 (caps in notify.rs; longer payloads are foreign and may
    // error the drain — the burst's telemetry is already on the wire by
    // then). SUBACK reuses the same buffer first.
    let mut buf = [0u8; 256];
    let (ty, n) = crate::net::mqtt_climate::read_frame(socket, &mut buf).await?;
    if ty & 0xF0 != 0x90 || n < 2 + topics.len() || buf[2..n].contains(&0x80) {
        return Err("bad SUBACK (announce)");
    }

    // Retained messages, if any, arrive immediately after the SUBACK — up to
    // four with the announce + notify pairs subscribed, so drain frames until
    // the window closes. handle_announce's BUILD_EPOCH gate and notify's
    // duplicate-of-newest guard arbitrate re-deliveries. The deadline
    // expiring = no (more) retained messages (fine); read_frame is
    // cancel-safe enough here — on expiry we only ever DISCONNECT + close.
    let deadline = Instant::now() + ANNOUNCE_WAIT;
    loop {
        match embassy_time::with_deadline(
            deadline,
            crate::net::mqtt_climate::read_frame(socket, &mut buf),
        )
        .await
        {
            Ok(Ok((ty, n))) if ty & 0xF0 == 0x30 => handle_announce_frame(&buf[..n]),
            Ok(Ok(_)) => {}  // unexpected control packet — ignore, keep draining
            Ok(Err(e)) => return Err(e),
            Err(_) => break, // window elapsed — common case
        }
    }
    Ok(())
}

/// Minimal QoS-0 PUBLISH body split (topic + payload) for the burst window's
/// retained frames: OTA announces + notifies (#32). Checked slicing
/// throughout — a malformed frame is dropped, never a panic.
fn handle_announce_frame(body: &[u8]) {
    if body.len() < 2 {
        return;
    }
    let topic_len = ((body[0] as usize) << 8) | body[1] as usize;
    let idx = 2 + topic_len;
    if idx > body.len() {
        return; // topic overruns frame — malformed
    }
    let topic = &body[2..idx];
    let (notify_fleet, notify_device) = crate::net::mqtt_climate::notify_topics();
    if topic == notify_fleet.as_bytes() || topic == notify_device.as_bytes() {
        crate::notify::handle_mqtt(&body[idx..]);
        return;
    }
    if topic != crate::net::ota_http::ANNOUNCE_TOPIC.as_bytes()
        && topic != crate::net::sigil::get().ota_topic.as_bytes()
    {
        return; // only the subscribed topics are expected; anything else is noise
    }
    crate::net::ota_http::handle_announce(&body[idx..]);
}

/// Build the MQTT 3.1.1 CONNECT packet (clean session, optional user/pass).
/// `client_id` is a parameter so the bidirectional climate session can connect
/// under a distinct id (same broker, avoids evicting the telemetry client).
/// `pub(crate)` — reused by [`crate::net::mqtt_climate`].
pub(crate) fn build_connect(
    pkt: &mut Vec<u8, PKT_CAP>,
    client_id: &str,
) -> Result<(), &'static str> {
    // Password without a username is invalid in MQTT 3.1.1; ignore it.
    let user = USER;
    let pass = if user.is_some() { PASS } else { None };

    let mut flags: u8 = 0x02; // clean session
    let mut remaining = 10 + 2 + client_id.len();
    if let Some(u) = user {
        flags |= 0x80;
        remaining += 2 + u.len();
    }
    if let Some(p) = pass {
        flags |= 0x40;
        remaining += 2 + p.len();
    }

    push(pkt, &[0x10])?;
    push_remaining_len(pkt, remaining)?;
    // Protocol name "MQTT", level 4, flags, keepalive.
    push(pkt, &[0x00, 0x04, b'M', b'Q', b'T', b'T', 0x04, flags])?;
    push(pkt, &KEEPALIVE_SECS.to_be_bytes())?;
    push_str(pkt, client_id)?;
    if let Some(u) = user {
        push_str(pkt, u)?;
    }
    if let Some(p) = pass {
        push_str(pkt, p)?;
    }
    Ok(())
}

/// Send one QoS-0 PUBLISH. `pub(crate)` — reused by the climate session for
/// command publishes.
pub(crate) async fn publish(
    socket: &mut TcpSocket<'_>,
    topic: &str,
    payload: &[u8],
    retain: bool,
) -> Result<(), &'static str> {
    let mut pkt: Vec<u8, PKT_CAP> = Vec::new();
    push(&mut pkt, &[0x30 | retain as u8])?;
    push_remaining_len(&mut pkt, 2 + topic.len() + payload.len())?;
    push_str(&mut pkt, topic)?;
    push(&mut pkt, payload)?;
    write_all(socket, &pkt).await
}

/// MQTT variable-length "remaining length": 7 bits per byte, MSB = more.
/// `pub(crate)` — shared framing primitive reused by the climate session.
pub(crate) fn push_remaining_len(
    pkt: &mut Vec<u8, PKT_CAP>,
    mut len: usize,
) -> Result<(), &'static str> {
    loop {
        let mut byte = (len & 0x7F) as u8;
        len >>= 7;
        if len > 0 {
            byte |= 0x80;
        }
        push(pkt, &[byte])?;
        if len == 0 {
            return Ok(());
        }
    }
}

/// UTF-8 string field: u16 big-endian length prefix + bytes.
/// `pub(crate)` — shared framing primitive reused by the climate session.
pub(crate) fn push_str(pkt: &mut Vec<u8, PKT_CAP>, s: &str) -> Result<(), &'static str> {
    push(pkt, &(s.len() as u16).to_be_bytes())?;
    push(pkt, s.as_bytes())
}

/// `pub(crate)` — shared framing primitive reused by the climate session.
pub(crate) fn push(pkt: &mut Vec<u8, PKT_CAP>, bytes: &[u8]) -> Result<(), &'static str> {
    pkt.extend_from_slice(bytes).map_err(|_| "packet too large")
}

/// `pub(crate)` — shared socket helper reused by the climate session.
pub(crate) async fn write_all(
    socket: &mut TcpSocket<'_>,
    mut buf: &[u8],
) -> Result<(), &'static str> {
    while !buf.is_empty() {
        match socket.write(buf).await {
            Ok(0) => return Err("tcp write: connection closed"),
            Ok(n) => buf = &buf[n..],
            Err(_) => return Err("tcp write"),
        }
    }
    Ok(())
}

/// `pub(crate)` — shared socket helper reused by the climate session.
pub(crate) async fn read_exact(
    socket: &mut TcpSocket<'_>,
    buf: &mut [u8],
) -> Result<(), &'static str> {
    let mut filled = 0;
    while filled < buf.len() {
        match socket.read(&mut buf[filled..]).await {
            Ok(0) => return Err("tcp read: connection closed"),
            Ok(n) => filled += n,
            Err(_) => return Err("tcp read"),
        }
    }
    Ok(())
}

/// Parse "a.b.c.d:port". `pub(crate)` — reused by the climate session.
pub(crate) fn parse_broker(s: &str) -> Option<(Ipv4Address, u16)> {
    let (ip_str, port_str) = s.split_once(':')?;
    let port: u16 = port_str.parse().ok()?;
    let mut octets = [0u8; 4];
    let mut parts = ip_str.split('.');
    for octet in &mut octets {
        *octet = parts.next()?.parse().ok()?;
    }
    if parts.next().is_some() {
        return None;
    }
    Some((
        Ipv4Address::new(octets[0], octets[1], octets[2], octets[3]),
        port,
    ))
}

/// Format an integer into `buf`, returning the ASCII digits.
fn fmt_u64(mut n: u64, buf: &mut [u8; 20]) -> &[u8] {
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    &buf[i..]
}
