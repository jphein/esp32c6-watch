//! BLE GATT server via trouble-host.
//!
//! Replaces the old raw-HCI advertising/scanning code. esp-radio's
//! `BleConnector` implements bt-hci 0.8's `Transport`, which trouble-host
//! 0.6 wraps with `ExternalController` to run a full host stack.
//!
//! The server exposes the standard Battery Service (0x180F) with a
//! Battery Level characteristic (0x2A19, read + notify). The level is
//! read from [`BATTERY_PERCENT`], which main.rs updates from its AXP2101
//! polling loop.
//!
//! Lifecycle: [`ble_host_task`] is spawned at boot but parks until the
//! watchface BLE button sets [`BLE_START_REQUEST`]. From then on the
//! trouble host owns the controller and loops forever:
//! advertise -> accept connection -> serve GATT + battery notifications
//! -> disconnect -> advertise again. There is no clean way to tear the
//! host down once its runner is started, so "BLE off" requires a reboot
//! (main.rs logs this on subsequent toggles).
//!
//! Scanning: the old device-discovery logging was dropped. trouble's
//! scanner drives the central role of the same host, which conflicts with
//! the single-connection peripheral setup here (and would need extra
//! connection slots + a second command path). Peripheral-only for now.

use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use embassy_futures::join::join;
use embassy_futures::select::select;
use embassy_time::{Duration, Timer};
use esp_println::println;
use esp_radio::ble::controller::BleConnector;
use trouble_host::prelude::*;

/// Battery level in percent, written by main.rs (AXP2101 polling),
/// read by the GATT server for the 0x2A19 characteristic.
pub static BATTERY_PERCENT: AtomicU8 = AtomicU8::new(0);

/// Set to true by the watchface BLE button. The parked host task starts
/// advertising once it sees this. Never cleared: stopping the trouble
/// host requires a reboot.
pub static BLE_START_REQUEST: AtomicBool = AtomicBool::new(false);

/// Advertised device name: the per-device sigil (#34), e.g.
/// "eldritch-lantern" — was a fleet-shared "Rust Watch", which made the two
/// watches indistinguishable in any scanner. `&'static` because the sigil
/// identity lives in a `static` (LazyLock over the efuse MAC).
fn device_name() -> &'static str {
    crate::net::sigil::get().sigil.as_str()
}

/// Deterministic per-device BLE address (#47): a STATIC RANDOM address
/// derived from the factory efuse base MAC — the same address on every boot,
/// so HA/Bermuda room-tracking registrations survive reboots and OTAs.
///
/// Replaces a HARDCODED fleet-shared constant (`C6:83:1E:E3:5A:42`), which was
/// worse than per-boot random: both watches advertised the SAME address, so a
/// scanner tracking one watch could silently follow the other.
///
/// Derivation (documented for HA-side prediction):
///   display address = efuse MAC with its two most significant bits forced to
///   `0b11` (the BLE static-random requirement, Core Spec Vol 6 Part B §1.3):
///     `addr[0] = mac[0] | 0xC0`, `addr[1..6] = mac[1..6]` (MSB-first).
///   trouble's `Address::random` takes the bytes LSB-first, hence the reversal
///   below (empirically anchored: `[0x42,…,0xc6]` scanned as `C6:83:1E:E3:5A:42`).
/// Fleet: efuse `98:A3:16:A7:2F:E4` → BLE `D8:A3:16:A7:2F:E4` (eldritch-lantern)
///        efuse `98:A3:16:A5:A7:F8` → BLE `D8:A3:16:A5:A7:F8` (mythic-throne)
fn stable_address() -> Address {
    let mac = crate::net::sigil::get().mac; // efuse base MAC, MSB-first
    Address::random([mac[5], mac[4], mac[3], mac[2], mac[1], mac[0] | 0xC0])
}

/// Max number of concurrent connections.
const CONNECTIONS_MAX: usize = 1;
/// Max number of L2CAP channels (signal + ATT).
const L2CAP_CHANNELS_MAX: usize = 2;

/// Concrete controller type: trouble's HCI wrapper around esp-radio's
/// connector, with 20 command slots (matches trouble's esp32 examples).
pub type WatchController = ExternalController<BleConnector<'static>, 20>;

// GATT server definition (expanded by trouble-host's derive macros).
#[gatt_server]
struct Server {
    battery_service: BatteryService,
}

/// Standard Battery Service (0x180F).
#[gatt_service(uuid = service::BATTERY)]
struct BatteryService {
    /// Battery Level (0x2A19), percent.
    #[characteristic(uuid = characteristic::BATTERY_LEVEL, read, notify)]
    level: u8,
}

