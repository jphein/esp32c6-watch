//! OTA firmware download over plain HTTP/1.0.
//!
//! Downloads a firmware image into the currently *inactive* OTA slot and,
//! only once the image has been fully written, flips the otadata boot
//! selection to it (state [`OtaImageState::New`]). The running slot is never
//! touched and otadata is never updated for a partial download, so a failed
//! or interrupted update cannot brick the watch — worst case the inactive
//! slot holds garbage that is never selected for boot.
//!
//! The image URL is baked in at build time: `OTA_URL=http://... cargo build`.
//! Plain HTTP only (no TLS, no DNS — the host must be a dotted-quad IPv4).
//!
//! Live deploy server: `http://10.0.11.11:8000/watch.bin` (ubox0, VLAN-11, same
//! subnet as the watch's "roam" WiFi). Set via `OTA_URL` in `.cargo/config.toml`
//! (gitignored). Full deploy flow: `docs/ota-deploy.md`.

use alloc::{format, vec};

use embassy_net::{tcp::TcpSocket, Ipv4Address, Stack};
use embassy_time::{with_timeout, Duration};
use embedded_storage::Storage;
use esp_bootloader_esp_idf::ota::{Ota, OtaImageState};
use esp_bootloader_esp_idf::partitions::{
    self, AppPartitionSubType, DataPartitionSubType, PartitionType,
};
use esp_println::println;

/// Firmware image URL, fixed at build time via the `OTA_URL` env var.
pub const URL: &str = match option_env!("OTA_URL") {
    Some(url) => url,
    None => "http://192.168.4.1:8000/watch.bin",
};

/// True when an explicit `OTA_URL` was baked into this build.
pub const URL_SET: bool = option_env!("OTA_URL").is_some();

/// Overall budget for the whole download + flash write.
const TIMEOUT: Duration = Duration::from_secs(30);
/// Flash write granularity: one 4 KiB sector per `Storage::write`.
const CHUNK: usize = 4096;
/// Progress log granularity.
const LOG_STEP: u32 = 64 * 1024;
/// First byte of every valid ESP-IDF app image.
const ESP_IMAGE_MAGIC: u8 = 0xE9;

/// Download `URL` into the inactive OTA slot and stage it for the next boot.
///
/// Never reboots by itself. On success logs
/// `[OTA] update staged - reboot to apply`.
pub async fn ota_update(
    stack: Stack<'static>,
    flash: &mut esp_storage::FlashStorage<'_>,
) -> Result<(), &'static str> {
    match with_timeout(TIMEOUT, run(stack, flash)).await {
        Ok(result) => result,
        Err(_) => Err("timeout (30s)"),
    }
}

