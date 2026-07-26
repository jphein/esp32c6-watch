pub mod audio;
// #23 shared I2S TX playback seam: play_pcm/busy + the feeder that substitutes
// SFX samples into silent_clock_task's always-running circular ring.
pub mod audio_out;
pub mod ble;
// ES7210 4-ch ADC — the actual microphone codec on this board (mics wired to it,
// SDOUT1 -> GPIO21; ES8311 is speaker-DAC only). Must be I2C-inited for capture.
pub mod es7210;
pub mod config;
pub mod cpu_clock;
// Die-temp helper: pre-staged, wired into main.rs's system-page push once
// light-sleep (#29) frees up main.rs. Unused until then → dead-code warning.
#[allow(dead_code)]
pub mod die_temp;
pub mod imu;
// MC2 mic capture (I2S RX -> mono PCM). Unwired until MC5 spawns the task from
// main.rs; silence dead-code until then.
#[allow(dead_code)]
pub mod mic_capture;
pub mod power;
pub mod power_stats;
pub mod rtc;
pub mod touch;
// (wifi.rs retired in v0.9.0: its WifiConfig/WifiState only served the fb
// Settings app; the hub's NETWORK flow lives in main.rs + slint_shell.)
