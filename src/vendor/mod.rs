//! Vendored third-party drivers that cannot be consumed as crates.
//!
//! Currently: esp-radio 0.18's IEEE 802.15.4 PHY/MAC driver, copied verbatim
//! (minus the coex path) to bypass esp-radio's `build.rs` panic that forbids
//! `wifi` + `ieee802154` in one esp-radio build (#23 / issue #2, Option A).
//! We keep esp-radio `wifi`-only and vendor the 154 code here, so a single
//! firmware image can run WiFi/ESP-NOW *and* switch to an 802.15.4 promiscuous
//! sniffer at runtime — no reboot, no two images. The blob symbols the driver
//! needs (`bt_bb_*`, PHY) are already linked by esp-wifi-sys-esp32c6 via the
//! wifi build (verified by `nm`). Compiled unconditionally so this link-test
//! proves coexistence; the Radio Scan flow wires it up later (RS3–RS6).
#![allow(dead_code, unused)]

pub mod ieee802154;
