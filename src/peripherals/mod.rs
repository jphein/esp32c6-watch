pub mod audio;
pub mod ble;
pub mod config;
pub mod cpu_clock;
// Die-temp helper: pre-staged, wired into main.rs's system-page push once
// light-sleep (#29) frees up main.rs. Unused until then → dead-code warning.
#[allow(dead_code)]
pub mod die_temp;
pub mod imu;
pub mod power;
pub mod power_stats;
pub mod rtc;
pub mod touch;
pub mod wifi;
