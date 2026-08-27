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
//!
//! **Signed (#489):** before the first flash write this path fetches
//! `<image-url>.manifest` and ed25519-verifies the fleet-shape manifest
//! M = `"build|size|sha256hex"`; the streamed image digest must match the
//! signed sha256 before the otadata flip. An unsigned deploy host is refused
//! — same posture as the mesh path and the fleet flavor's both paths.
//! Plain HTTP only (no TLS, no DNS — the host must be a dotted-quad IPv4).
//!
//! Live deploy server: `http://10.0.11.11:8000/watch.bin` (ubox0, VLAN-11, same
//! subnet as the watch's "roam" WiFi). Set via `OTA_URL` in `.cargo/config.toml`
//! (gitignored). Full deploy flow: `docs/ota-deploy.md`.
//!
//! **Push OTA** (`tools/ota_push.sh`): the deploy host publishes a *retained*
//! MQTT announce (`OTA|<build_id>|<url>`) to [`ANNOUNCE_TOPIC`]; both MQTT
//! paths (boot burst + climate session) subscribe and feed the payload to
//! [`handle_announce`], which gates on [`BUILD_EPOCH`] monotonicity and posts
//! an [`Announce`] for main.rs to [`take_announce`] — the same one-tap update
//! flow as the Settings button, zero-touch from any screen. Retained is what
//! makes push work on a single bursty radio: a watch offline at publish time
//! picks the announce up on its next MQTT window.

use core::cell::RefCell;

use alloc::{format, vec};

use embassy_net::{tcp::TcpSocket, Ipv4Address, Stack};
use embassy_sync::blocking_mutex::{raw::CriticalSectionRawMutex, Mutex as BlockingMutex};
use embassy_time::{with_timeout, Duration};
use embedded_storage::Storage;
use esp_bootloader_esp_idf::ota::{Ota, OtaImageState};
use esp_bootloader_esp_idf::partitions::{
    self, AppPartitionSubType, DataPartitionSubType, PartitionType,
};
use esp_println::println;
use heapless::String;
use sha2::{Digest, Sha256};

/// Firmware image URL, fixed at build time via the `OTA_URL` env var.
pub const URL: &str = match option_env!("OTA_URL") {
    Some(url) => url,
    None => "http://192.168.4.1:8000/watch.bin",
};

/// True when an explicit `OTA_URL` was baked into this build.
pub const URL_SET: bool = option_env!("OTA_URL").is_some();

/// Build id of the RUNNING firmware, baked at compile time via the `OTA_BUILD`
/// env var (unix-seconds, stamped into `.cargo/config.toml [env]` by
/// `tools/ota_push.sh`). `0` when unset (dev builds). The push-OTA
/// monotonicity gate: an announce only triggers when its build id is
/// **strictly greater** — after the post-OTA reboot the still-retained
/// announce carries `build_id == BUILD_EPOCH` and is rejected, so a retained
/// announce can never re-trigger-loop the watch.
pub const BUILD_EPOCH: u64 = match option_env!("OTA_BUILD") {
    Some(s) => parse_u64_or_zero(s),
    None => 0,
};

/// MQTT topic the deploy host publishes the retained OTA announce to.
pub const ANNOUNCE_TOPIC: &str = "watch/ota/announce";

/// Max announce-URL length we accept (baked URL is ~31 chars; generous).
pub const ANNOUNCE_URL_CAP: usize = 96;

/// An accepted (gate-passed) push-OTA announce, posted for main.rs.
pub struct Announce {
    /// The announced build id (unix-seconds; `> BUILD_EPOCH` by construction).
    pub build: u64,
    /// Image URL override; `None` = use the baked [`URL`].
    pub url: Option<String<ANNOUNCE_URL_CAP>>,
}

/// Latest accepted announce, written by the MQTT rx paths (boot burst /
/// climate session task), consumed by the main loop. A blocking critical-
/// section cell (single write + single take per announce, never held across
/// an await).
static PENDING_ANNOUNCE: BlockingMutex<CriticalSectionRawMutex, RefCell<Option<Announce>>> =
    BlockingMutex::new(RefCell::new(None));

