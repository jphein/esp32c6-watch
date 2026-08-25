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
// OTA). Spawned from main.rs; commands in via `net_task::send`, state out via
// `net_task::snapshot` + NET_WAKE.
pub mod net_task;
pub mod ota_http;
#[cfg(feature = "mesh-ota")]
pub mod ota_mesh;
// Per-device SIGIL IDENTITY from the efuse MAC (#34): name, node id,
// per-watch OTA topic. `mac` is a logs/debug field until a consumer lands.
#[allow(dead_code)]
pub mod sigil;
pub mod smol_mesh;
// Voice-to-text upload (STT bridge). Unwired until MC5 spawns it from main.rs;
// silence dead-code until then rather than churn per-item attributes.
#[allow(dead_code)]
pub mod voice_stt;
// Text-to-speech playback (same bridge, reverse direction): notification text
// -> Azure -> raw mono 16 kHz PCM streamed into audio_out. Gated on `tts`
// because the binary is out of ROM, NOT because it is incomplete — see the
// feature's comment in Cargo.toml. Some of the seam (explicit-address entry
// point, Spoken accessors) has no in-tree caller yet.
#[cfg(feature = "tts")]
#[allow(dead_code)]
pub mod voice_tts;
// Endless LitRPG story client (#story): the JSON half — chapter index, manifest
// segment index, character/stats and the playback cursor, all stream-parsed so
// an ~18 KB chapter payload is never resident. Gated on `story` so the shipped
// default build is byte-identical until the app has been on glass once; the
// audio half lives in `story_play`.
#[cfg(feature = "story")]
#[allow(dead_code)]
pub mod story_api;
// The audio half: Range-window `GET /media/{n}.pcm` streamed into audio_out.
// Must pump `service_amp` per chunk (read-aloud spec §6.2) and enforces a
// measured paint budget so live highlighting can never cost chopped audio.
#[cfg(feature = "story")]
#[allow(dead_code)]
pub mod story_play;
pub mod weather;
