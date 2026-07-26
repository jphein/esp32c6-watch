// The Familiar creature UI accessors (known/is_holder/mood/creature/stage_level
// + growth-stage consts) lost their only caller when task 9 dropped the eg
// watchface fam snapshot; task 12 re-wires them onto the Slint watchface. The
// mesh arbitration/beat half of the module stays live. Silence dead-code until
// then rather than churn per-item attributes that task 12 would revert.
#[allow(dead_code)]
pub mod familiar;
// Temporary stand-in for the `crates/climate-model` crate (built in parallel on
// feat/climate-model). Delete on integration — see the module docs. Silence
// dead-code until the bidirectional climate session is wired from main.rs.
#[allow(dead_code)]
pub mod mqtt_ha;
// Bidirectional MQTT climate session. Unwired until main.rs spawns it for the
// Climate screen (integrator's serial step); silence dead-code until then, same
// as voice_stt, rather than churn per-item attributes.
#[allow(dead_code)]
pub mod mqtt_climate;
pub mod names;
// #53 net_task: the network owner (WiFi connect machine, scan, boot burst,
// OTA). Dead-code-silenced until the stage-3 main.rs migration spawns it —
// same convention as mqtt_climate/voice_stt during their integration windows.
#[allow(dead_code)]
pub mod net_task;
pub mod ota_http;
// Per-device SIGIL IDENTITY from the efuse MAC (#34): name, node id,
// per-watch OTA topic. `mac` is a logs/debug field until a consumer lands.
#[allow(dead_code)]
pub mod sigil;
pub mod smol_mesh;
// Voice-to-text upload (STT bridge). Unwired until MC5 spawns it from main.rs;
// silence dead-code until then rather than churn per-item attributes.
#[allow(dead_code)]
pub mod voice_stt;
pub mod weather;