async fn run(
    stack: Stack<'static>,
    flash: &mut esp_storage::FlashStorage<'_>,
) -> Result<(), &'static str> {
    // --- Slot selection: read the partition table + otadata -----------------
    let mut pt_mem = vec![0u8; partitions::PARTITION_TABLE_MAX_LEN];
    let pt = partitions::read_partition_table(flash, &mut pt_mem)
        .map_err(|_| "partition table read failed")?;

    let otadata = pt
        .find_partition(PartitionType::Data(DataPartitionSubType::Ota))
        .map_err(|_| "partition table scan failed")?
        .ok_or("no otadata partition")?;

    let current = {
        let region = otadata.as_embedded_storage(flash);
        let mut ota = Ota::new(region, 2).map_err(|_| "otadata invalid")?;
        ota.current_app_partition().map_err(|_| "otadata read failed")?
    };
    // With empty otadata (Factory) the bootloader falls back to ota_0.
    let target = match current {
        AppPartitionSubType::Ota0 | AppPartitionSubType::Factory => AppPartitionSubType::Ota1,
        AppPartitionSubType::Ota1 => AppPartitionSubType::Ota0,
        _ => return Err("unexpected boot slot"),
    };
    println!("[OTA] current slot {current:?}, writing to {target:?}");

    let target_entry = pt
        .find_partition(PartitionType::App(target))
        .map_err(|_| "partition table scan failed")?
        .ok_or("target ota slot missing")?;
    let slot_size = target_entry.len() as u64;

    // --- HTTP GET ------------------------------------------------------------
    let (addr, port, host, path) = parse_url(URL)?;
    println!("[OTA] GET {URL}");

    let mut rx_buf = vec![0u8; 4096];
    let mut tx_buf = vec![0u8; 512];
    let mut socket = TcpSocket::new(stack, &mut rx_buf, &mut tx_buf);
    socket.set_timeout(Some(Duration::from_secs(10)));
    socket
        .connect((addr, port))
        .await
        .map_err(|_| "connect failed")?;

    let request = format!(
        "GET {path} HTTP/1.0\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n"
    );
    let mut sent = 0;
    while sent < request.len() {
        let n = socket
            .write(&request.as_bytes()[sent..])
            .await
            .map_err(|_| "request send failed")?;
        sent += n;
    }

    // --- Response headers ------------------------------------------------------
    let mut header = [0u8; 1024];
    let mut header_len = 0;
    let body_start = loop {
        if header_len == header.len() {
            return Err("response headers too large");
        }
        let n = socket
            .read(&mut header[header_len..])
            .await
            .map_err(|_| "socket read failed")?;
        if n == 0 {
            return Err("connection closed in headers");
        }
        header_len += n;
        if let Some(pos) = find(&header[..header_len], b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let content_len = parse_headers(&header[..body_start])?;
    if content_len == 0 {
        return Err("empty image");
    }
    if content_len > slot_size {
        return Err("image larger than ota slot");
    }
    println!("[OTA] image size {content_len} bytes");

    // --- Stream the body into the inactive slot -------------------------------
    let mut region = target_entry.as_embedded_storage(flash);
    let mut chunk = vec![0u8; CHUNK];
    let leftover = &header[body_start..header_len];
    chunk[..leftover.len()].copy_from_slice(leftover);
    let mut chunk_len = leftover.len().min(content_len as usize);
    let mut received = chunk_len as u64;
    let mut flashed: u32 = 0;
    let mut next_log = LOG_STEP;

    loop {
        if chunk_len == CHUNK || (received == content_len && chunk_len > 0) {
            if flashed == 0 && chunk[0] != ESP_IMAGE_MAGIC {
                return Err("not an esp app image (bad magic)");
            }
            region
                .write(flashed, &chunk[..chunk_len])
                .map_err(|_| "flash write failed")?;
            flashed += chunk_len as u32;
            chunk_len = 0;
            if flashed >= next_log {
                println!("[OTA] {flashed} / {content_len} bytes");
                next_log += LOG_STEP;
            }
        }
        if received == content_len {
            break;
        }
        let want = (CHUNK - chunk_len).min((content_len - received) as usize);
        let n = socket
            .read(&mut chunk[chunk_len..chunk_len + want])
            .await
            .map_err(|_| "socket read failed")?;
        if n == 0 {
            return Err("connection closed mid-body");
        }
        chunk_len += n;
        received += n as u64;
    }
    drop(region);
    socket.abort();
    println!("[OTA] download complete ({flashed} bytes flashed)");

    // --- Image fully written: flip otadata to the new slot --------------------
    let region = otadata.as_embedded_storage(flash);
    let mut ota = Ota::new(region, 2).map_err(|_| "otadata invalid")?;
    ota.set_current_app_partition(target)
        .map_err(|_| "otadata slot switch failed")?;
    ota.set_current_ota_state(OtaImageState::New)
        .map_err(|_| "otadata state update failed")?;
    println!("[OTA] update staged - reboot to apply");
    Ok(())
}

/// Rollback-safety: confirm the running image is healthy so the bootloader keeps
/// it. A freshly-OTA'd slot is staged as [`OtaImageState::New`]; when the
/// bootloader has auto-rollback enabled it flips that to
/// [`OtaImageState::PendingVerify`] on first boot. If the app never transitions
/// that to [`OtaImageState::Valid`], the bootloader reverts to the previous slot
/// on the next boot (and marks the unconfirmed slot `Aborted`). So a good image
/// only *sticks* once this is called — and a bricked one that never reaches this
/// call auto-rolls-back. Call it once the app has proven itself healthy
/// (peripherals up + the main loop running for a few seconds).
///
/// Transitions both `New` and `PendingVerify` -> `Valid` so the confirm is
/// correct whether or not the bootloader was built with auto-rollback (with it
/// off the state stays `New`; marking it valid is harmless and forward-safe).
///
/// Returns `Ok(true)` if it just marked the slot valid, `Ok(false)` if there was
/// nothing to do (already `Valid`/`Invalid`, or a factory layout with no
/// otadata). Never touches flash beyond the otadata select entry.
pub fn mark_valid_if_pending(
    flash: &mut esp_storage::FlashStorage<'_>,
) -> Result<bool, &'static str> {
    let mut pt_mem = vec![0u8; partitions::PARTITION_TABLE_MAX_LEN];
    let pt = partitions::read_partition_table(flash, &mut pt_mem)
        .map_err(|_| "partition table read failed")?;

    let Some(otadata) = pt
        .find_partition(PartitionType::Data(DataPartitionSubType::Ota))
        .map_err(|_| "partition table scan failed")?
    else {
        // Factory layout (no otadata) — nothing to confirm, nothing to roll back.
        return Ok(false);
    };

    let region = otadata.as_embedded_storage(flash);
    let mut ota = Ota::new(region, 2).map_err(|_| "otadata invalid")?;
    let state = ota.current_ota_state().map_err(|_| "otadata read failed")?;
    match state {
        OtaImageState::New | OtaImageState::PendingVerify => {
            ota.set_current_ota_state(OtaImageState::Valid)
                .map_err(|_| "otadata mark-valid failed")?;
            println!("[OTA] marked current slot VALID (was {state:?}) - rollback cancelled");
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// `http://a.b.c.d[:port][/path]` -> (addr, port, host, path). IPv4 only.
fn parse_url(url: &str) -> Result<(Ipv4Address, u16, &str, &str), &'static str> {
    let rest = url.strip_prefix("http://").ok_or("OTA_URL must be http://")?;
    let (host_port, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match host_port.split_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().map_err(|_| "bad port in OTA_URL")?),
        None => (host_port, 80),
    };
    let mut octets = [0u8; 4];
    let mut parts = host.split('.');
    for octet in &mut octets {
        *octet = parts
            .next()
            .and_then(|p| p.parse().ok())
            .ok_or("OTA_URL host must be a dotted-quad IPv4")?;
    }
    if parts.next().is_some() {
        return Err("OTA_URL host must be a dotted-quad IPv4");
    }
    Ok((
        Ipv4Address::new(octets[0], octets[1], octets[2], octets[3]),
        port,
        host,
        path,
    ))
}

/// Check the status line is 200 and return the Content-Length.
fn parse_headers(header: &[u8]) -> Result<u64, &'static str> {
    let mut lines = header.split(|&b| b == b'\n').map(|l| l.strip_suffix(b"\r").unwrap_or(l));
    let status = lines.next().ok_or("empty response")?;
    // "HTTP/1.x NNN ..."
    let code = status
        .split(|&b| b == b' ')
        .nth(1)
        .and_then(|c| core::str::from_utf8(c).ok())
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or("malformed status line")?;
    if code != 200 {
        println!("[OTA] server returned HTTP {code}");
        return Err("http status not 200");
    }
    for line in lines {
        if let Some((name, value)) = split_header(line) {
            if name.eq_ignore_ascii_case("content-length") {
                return value
                    .trim()
                    .parse::<u64>()
                    .map_err(|_| "bad content-length");
            }
        }
    }
    Err("no content-length header")
}

fn split_header(line: &[u8]) -> Option<(&str, &str)> {
    let line = core::str::from_utf8(line).ok()?;
    let (name, value) = line.split_once(':')?;
    Some((name.trim(), value))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}
