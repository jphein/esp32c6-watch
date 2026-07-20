//! One-shot Open-Meteo weather fetch over plain HTTP/1.0.
//!
//! No TLS on the watch, so this talks to api.open-meteo.com on port 80.
//! The host is resolved with embassy-net's DNS socket (DHCP supplies the
//! resolver); if the lookup fails we fall back to a resolved IP baked in at
//! build review time, still sending the proper Host header (Open-Meteo is
//! name-routed behind a shared frontend).
//!
//! Called once per WiFi window, right after the NTP/MQTT burst and before
//! the firmware drops the association. Fire-and-forget like mqtt_ha: any
//! failure logs `[WX] failed: ...` and returns None — boot never blocks for
//! more than 8s and never breaks on a bad fetch.
//!
//! JSON parsing is deliberately minimal: find the `"temperature_2m":` and
//! `"weather_code":` keys whose value is numeric (the `current_units`
//! object repeats the same keys with string values) and parse the digits.
//! No serde, no allocation.

use embassy_net::{dns::DnsQueryType, tcp::TcpSocket, IpAddress, Ipv4Address, Stack};
use embassy_time::{with_timeout, Duration};
use esp_println::println;

/// Seattle-ish, current temp + WMO weather code, Fahrenheit.
const HOST: &str = "api.open-meteo.com";
const PATH: &str = "/v1/forecast?latitude=47.6&longitude=-122.3\
    &current=temperature_2m,weather_code&temperature_unit=fahrenheit";

/// Fallback if DNS is unavailable (api.open-meteo.com as of 2026-07).
const FALLBACK_IP: Ipv4Address = Ipv4Address::new(188, 40, 99, 226);

/// Everything the watchface needs from the response.
#[derive(Clone, Copy, Debug)]
pub struct Weather {
    /// Current temperature, degrees Fahrenheit, rounded to nearest.
    pub temp_f: i16,
    /// WMO weather interpretation code (0 = clear ... 99 = thunderstorm).
    pub code: u8,
}

/// Fetch current weather. Never fails the caller: logs `[WX] ...` and
/// returns `None` on any error. Bounded at 8s wall time.
pub async fn fetch(stack: Stack<'static>) -> Option<Weather> {
    match with_timeout(Duration::from_secs(8), fetch_inner(stack)).await {
        Ok(Ok(wx)) => {
            println!("[WX] {}F, WMO code {}", wx.temp_f, wx.code);
            Some(wx)
        }
        Ok(Err(reason)) => {
            println!("[WX] failed: {reason}");
            None
        }
        Err(_) => {
            println!("[WX] failed: timeout (8s)");
            None
        }
    }
}

async fn fetch_inner(stack: Stack<'static>) -> Result<Weather, &'static str> {
    // Resolve, with a hardcoded fallback so a broken resolver doesn't kill
    // the feature outright.
    let addr = match stack.dns_query(HOST, DnsQueryType::A).await {
        Ok(addrs) => addrs.first().copied().unwrap_or(IpAddress::from(FALLBACK_IP)),
        Err(_) => {
            println!("[WX] dns failed, using fallback IP");
            IpAddress::from(FALLBACK_IP)
        }
    };

    let mut rx_buf = [0u8; 1024];
    let mut tx_buf = [0u8; 512];
    let mut socket = TcpSocket::new(stack, &mut rx_buf, &mut tx_buf);
    socket.set_timeout(Some(Duration::from_secs(7)));

    socket.connect((addr, 80)).await.map_err(|_| "tcp connect")?;

    // HTTP/1.0 implies Connection: close, so "read until EOF" frames the
    // response for us — no chunked encoding, no keep-alive.
    let mut req: heapless::Vec<u8, 256> = heapless::Vec::new();
    req.extend_from_slice(b"GET ").map_err(|_| "req too large")?;
    req.extend_from_slice(PATH.as_bytes()).map_err(|_| "req too large")?;
    req.extend_from_slice(b" HTTP/1.0\r\nHost: ").map_err(|_| "req too large")?;
    req.extend_from_slice(HOST.as_bytes()).map_err(|_| "req too large")?;
    req.extend_from_slice(b"\r\n\r\n").map_err(|_| "req too large")?;
    write_all(&mut socket, &req).await?;

    // Headers + JSON body comfortably fit in 1.5KB; anything past that is
    // ignored (both keys appear early in the body).
    let mut resp = [0u8; 1536];
    let mut filled = 0;
    loop {
        match socket.read(&mut resp[filled..]).await {
            Ok(0) => break,
            Ok(n) => {
                filled += n;
                if filled == resp.len() {
                    break;
                }
            }
            Err(_) => {
                if filled == 0 {
                    return Err("tcp read");
                }
                break; // parse whatever we already have
            }
        }
    }
    let resp = &resp[..filled];

    if !resp.starts_with(b"HTTP/1.0 200") && !resp.starts_with(b"HTTP/1.1 200") {
        return Err("http status not 200");
    }

    let temp = find_number(resp, b"\"temperature_2m\":").ok_or("no temperature_2m")?;
    let code = find_number(resp, b"\"weather_code\":").ok_or("no weather_code")?;

    Ok(Weather {
        temp_f: temp.clamp(-999, 999) as i16,
        code: code.clamp(0, 255) as u8,
    })
}

/// Find `key` followed by a numeric value and parse it, rounded to the
/// nearest integer. Occurrences where the value is not numeric (e.g. the
/// unit strings in `current_units`) are skipped.
fn find_number(hay: &[u8], key: &[u8]) -> Option<i32> {
    let mut start = 0;
    while let Some(pos) = find(&hay[start..], key) {
        let val = &hay[start + pos + key.len()..];
        if let Some(n) = parse_rounded(val) {
            return Some(n);
        }
        start += pos + key.len();
    }
    None
}

/// Parse `-?digits(.digits)?`, rounding on the first fractional digit.
/// Returns None if the slice doesn't start with a number.
fn parse_rounded(s: &[u8]) -> Option<i32> {
    let mut i = 0;
    let neg = *s.first()? == b'-';
    if neg {
        i = 1;
    }
    let mut int: i32 = 0;
    let mut digits = 0;
    while let Some(&c) = s.get(i) {
        if !c.is_ascii_digit() {
            break;
        }
        int = int.saturating_mul(10).saturating_add((c - b'0') as i32);
        digits += 1;
        i += 1;
    }
    if digits == 0 {
        return None;
    }
    let mut round_up = false;
    if s.get(i) == Some(&b'.') {
        if let Some(&frac) = s.get(i + 1) {
            round_up = frac.is_ascii_digit() && frac >= b'5';
        }
    }
    if round_up {
        int += 1;
    }
    Some(if neg { -int } else { int })
}

/// Naive substring search (haystack is ~1.5KB, run once per WiFi window).
fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

async fn write_all(socket: &mut TcpSocket<'_>, mut buf: &[u8]) -> Result<(), &'static str> {
    while !buf.is_empty() {
        match socket.write(buf).await {
            Ok(0) => return Err("tcp write: connection closed"),
            Ok(n) => buf = &buf[n..],
            Err(_) => return Err("tcp write"),
        }
    }
    Ok(())
}
