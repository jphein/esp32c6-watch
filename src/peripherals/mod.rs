pub mod audio;
pub mod ble;
pub mod config;
pub mod cpu_clock;
pub mod imu;
// MC2 mic capture (I2S RX -> mono PCM). Unwired until MC5 spawns the task from
// main.rs; silence dead-code until then.
#[allow(dead_code)]
pub mod mic_capture;
pub mod power;
pub mod power_stats;
pub mod rtc;
pub mod touch;
pub mod wifi;
