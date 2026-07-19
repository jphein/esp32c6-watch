//! Runtime CPU frequency switching (DVFS) — C6 stub.
//!
//! The S3 firmware pokes the S3 SYSTEM registers directly to switch
//! 80/160/240 MHz at runtime. The C6 clock tree lives in the PCR block and
//! its max is 160 MHz; hand-poking it without the TRM open is a good way to
//! hang the chip, so until this is properly ported the watch runs at a fixed
//! 160 MHz and the CPU button on the watchface is cosmetic.

pub fn set_cpu_mhz(_mhz: u16) -> u16 {
    log::warn!("DVFS not ported to C6 yet - staying at 160MHz");
    160
}
