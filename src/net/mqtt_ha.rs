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
use embassy_time::{with_timeout, Duration, Instant};
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

const CLIENT_ID: &str = "smolwatch042";
const KEEPALIVE_SECS: u16 = 30;

/// Home Assistant discovery config for the battery sensor (retained).
const DISCOVERY_TOPIC: &str = "homeassistant/sensor/smolwatch/battery/config";
const DISCOVERY_PAYLOAD: &str = concat!(
    r#"{"name":"smol watch battery","state_topic":"smolwatch/battery","#,
    r#""unit_of_measurement":"%","device_class":"battery","#,
    r#""unique_id":"smolwatch_battery","device":{"identifiers":["smolwatch042"],"#,
    r#""name":"smol watch","model":"ESP32-C6-Touch-AMOLED-2.06","#,
    r#""manufacturer":"jphein"}}"#
);

const BATTERY_TOPIC: &str = "smolwatch/battery";
const UPTIME_TOPIC: &str = "smolwatch/uptime";

/// Largest single packet we build (discovery config is ~330 bytes).
/// `pub(crate)` so the climate session reuses the shared packet builders.
pub(crate) const PKT_CAP: usize = 512;

/// Publish the HA discovery config, battery percent, and uptime to the
/// broker. Never fails the caller: logs `[MQTT] published` or
/// `[MQTT] failed: <reason>` and returns. Bounded at 5s wall time.
pub async fn publish_burst(stack: Stack<'static>, batt_pct: u8) {
    match with_timeout(Duration::from_secs(5), burst(stack, batt_pct)).await {
        Ok(Ok(())) => println!("[MQTT] published"),
        Ok(Err(reason)) => println!("[MQTT] failed: {reason}"),
        Err(_) => println!("[MQTT] failed: timeout (5s)"),
    }
}

async fn burst(stack: Stack<'static>, batt_pct: u8) -> Result<(), &'static str> {
    let (ip, port) = parse_broker(BROKER).ok_or("bad MQTT_BROKER (want ip:port)")?;

    let mut rx_buf = [0u8; 256];
    let mut tx_buf = [0u8; 1024];
    let mut socket = TcpSocket::new(stack, &mut rx_buf, &mut tx_buf);
    // 2s (was 4s): the telemetry broker (MQTT_BROKER) may be on a subnet the watch
    // can't reach from its roam VLAN (the SYN is silently dropped, not RST'd), so
    // this timeout governs how long `connect` below blocks the single-threaded
    // executor on a doomed connect. Fail fast so an unreachable broker doesn't
    // freeze the UI/PTT loop for 4s during the boot NTP burst. A reachable broker
    // completes the handshake well inside 2s.
    socket.set_timeout(Some(Duration::from_secs(2)));

    socket
        .connect((ip, port))
        .await
        .map_err(|_| "tcp connect")?;

    // CONNECT -> CONNACK
    let mut pkt: Vec<u8, PKT_CAP> = Vec::new();
    build_connect(&mut pkt, CLIENT_ID)?;
    write_all(&mut socket, &pkt).await?;

    let mut ack = [0u8; 4];
    read_exact(&mut socket, &mut ack).await?;
    if ack[0] != 0x20 || ack[1] != 0x02 {
        return Err("bad CONNACK");
    }
    if ack[3] != 0x00 {
        return Err("broker refused connection (check MQTT_USER/MQTT_PASS)");
    }

    // Discovery config (retained) + state topics (QoS 0).
    publish(&mut socket, DISCOVERY_TOPIC, DISCOVERY_PAYLOAD.as_bytes(), true).await?;

    let mut num = [0u8; 20];
    let batt = fmt_u64(batt_pct as u64, &mut num);
    publish(&mut socket, BATTERY_TOPIC, batt, false).await?;

    let mut num = [0u8; 20];
    let uptime = fmt_u64(Instant::now().as_secs(), &mut num);
    publish(&mut socket, UPTIME_TOPIC, uptime, false).await?;

    // DISCONNECT, then flush so everything hits the wire before close.
    write_all(&mut socket, &[0xE0, 0x00]).await?;
    socket.flush().await.map_err(|_| "tcp flush")?;
    socket.close();
    Ok(())
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