/// Overall budget for the whole download + flash write. Generous on purpose:
/// a ~3.8 MB image on a slow/contended link (single radio, weak RSSI) can
/// legitimately take minutes — the old 30s cap killed a real on-glass update
/// mid-transfer (died past 1 MB). [`STALL_TIMEOUT`] is what catches a
/// genuinely dead transfer; this is only a hard cap.
const TIMEOUT: Duration = Duration::from_secs(300);
/// Per-read inactivity budget: if the socket produces no data for this long
/// the transfer is declared stalled. Distinct from the overall [`TIMEOUT`]
/// cap so the error names which failure mode actually happened —
/// "stalled (20s, no data)" (server/link went quiet mid-transfer) vs
/// "timeout (5 min overall)" (transfer alive but too slow to finish).
const STALL_TIMEOUT: Duration = Duration::from_secs(20);
/// Flash write granularity: one 4 KiB sector per `Storage::write`.
const CHUNK: usize = 4096;
/// Progress log granularity.
const LOG_STEP: u32 = 64 * 1024;
/// First byte of every valid ESP-IDF app image.
const ESP_IMAGE_MAGIC: u8 = 0xE9;

/// Const decimal parser for [`BUILD_EPOCH`]: any non-digit (or empty) → 0,
/// so a malformed `OTA_BUILD` degrades to "dev build", never a compile error.
const fn parse_u64_or_zero(s: &str) -> u64 {
    let bytes = s.as_bytes();
    if bytes.is_empty() {
        return 0;
    }
    let mut n: u64 = 0;
    let mut i = 0;
    while i < bytes.len() {
        let d = bytes[i];
        if d < b'0' || d > b'9' {
            return 0;
        }
        n = n * 10 + (d - b'0') as u64;
        i += 1;
    }
    n
}

/// Feed an inbound `watch/ota/announce` payload through the monotonicity gate.
///
/// Payload contract (from `tools/ota_push.sh`): `OTA|<build_id>[|<url>]`,
/// where `<build_id>` is decimal unix-seconds and `<url>` (optional, may be
/// empty) overrides the baked [`URL`]. Accepted announces (build id strictly
/// greater than the running [`BUILD_EPOCH`]) are posted for
/// [`take_announce`]; everything else is logged and dropped. Malformed
/// payloads never panic — bounded, UTF-8 checked, quietly rejected.
///
/// Called from both MQTT rx paths (boot burst + climate session). Non-async,
/// no locks held across awaits.
pub fn handle_announce(payload: &[u8]) {
    let Ok(text) = core::str::from_utf8(payload) else {
        println!("[OTA] announce rejected (not utf-8)");
        return;
    };
    // An empty retained-clear (`mosquitto_pub -r -n`) is not an announce.
    if text.is_empty() {
        return;
    }
    let mut parts = text.split('|');
    let (Some("OTA"), Some(build_str)) = (parts.next(), parts.next()) else {
        println!("[OTA] announce rejected (malformed: {text})");
        return;
    };
    let Ok(build) = build_str.parse::<u64>() else {
        println!("[OTA] announce rejected (bad build id: {build_str})");
        return;
    };
    let url = match parts.next() {
        None | Some("") => None, // no override — use the baked URL
        Some(u) => {
            if !u.starts_with("http://") {
                println!("[OTA] announce rejected (url not http://): {u}");
                return;
            }
            let mut owned: String<ANNOUNCE_URL_CAP> = String::new();
            if owned.push_str(u).is_err() {
                println!("[OTA] announce rejected (url too long)");
                return;
            }
            Some(owned)
        }
    };
    println!("[OTA] announce received: build {build} (running {})", BUILD_EPOCH);
    if build <= BUILD_EPOCH {
        println!("[OTA] announce rejected (build {build} <= running {})", BUILD_EPOCH);
        return;
    }
    let refused = LAST_REFUSED_BUILD.lock(|cell| *cell.borrow());
    if build == refused {
        println!(
            "[OTA] announce rejected (build {build} was refused by a pre-write check; \
             clear or replace the retained announce)"
        );
        return;
    }
    println!("[OTA] announce accepted (build {build} > running {})", BUILD_EPOCH);
    CURRENT_BUILD.lock(|cell| *cell.borrow_mut() = build);
    PENDING_ANNOUNCE.lock(|cell| cell.borrow_mut().replace(Announce { build, url }));
}

/// Take the pending accepted announce, if any (clears it). Polled by main.rs.
pub fn take_announce() -> Option<Announce> {
    PENDING_ANNOUNCE.lock(|cell| cell.borrow_mut().take())
}