/// The trouble host task. Spawned at boot; parks until the watchface
/// requests BLE, then runs the host + GATT server forever.
#[embassy_executor::task]
pub async fn ble_host_task(controller: WatchController) {
    // Park cheaply until the user asks for BLE.
    while !BLE_START_REQUEST.load(Ordering::Relaxed) {
        Timer::after(Duration::from_millis(250)).await;
    }

    // Deterministic static-random address from the efuse MAC (#47) — stable
    // across reboots/OTAs so the Bermuda registration holds.
    let address = stable_address();
    let mut resources: HostResources<DefaultPacketPool, CONNECTIONS_MAX, L2CAP_CHANNELS_MAX> =
        HostResources::new();
    let stack = trouble_host::new(controller, &mut resources).set_random_address(address);
    let Host {
        mut peripheral,
        mut runner,
        ..
    } = stack.build();

    let server = match Server::new_with_config(GapConfig::Peripheral(PeripheralConfig {
        name: device_name(),
        appearance: &appearance::watch::SMARTWATCH,
    })) {
        Ok(server) => server,
        Err(e) => {
            println!("[BLE] GATT server init failed: {e:?}");
            return;
        }
    };
    println!(
        "[BLE] host up, advertising as '{}' at stable addr {} (#47)",
        device_name(),
        address
    );

    // The host runner must run alongside everything else, forever.
    let host_fut = async {
        loop {
            if let Err(e) = runner.run().await {
                println!("[BLE] host runner error: {e:?}");
                Timer::after(Duration::from_secs(1)).await;
            }
        }
    };

    let gatt_fut = async {
        loop {
            // Keep the readable attribute fresh even while unconnected.
            let pct = BATTERY_PERCENT.load(Ordering::Relaxed);
            let _ = server.set(&server.battery_service.level, &pct);

            match advertise(&mut peripheral, &server).await {
                Ok(conn) => {
                    // Serve GATT events and push battery notifications until
                    // the central disconnects, then go back to advertising.
                    select(gatt_events(&conn), notify_battery(&server, &conn)).await;
                }
                Err(e) => {
                    println!("[BLE] advertise error: {e:?}");
                    Timer::after(Duration::from_secs(3)).await;
                }
            }
        }
    };

    join(host_fut, gatt_fut).await;
}

/// Advertise as the connectable per-device sigil and wait for a central.
/// ADV payload budget: 3 (flags) + 4 (battery uuid) + 2 + sigil (≤ 20) = 29
/// of the 31 legacy-ADV bytes — every corpus name fits (host-tested cap).
async fn advertise<'values, 'server, C: Controller>(
    peripheral: &mut Peripheral<'values, C, DefaultPacketPool>,
    server: &'server Server<'values>,
) -> Result<GattConnection<'values, 'server, DefaultPacketPool>, BleHostError<C::Error>> {
    let mut adv_data = [0; 31];
    let len = AdStructure::encode_slice(
        &[
            AdStructure::Flags(LE_GENERAL_DISCOVERABLE | BR_EDR_NOT_SUPPORTED),
            AdStructure::ServiceUuids16(&[[0x0f, 0x18]]), // Battery Service
            AdStructure::CompleteLocalName(device_name().as_bytes()),
        ],
        &mut adv_data[..],
    )?;
    let advertiser = peripheral
        .advertise(
            &Default::default(),
            Advertisement::ConnectableScannableUndirected {
                adv_data: &adv_data[..len],
                scan_data: &[],
            },
        )
        .await?;
    println!("[BLE] advertising as '{}'", device_name());
    let conn = advertiser.accept().await?.with_attribute_server(server)?;
    println!("[BLE] central connected");
    Ok(conn)
}

/// Answer GATT requests until the connection closes.
async fn gatt_events<P: PacketPool>(conn: &GattConnection<'_, '_, P>) {
    loop {
        match conn.next().await {
            GattConnectionEvent::Disconnected { reason } => {
                println!("[BLE] disconnected: {reason:?}");
                break;
            }
            GattConnectionEvent::Gatt { event } => {
                // Attribute reads/writes are served from the server table;
                // we only need to accept so the reply is sent.
                match event.accept() {
                    Ok(reply) => reply.send().await,
                    Err(e) => println!("[BLE] gatt reply error: {e:?}"),
                }
            }
            _ => {}
        }
    }
}

/// Push battery-level notifications to the connected central whenever the
/// value changes (checked every 10s); ends when the connection drops.
async fn notify_battery<P: PacketPool>(server: &Server<'_>, conn: &GattConnection<'_, '_, P>) {
    let level = server.battery_service.level;
    let mut last = 0xFFu8; // force one initial notification
    loop {
        let pct = BATTERY_PERCENT.load(Ordering::Relaxed);
        if pct != last {
            last = pct;
            if level.notify(conn, &pct).await.is_err() {
                println!("[BLE] notify failed (connection closed?)");
                break;
            }
        }
        Timer::after(Duration::from_secs(10)).await;
    }
}
