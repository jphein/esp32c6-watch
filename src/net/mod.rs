// The Familiar creature UI accessors (known/is_holder/mood/creature/stage_level
// + growth-stage consts) lost their only caller when task 9 dropped the eg
// watchface fam snapshot; task 12 re-wires them onto the Slint watchface. The
// mesh arbitration/beat half of the module stays live. Silence dead-code until
// then rather than churn per-item attributes that task 12 would revert.
#[allow(dead_code)]
pub mod familiar;
pub mod mqtt_ha;
pub mod names;
pub mod ota_http;
pub mod smol_mesh;
pub mod weather;