/// The build id of the announce currently being acted on (set on accept), and
/// the most recent build whose image was REFUSED by a deterministic pre-write
/// check (wrong chip / bad magic / oversized). A retained announce re-arrives
/// on every announce window; without this memory a refused build re-queues an
/// endless download-refuse loop — observed live on the CYD-C5 (2026-08-25),
/// where each round also cost a multi-MB fetch. No 64-bit atomics on
/// riscv32imac, hence the critical-section cells (PENDING_ANNOUNCE's pattern).
static CURRENT_BUILD: BlockingMutex<CriticalSectionRawMutex, RefCell<u64>> =
    BlockingMutex::new(RefCell::new(0));
static LAST_REFUSED_BUILD: BlockingMutex<CriticalSectionRawMutex, RefCell<u64>> =
    BlockingMutex::new(RefCell::new(0));

/// net_task calls this when a download ends in a `refused:` error — the
/// verdict is about the IMAGE BYTES, so retrying the same build is pointless
/// and the announce that named it must stop re-queuing it.
pub fn mark_current_build_refused() {
    let b = CURRENT_BUILD.lock(|cell| *cell.borrow());
    if b != 0 {
        LAST_REFUSED_BUILD.lock(|cell| *cell.borrow_mut() = b);
        println!("[OTA] build {b} marked refused - its announce will be ignored");
    }
}

/// Download the firmware image into the inactive OTA slot and stage it for
/// the next boot. `url_override` (from a push announce) replaces the baked
/// [`URL`] for this one download.
///
/// `flash` is the shared [`crate::FlashMutex`]; it is locked **per operation**
/// (the table/otadata reads, each 4 KB chunk write, the final otadata flip) —
/// never across a socket await — so the main loop's config saves stay bounded
/// while a download is in flight.
///
/// Never reboots by itself. On success logs
/// `[OTA] update staged - reboot to apply`.
///
/// `progress` is called with `(bytes_flashed, content_length)` — once when the
/// headers land (0, total) and after every 4 KB chunk — so a live UI (#53:
/// net_task publishes it through the net-state signal) can render the download
/// without polling. Must be cheap and non-blocking (it runs between socket
/// reads); pass `|_, _| {}` to opt out.
pub async fn ota_update(
    stack: Stack<'static>,
    flash: &'static crate::FlashMutex,
    url_override: Option<&str>,
    progress: fn(u32, u32),
) -> Result<(), &'static str> {
    let url = url_override.unwrap_or(URL);
    match with_timeout(TIMEOUT, run(stack, flash, url, progress)).await {
        Ok(result) => result,
        Err(_) => Err("timeout (5 min overall)"),
    }
}

async fn run(
    stack: Stack<'static>,
    flash: &'static crate::FlashMutex,
    url: &str,
    progress: fn(u32, u32),
) -> Result<(), &'static str> {
    // --- Slot selection: read the partition table + otadata -----------------
    // The returned table borrows `pt_mem`, NOT the flash handle (same pattern
    // as the boot-time scan in main.rs), so the lock guards can stay scoped.
    let mut pt_mem = vec![0u8; partitions::PARTITION_TABLE_MAX_LEN];
    let pt = partitions::read_partition_table(&mut *flash.lock().await, &mut pt_mem)
        .map_err(|_| "partition table read failed")?;

    let otadata = pt
        .find_partition(PartitionType::Data(DataPartitionSubType::Ota))
        .map_err(|_| "partition table scan failed")?
        .ok_or("no otadata partition")?;

    // #55: the RUNNING slot comes from the MMU (which physical flash page the
    // CPU is executing from — `booted_partition` reads MMU entry 0), NEVER
    // from otadata. otadata is a boot *request*, not a boot *fact*: after
    // #50's re-partition, stale otadata still said "Ota1, Valid" while ota_1
    // was empty — the bootloader fell back to ota_0, but the old code here
    // (`Ota::current_app_partition`) believed otadata, picked "the other
    // slot" = ota_0, and streamed the download over the running image
    // (sector erase @0x152000 + replanted app-desc SHA @0x100B0 → checksum
    // fail → boot-loop brick, zero user interaction required).
    let booted = pt
        .booted_partition()
        .map_err(|_| "booted-slot probe failed")?
        .ok_or("booted slot not in partition table")?;
    let current = match booted.partition_type() {
        PartitionType::App(sub) => sub,
        _ => return Err("booted partition is not an app slot"),
    };
    let target = match current {
        AppPartitionSubType::Ota0 | AppPartitionSubType::Factory => AppPartitionSubType::Ota1,
        AppPartitionSubType::Ota1 => AppPartitionSubType::Ota0,
        _ => return Err("unexpected boot slot"),
    };
    println!(
        "[OTA] running from {current:?} @{:#x} (MMU), writing to {target:?}",
        booted.offset()
    );

    let target_entry = pt
        .find_partition(PartitionType::App(target))
        .map_err(|_| "partition table scan failed")?
        .ok_or("target ota slot missing")?;
    // Belt and braces (#55): whatever the selection above computed, refuse
    // outright if the write target is the partition we are executing from.
    // (The FlashMutex's GuardedFlash range-check backstops this again at
    // every individual write.)
    if target_entry.offset() == booted.offset() {
        // "refused:" PREFIX, not suffix — net_task classifies terminal errors by
        // e.starts_with("refused:"), and a suffix reads identically to a human
        // while being invisible to the classifier (morpheus caught his chip-id
        // string with exactly that shape; this line had it too). A retry can
        // never make the running slot stop being the target slot.
        return Err("refused: target slot == running slot");
    }
    let slot_size = target_entry.len() as u64;

    // --- #489: signed manifest, verified BEFORE the first flash write ---------
    // The mesh path verifies ed25519 before trusting a byte; the HTTP path now
    // does the same. `<image-url>.manifest` = M ("build|size|sha256hex") + sig.
    // BUILD_EPOCH monotonicity re-checked against the SIGNED build id, so a
    // forged announce cannot ride a stale-but-genuine manifest downgrade.
    let manifest = {
        let mut murl = alloc::string::String::from(url);
        murl.push_str(".manifest");
        println!("[OTA] GET {murl} (signed manifest)");
        let body = http_get_small(stack, &murl, 512).await?;
        parse_manifest(&body)?
    };
    if manifest.build <= BUILD_EPOCH {
        return Err("refused: signed manifest build <= running build");
    }
    println!(
        "[OTA] manifest verified (ed25519 ok): build {} size {}",
        manifest.build, manifest.size
    );
    if manifest.size == 0 || manifest.size > slot_size {
        return Err("refused: signed size empty or larger than ota slot");
    }

    // --- HTTP GET ------------------------------------------------------------
    let (addr, port, host, path) = parse_url(url)?;
    println!("[OTA] GET {url}");

    // No blanket socket inactivity timeout — every await below carries its own
    // explicit STALL_TIMEOUT so the error can NAME the phase that went quiet
    // (JP kept hitting an undiagnosable generic "timeout").
    let mut rx_buf = vec![0u8; 4096];
    let mut tx_buf = vec![0u8; 512];
    let mut socket = TcpSocket::new(stack, &mut rx_buf, &mut tx_buf);

    // EVERY exit of the network phase funnels through the single abort() below
    // (#refusal-path, 2026-08-25): the old shape returned straight out of this
    // function from a dozen error sites with the socket still open — observed
    // live on the CYD-C5 as a zero-window deadlock the server could not shake
    // (send-queue frozen at the same byte count twice, the client re-wedging
    // from the SAME source port on retry) with the heap bleeding under it.
    // `break 'net` replaces `return`/`?` inside the phase; nothing else may
    // leave it. abort() (RST) rather than close(): a refused 3.4 MB body must
    // never be politely drained.
    // #489: streamed digest of exactly the bytes written to flash; compared
    // against the SIGNED sha256 before the otadata flip. Lives outside the
    // network block so the verdict survives the socket teardown.
    let mut hasher = Sha256::new();
    let flashed = 'net: {
        match with_timeout(STALL_TIMEOUT, socket.connect((addr, port))).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => break 'net Err("connect refused/reset"),
            Err(_) => break 'net Err("connect timeout (10s connect, server down?)"),
        }

        let request = format!(
            "GET {path} HTTP/1.0\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n"
        );
        let mut sent = 0;
        while sent < request.len() {
            let n = match with_timeout(STALL_TIMEOUT, socket.write(&request.as_bytes()[sent..]))
                .await
            {
                Ok(Ok(n)) => n,
                Ok(Err(_)) => break 'net Err("request send failed"),
                Err(_) => break 'net Err("stalled sending request (10s)"),
            };
            sent += n;
        }

        // --- Response headers --------------------------------------------------
        let mut header = [0u8; 1024];
        let mut header_len = 0;
        let body_start = loop {
            if header_len == header.len() {
                break 'net Err("response headers too large");
            }
            let n = match with_timeout(STALL_TIMEOUT, socket.read(&mut header[header_len..]))
                .await
            {
                Ok(Ok(n)) => n,
                Ok(Err(_)) => break 'net Err("connection reset in headers"),
                Err(_) => break 'net Err("stalled in headers (10s, no data)"),
            };
            if n == 0 {
                break 'net Err("connection closed in headers");
            }
            header_len += n;
            if let Some(pos) = find(&header[..header_len], b"\r\n\r\n") {
                break pos + 4;
            }
        };
        let content_len = match parse_headers(&header[..body_start]) {
            Ok(v) => v,
            Err(e) => break 'net Err(e),
        };
        if content_len == 0 {
            // "refused:" marks a DETERMINISTIC verdict about the image itself —
            // net_task gives up immediately (no retries: the same bytes would
            // come back) and remembers the build so the retained announce
            // cannot re-queue an endless download-refuse loop.
            break 'net Err("refused: empty image");
        }
        if content_len > slot_size {
            break 'net Err("refused: image larger than ota slot");
        }
        // #489: the body must be byte-for-byte the signed object.
        if content_len != manifest.size {
            break 'net Err("refused: image size != signed manifest size");
        }
        println!("[OTA] image size {content_len} bytes");
        progress(0, content_len as u32);

        // --- Stream the body into the inactive slot ---------------------------
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
                    break 'net Err("refused: not an esp app image (bad magic)");
                }
                hasher.update(&chunk[..chunk_len]);
                {
                    // One 4 KB sector per lock: a concurrent config save waits at
                    // most one program cycle, never the whole download.
                    let mut f = flash.lock().await;
                    let mut region = target_entry.as_embedded_storage(&mut *f);
                    if region.write(flashed, &chunk[..chunk_len]).is_err() {
                        break 'net Err("flash write failed");
                    }
                }
                flashed += chunk_len as u32;
                chunk_len = 0;
                progress(flashed, content_len as u32);
                if flashed >= next_log {
                    println!("[OTA] {flashed} / {content_len} bytes");
                    next_log += LOG_STEP;
                }
            }
            if received == content_len {
                break 'net Ok(flashed);
            }
            let want = (CHUNK - chunk_len).min((content_len - received) as usize);
            let n = match with_timeout(
                STALL_TIMEOUT,
                socket.read(&mut chunk[chunk_len..chunk_len + want]),
            )
            .await
            {
                Ok(Ok(n)) => n,
                Ok(Err(_)) => break 'net Err("connection reset mid-body"),
                Err(_) => break 'net Err("stalled (20s, no data)"),
            };
            if n == 0 {
                break 'net Err("connection closed mid-body");
            }
            chunk_len += n;
            received += n as u64;
        }
    };
    socket.abort();
    let flashed = flashed?;
    println!("[OTA] download complete ({flashed} bytes flashed)");

    // --- #489: the written bytes must BE the signed object --------------------
    // Refused (not retried): the same URL serves the same bytes, and a digest
    // mismatch against a VALID signature means the image on the host is not
    // the image that was signed.
    let digest = hasher.finalize();
    if digest[..] != manifest.sha256[..] {
        return Err("refused: image sha256 != signed manifest");
    }
    println!("[OTA] image digest matches signed manifest");

    // --- Image fully written: flip otadata to the new slot --------------------
    {
        let mut f = flash.lock().await;
        let region = otadata.as_embedded_storage(&mut *f);
        let mut ota = Ota::new(region, 2).map_err(|_| "otadata invalid")?;
        ota.set_current_app_partition(target)
            .map_err(|_| "otadata slot switch failed")?;
        ota.set_current_ota_state(OtaImageState::New)
            .map_err(|_| "otadata state update failed")?;
    }
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
    flash: &mut impl embedded_storage::Storage,
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
/// One small HTTP/1.0 GET (#489 manifest fetch). Same socket discipline as
/// the image download: explicit stall timeouts, abort() on every exit.
async fn http_get_small(
    stack: Stack<'static>,
    url: &str,
    cap: usize,
) -> Result<alloc::vec::Vec<u8>, &'static str> {
    let (addr, port, host, path) = parse_url(url)?;
    let mut rx_buf = vec![0u8; 2048];
    let mut tx_buf = vec![0u8; 512];
    let mut socket = TcpSocket::new(stack, &mut rx_buf, &mut tx_buf);
    let result = 'net: {
        match with_timeout(STALL_TIMEOUT, socket.connect((addr, port))).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => break 'net Err("manifest connect refused/reset"),
            Err(_) => break 'net Err("manifest connect timeout"),
        }
        let request = format!(
            "GET {path} HTTP/1.0\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n"
        );
        let mut sent = 0;
        while sent < request.len() {
            let n = match with_timeout(STALL_TIMEOUT, socket.write(&request.as_bytes()[sent..]))
                .await
            {
                Ok(Ok(n)) => n,
                Ok(Err(_)) => break 'net Err("manifest request send failed"),
                Err(_) => break 'net Err("manifest stalled sending request"),
            };
            sent += n;
        }
        let mut buf = alloc::vec::Vec::new();
        loop {
            if buf.len() > cap + 1024 {
                break 'net Err("refused: manifest response too large");
            }
            let mut tmp = [0u8; 512];
            let n = match with_timeout(STALL_TIMEOUT, socket.read(&mut tmp)).await {
                Ok(Ok(n)) => n,
                Ok(Err(_)) => break 'net Err("manifest connection reset"),
                Err(_) => break 'net Err("manifest stalled"),
            };
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
        }
        let body_start = match find(&buf, b"\r\n\r\n") {
            Some(pos) => pos + 4,
            None => break 'net Err("manifest response malformed"),
        };
        if parse_headers(&buf[..body_start]).is_err() {
            // A 404 here is DETERMINISTIC: the deploy host serves no manifest
            // for this image, and retrying fetches the same absence.
            break 'net Err("refused: no signed manifest beside the image");
        }
        buf.drain(..body_start);
        Ok(buf)
    };
    socket.abort();
    result
}

/// Signed OTA manifest (#489): the fleet's trust shape on the HTTP path.
///
/// Served by the deploy host at `<image-url>.manifest` as two lines:
/// line 1 = M exactly as signed (`build|size|sha256hex`), line 2 = the
/// 128-hex-char ed25519 signature over M. The signature — not the transport,
/// not the broker ACL — is the trust: `verify_signature_with` against the
/// fleet public key, RFC 8032 strict, fail-closed.
pub struct SignedManifest {
    pub build: u64,
    pub size: u64,
    pub sha256: [u8; 32],
}

/// Parse + verify a `.manifest` body. Any malformation or a bad signature is
/// a refusal, never a panic.
fn parse_manifest(body: &[u8]) -> Result<SignedManifest, &'static str> {
    let text = core::str::from_utf8(body).map_err(|_| "refused: manifest not utf-8")?;
    let mut lines = text.lines();
    let m = lines.next().ok_or("refused: manifest empty")?.trim();
    let sig_hex = lines.next().ok_or("refused: manifest missing signature")?.trim();
    let mut sig = [0u8; 64];
    if sig_hex.len() != 128 {
        return Err("refused: manifest signature not 128 hex chars");
    }
    for (i, byte) in sig.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&sig_hex[i * 2..i * 2 + 2], 16)
            .map_err(|_| "refused: manifest signature not hex")?;
    }
    if !ota_proto::verify_signature_with(&ota_proto::OTA_SIGNING_PUBKEY, m.as_bytes(), &sig) {
        return Err("refused: manifest signature invalid");
    }
    // Only after the signature holds do the fields mean anything.
    let mut it = m.split('|');
    let build = it
        .next()
        .and_then(|v| v.parse::<u64>().ok())
        .ok_or("refused: manifest build unparsable")?;
    let size = it
        .next()
        .and_then(|v| v.parse::<u64>().ok())
        .ok_or("refused: manifest size unparsable")?;
    let sha_hex = it.next().ok_or("refused: manifest sha missing")?;
    if sha_hex.len() != 64 {
        return Err("refused: manifest sha not 64 hex chars");
    }
    let mut sha256 = [0u8; 32];
    for (i, byte) in sha256.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&sha_hex[i * 2..i * 2 + 2], 16)
            .map_err(|_| "refused: manifest sha not hex")?;
    }
    Ok(SignedManifest { build, size, sha256 })
}

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
