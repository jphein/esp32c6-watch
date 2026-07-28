//! Host tests for the climate-model correctness core. Run: `cargo test -p climate-model`.
//!
//! Emphasis on the untrusted-input surface (malformed/truncated/oversized MQTT
//! payloads must return `None`/truncate, never panic) and the UI/wire ABI
//! (enum discriminants + `modes_mask`), which is luna's canonical interface.

use climate_model::*;

// ---------------------------------------------------------------------------
// Enum ABI — discriminants are the Slint segmented-control contract. If these
// break, the UI renders the wrong mode/color.
// ---------------------------------------------------------------------------

#[test]
fn hvac_mode_discriminants_are_the_ui_abi() {
    assert_eq!(HvacMode::Off as i32, 0);
    assert_eq!(HvacMode::Heat as i32, 1);
    assert_eq!(HvacMode::Cool as i32, 2);
    assert_eq!(HvacMode::Auto as i32, 3);
    assert_eq!(HvacMode::FanOnly as i32, 4);
    assert_eq!(HvacMode::Dry as i32, 5);
    // as_ui() is the integrator's accessor
    assert_eq!(HvacMode::FanOnly.as_ui(), 4);
    assert_eq!(HvacMode::Dry.as_ui(), 5);
}

#[test]
fn hvac_action_discriminants_are_the_ui_abi() {
    assert_eq!(HvacAction::Idle as i32, 0);
    assert_eq!(HvacAction::Heating as i32, 1);
    assert_eq!(HvacAction::Cooling as i32, 2);
}

#[test]
fn ha_mode_strings_map_to_correct_ints() {
    // The mapping luna's UI depends on — fan=4/dry=5 NOT swapped, heat_cool→3.
    assert_eq!(HvacMode::from_ha("off"), Some(HvacMode::Off));
    assert_eq!(HvacMode::from_ha("heat"), Some(HvacMode::Heat));
    assert_eq!(HvacMode::from_ha("cool"), Some(HvacMode::Cool));
    assert_eq!(HvacMode::from_ha("auto"), Some(HvacMode::Auto));
    assert_eq!(HvacMode::from_ha("fan_only"), Some(HvacMode::FanOnly));
    assert_eq!(HvacMode::from_ha("fan_only").map(HvacMode::as_ui), Some(4));
    assert_eq!(HvacMode::from_ha("dry"), Some(HvacMode::Dry));
    assert_eq!(HvacMode::from_ha("dry").map(HvacMode::as_ui), Some(5));
    // heat_cool folds into Auto (=3): the UI has no separate heat_cool
    assert_eq!(HvacMode::from_ha("heat_cool"), Some(HvacMode::Auto));
    assert_eq!(HvacMode::from_ha("heat_cool").map(HvacMode::as_ui), Some(3));
    // unknown → None (skipped, not mis-rendered)
    assert_eq!(HvacMode::from_ha("eco"), None);
    assert_eq!(HvacMode::from_ha(""), None);
}

#[test]
fn ha_action_strings_map_to_correct_ints() {
    assert_eq!(HvacAction::from_ha("off"), Some(HvacAction::Idle));
    assert_eq!(HvacAction::from_ha("idle"), Some(HvacAction::Idle));
    assert_eq!(HvacAction::from_ha("fan"), Some(HvacAction::Idle));
    assert_eq!(HvacAction::from_ha("heating"), Some(HvacAction::Heating));
    assert_eq!(HvacAction::from_ha("preheating"), Some(HvacAction::Heating));
    assert_eq!(HvacAction::from_ha("cooling"), Some(HvacAction::Cooling));
    // drying/defrosting run the compressor → cool/blue accent
    assert_eq!(HvacAction::from_ha("drying"), Some(HvacAction::Cooling));
    assert_eq!(HvacAction::from_ha("defrosting"), Some(HvacAction::Cooling));
    assert_eq!(HvacAction::from_ha("bogus"), None);
}

// ---------------------------------------------------------------------------
// Real payloads
// ---------------------------------------------------------------------------

#[test]
fn parses_real_nest_thermostat_state() {
    let json = br#"{"name":"Living Room","cur":71.5,"set":72,"mode":"heat","action":"heating","min":50,"max":90,"step":1.0,"modes":["off","heat","cool","auto"]}"#;
    let e = parse_state(json).expect("valid thermostat state");
    assert_eq!(e.name, "Living Room");
    assert_eq!(e.cur, Some(71.5));
    assert_eq!(e.set, Some(72.0));
    assert_eq!(e.mode, HvacMode::Heat);
    assert_eq!(e.action, HvacAction::Heating);
    assert_eq!(e.min, 50.0);
    assert_eq!(e.max, 90.0);
    assert_eq!(e.step, 1.0);
    assert_eq!(e.modes.len(), 4);
    // off|heat|cool|auto = bits 0,1,2,3 = 0b001111 = 15
    assert_eq!(e.modes_mask(), 0b0000_1111);
    assert_eq!(e.modes_mask(), 15);
}

#[test]
fn parses_minisplit_with_dry_and_fan_modes() {
    // Full six-mode minisplit; heat_cool is present AND auto is present → deduped
    // to a single Auto. Distinct modes = off,cool,heat,dry,fan_only,auto = all six.
    let json = br#"{"name":"Office Minisplit","cur":74.0,"set":70.0,"mode":"cool","action":"cooling","min":60,"max":86,"step":0.5,"modes":["off","cool","heat","dry","fan_only","heat_cool","auto"]}"#;
    let e = parse_state(json).expect("valid minisplit state");
    assert_eq!(e.name, "Office Minisplit");
    assert_eq!(e.cur, Some(74.0));
    assert_eq!(e.set, Some(70.0));
    assert_eq!(e.mode, HvacMode::Cool);
    assert_eq!(e.action, HvacAction::Cooling);
    assert_eq!(e.step, 0.5);
    // dedup: 7 array entries → 6 distinct modes (heat_cool folded onto auto)
    assert_eq!(e.modes.len(), 6);
    assert!(e.modes.contains(&HvacMode::Dry));
    assert!(e.modes.contains(&HvacMode::FanOnly));
    assert!(e.modes.contains(&HvacMode::Auto));
    // all six modes → 0b111111 = 63
    assert_eq!(e.modes_mask(), 0b0011_1111);
    assert_eq!(e.modes_mask(), 63);
}

#[test]
fn drying_action_reads_as_cooling_accent() {
    let json = br#"{"name":"Dehumidify","cur":75.0,"set":72.0,"mode":"dry","action":"drying","min":60,"max":86,"step":0.5,"modes":["off","dry","cool"]}"#;
    let e = parse_state(json).expect("valid dry state");
    assert_eq!(e.mode, HvacMode::Dry);
    assert_eq!(e.action, HvacAction::Cooling); // drying → blue
}

#[test]
fn missing_optional_cur_is_none_but_rest_parses() {
    let json = br#"{"name":"Bedroom","set":68,"mode":"off","action":"off","min":50,"max":90,"step":1.0,"modes":["off","heat"]}"#;
    let e = parse_state(json).expect("state without cur still valid");
    assert_eq!(e.cur, None);
    assert_eq!(e.set, Some(68.0));
    assert_eq!(e.mode, HvacMode::Off);
    assert_eq!(e.action, HvacAction::Idle); // "off" action → Idle
    assert_eq!(e.name, "Bedroom");
}

#[test]
fn null_cur_is_none() {
    let json = br#"{"name":"Attic","cur":null,"set":65,"mode":"heat","action":"idle","min":50,"max":90,"step":1.0,"modes":["off","heat"]}"#;
    let e = parse_state(json).expect("null cur tolerated");
    assert_eq!(e.cur, None);
    assert_eq!(e.action, HvacAction::Idle);
}

#[test]
fn unknown_fields_and_modes_are_tolerated() {
    // Forward-compat: bridge adds fields/modes the watch doesn't know → ignored/skipped.
    let json = br#"{"name":"Future","cur":70,"set":70,"mode":"heat","action":"heating","min":50,"max":90,"step":1,"modes":["off","heat","eco","boost"],"fan_mode":"auto","preset":"home"}"#;
    let e = parse_state(json).expect("unknown fields ignored");
    assert_eq!(e.mode, HvacMode::Heat);
    // "eco"/"boost" skipped → only off,heat kept
    assert_eq!(e.modes.len(), 2);
    assert_eq!(e.modes_mask(), 0b11); // off|heat
}

#[test]
fn unknown_top_level_mode_defaults_to_off() {
    let json = br#"{"name":"X","cur":70,"set":70,"mode":"turbo","action":"heating","min":50,"max":90,"step":1,"modes":["off","heat"]}"#;
    let e = parse_state(json).expect("unknown mode tolerated");
    assert_eq!(e.mode, HvacMode::Off); // unknown mode → safe default
}

#[test]
fn field_order_and_whitespace_independent() {
    let json = br#"  {  "modes" : ["off","cool"] , "action":"cooling" ,"set": 68.5, "mode":"cool","name":"Den","min":60,"max":85,"step":0.5,"cur":70.0 }  "#;
    let e = parse_state(json).expect("reordered + spaced JSON");
    assert_eq!(e.name, "Den");
    assert_eq!(e.set, Some(68.5));
    assert_eq!(e.mode, HvacMode::Cool);
    assert_eq!(e.action, HvacAction::Cooling);
    assert_eq!(e.modes.len(), 2);
}

// ---------------------------------------------------------------------------
// Untrusted input: malformed / truncated must return None, never panic.
// ---------------------------------------------------------------------------

#[test]
fn malformed_and_truncated_return_none_never_panic() {
    let cases: &[&[u8]] = &[
        b"",                                   // empty
        b"   ",                                // whitespace only
        b"not json",                           // not an object
        b"[1,2,3]",                            // array, not object
        b"{",                                  // bare open brace
        b"{\"name\"",                          // key, no colon/value
        b"{\"name\":",                         // colon, no value
        b"{\"name\":}",                        // value is a delimiter
        b"{\"name\":\"Liv",                    // unterminated string value
        b"{\"name\":\"Living Room\"",          // unterminated object
        b"{\"name\":\"x\",",                   // trailing comma, no close
        b"{\"modes\":[\"off\"",               // unterminated array
        b"{\"modes\":[\"off",                 // unterminated array element
        b"{\"cur\": }",                        // missing number
        b"\x00\x01\x02\xff\xfe",              // raw garbage bytes
        b"{\"name\":\"\xff\xfe\xff\"}",       // invalid UTF-8 inside a string
        b"null",                               // JSON null
        b"12345",                              // bare number
    ];
    for c in cases {
        // The contract: no panic. Most of these are structurally broken → None.
        let _ = parse_state(c);
    }
    // Spot-check the ones that must be None (broken structure):
    assert_eq!(parse_state(b""), None);
    assert_eq!(parse_state(b"not json"), None);
    assert_eq!(parse_state(b"{"), None);
    assert_eq!(parse_state(b"{\"name\":\"Liv"), None);
    assert_eq!(parse_state(b"{\"name\":\"Living Room\""), None);
    assert_eq!(parse_state(b"{\"modes\":[\"off\""), None);
}

#[test]
fn oversized_total_payload_is_rejected() {
    // > MAX_INPUT bytes → None (bounded scan), not a panic.
    let mut big = Vec::new();
    big.extend_from_slice(b"{\"name\":\"");
    big.resize(big.len() + 5000, b'A');
    big.extend_from_slice(b"\"}");
    assert_eq!(parse_state(&big), None);
}

#[test]
fn oversized_ascii_name_truncates_to_32_bytes() {
    let mut json = Vec::new();
    json.extend_from_slice(b"{\"name\":\"");
    json.extend(core::iter::repeat(b'A').take(200));
    json.extend_from_slice(b"\",\"set\":70,\"mode\":\"heat\",\"action\":\"idle\",\"min\":50,\"max\":90,\"step\":1,\"modes\":[\"heat\"]}");
    let e = parse_state(&json).expect("oversized name still parses");
    assert_eq!(e.name.len(), 32); // exactly the byte cap, no overflow
    assert!(e.name.chars().all(|c| c == 'A'));
    assert_eq!(e.set, Some(70.0));
}

#[test]
fn oversized_multibyte_name_truncates_on_char_boundary() {
    // '€' is 3 bytes. 40 of them = 120 bytes → must clip to a multiple of 3 ≤ 32.
    let euros: std::string::String = core::iter::repeat('€').take(40).collect();
    let json = std::format!("{{\"name\":\"{}\",\"set\":70,\"mode\":\"heat\",\"action\":\"idle\",\"min\":50,\"max\":90,\"step\":1,\"modes\":[\"heat\"]}}", euros);
    let e = parse_state(json.as_bytes()).expect("multibyte name parses");
    assert!(e.name.len() <= 32, "len {} exceeds cap", e.name.len());
    assert_eq!(e.name.len() % 3, 0, "clipped mid-codepoint: {}", e.name.len());
    assert!(e.name.chars().all(|c| c == '€'));
    // core::str::from_utf8 on the stored bytes must succeed (valid UTF-8)
    assert!(core::str::from_utf8(e.name.as_bytes()).is_ok());
}

#[test]
fn backslash_before_multibyte_char_does_not_panic() {
    // Regression (oracle-t9-spec): `\` followed by a multibyte codepoint used to
    // step the byte cursor mid-'é' and panic on the next slice. Must parse cleanly.
    // b"{\"name\":\"\\\xc3\xa9\"}"  ==  {"name":"\é"}
    let input = b"{\"name\":\"\\\xc3\xa9\"}";
    let e = parse_state(input).expect("backslash+multibyte must not panic");
    assert_eq!(e.name, "\u{e9}"); // unknown escape → the char emitted literally

    // A few more adversarial escape/boundary shapes — all must return Some, no panic.
    assert!(parse_state(b"{\"name\":\"\\\xe2\x82\xac\"}").is_some()); // \ + '€' (3-byte)
    assert!(parse_state("{\"name\":\"\\🔥\"}".as_bytes()).is_some()); // \ + 4-byte emoji
    // `\"` escapes the closing quote → string is unterminated → None (not a panic).
    assert!(parse_state(b"{\"name\":\"abc\\\"}").is_none());
    assert!(parse_state(b"{\"name\":\"\\u00e9\"}").is_some()); // valid \u escape → 'é'
    assert!(parse_state(b"{\"name\":\"\\u12\"}").is_some()); // truncated \u escape
    assert_eq!(
        parse_state(b"{\"name\":\"\\u00e9\"}").unwrap().name,
        "\u{e9}"
    );
}

#[test]
fn json_escapes_in_name_are_decoded() {
    let json = br#"{"name":"A\"B\\C\/D","set":70,"mode":"heat","action":"idle","min":50,"max":90,"step":1,"modes":["heat"]}"#;
    let e = parse_state(json).expect("escaped name parses");
    assert_eq!(e.name, "A\"B\\C/D");
}

// ---------------------------------------------------------------------------
// clamp_step — bounds and mid-range
// ---------------------------------------------------------------------------

#[test]
fn clamp_step_mid_range_increments_by_step() {
    assert_eq!(clamp_step(70.0, 1.0, 50.0, 90.0, 1.0), 71.0);
    assert_eq!(clamp_step(70.0, -1.0, 50.0, 90.0, 1.0), 69.0);
    assert_eq!(clamp_step(70.0, 0.5, 50.0, 90.0, 0.5), 70.5);
}

#[test]
fn clamp_step_holds_at_min_bound() {
    assert_eq!(clamp_step(50.0, -5.0, 50.0, 90.0, 1.0), 50.0);
    assert_eq!(clamp_step(51.0, -10.0, 50.0, 90.0, 1.0), 50.0);
}

#[test]
fn clamp_step_holds_at_max_bound() {
    assert_eq!(clamp_step(90.0, 5.0, 50.0, 90.0, 1.0), 90.0);
    assert_eq!(clamp_step(89.0, 10.0, 50.0, 90.0, 1.0), 90.0);
}

#[test]
fn clamp_step_snaps_offset_value_to_step_grid() {
    // 70.3 with a 0.5 grid rooted at min=50 → nearest is 70.5
    assert_eq!(clamp_step(70.3, 0.0, 50.0, 90.0, 0.5), 70.5);
    // half-degree grid, +0.5 step
    assert_eq!(clamp_step(70.0, 0.5, 50.0, 90.0, 0.5), 70.5);
}

#[test]
fn clamp_step_tolerates_garbage_params() {
    // step <= 0 → no snapping, just clamp. Non-finite → no panic.
    assert_eq!(clamp_step(70.0, 1.0, 50.0, 90.0, 0.0), 71.0);
    assert_eq!(clamp_step(70.0, 1.0, 50.0, 90.0, -1.0), 71.0);
    let _ = clamp_step(f32::NAN, 1.0, 50.0, 90.0, 1.0);
    let _ = clamp_step(70.0, f32::INFINITY, 50.0, 90.0, 1.0);
    let _ = clamp_step(70.0, 1.0, f32::NAN, f32::NAN, f32::NAN);
}

// ---------------------------------------------------------------------------
// Command encode round-trip
// ---------------------------------------------------------------------------

#[test]
fn encode_set_temp_shape() {
    assert_eq!(encode_set_temp(72.0).as_str(), "{\"set\":72.0}");
    assert_eq!(encode_set_temp(68.5).as_str(), "{\"set\":68.5}");
    // non-finite coerced to valid JSON
    assert_eq!(encode_set_temp(f32::NAN).as_str(), "{\"set\":0.0}");
}

#[test]
fn encode_set_mode_shape() {
    assert_eq!(encode_set_mode(HvacMode::Heat).as_str(), "{\"mode\":\"heat\"}");
    assert_eq!(encode_set_mode(HvacMode::Cool).as_str(), "{\"mode\":\"cool\"}");
    assert_eq!(encode_set_mode(HvacMode::Off).as_str(), "{\"mode\":\"off\"}");
    assert_eq!(encode_set_mode(HvacMode::Auto).as_str(), "{\"mode\":\"auto\"}");
    assert_eq!(
        encode_set_mode(HvacMode::FanOnly).as_str(),
        "{\"mode\":\"fan_only\"}"
    );
    assert_eq!(encode_set_mode(HvacMode::Dry).as_str(), "{\"mode\":\"dry\"}");
}

#[test]
fn encode_set_temp_round_trips_through_parse_num() {
    // The number we emit must parse back to the same value (bridge/HA read-side).
    for &t in &[72.0f32, 68.5, 50.0, 90.0, 65.5] {
        let payload = encode_set_temp(t);
        // reuse parse_state by wrapping the command number in a minimal state
        let wrapped = std::format!("{{\"set\":{}}}", &payload.as_str()[7..payload.len() - 1]);
        let e = parse_state(wrapped.as_bytes()).expect("wrapped set parses");
        assert_eq!(e.set, Some(t));
    }
}

// ---------------------------------------------------------------------------
// ClimateState upsert
// ---------------------------------------------------------------------------

fn entity_named(name: &str, set: f32) -> ClimateEntity {
    let json = std::format!(
        "{{\"name\":\"{}\",\"cur\":70,\"set\":{},\"mode\":\"heat\",\"action\":\"idle\",\"min\":50,\"max\":90,\"step\":1,\"modes\":[\"off\",\"heat\"]}}",
        name, set
    );
    parse_state(json.as_bytes()).expect("test entity parses")
}

#[test]
fn upsert_inserts_then_replaces_by_object_id() {
    let mut st = ClimateState::new();
    st.upsert("living_room", entity_named("Living Room", 70.0));
    st.upsert("bedroom", entity_named("Bedroom", 68.0));
    assert_eq!(st.len(), 2);

    // same id → replace in place, len unchanged
    st.upsert("living_room", entity_named("Living Room", 72.0));
    assert_eq!(st.len(), 2);
    assert_eq!(st.get("living_room").unwrap().set, Some(72.0));
    assert_eq!(st.get("bedroom").unwrap().set, Some(68.0));
    assert!(st.get("garage").is_none());
}

#[test]
fn upsert_is_bounded_and_never_panics_when_full() {
    let mut st = ClimateState::new();
    for i in 0..50 {
        let id = std::format!("dev_{}", i);
        st.upsert(&id, entity_named("D", 70.0));
    }
    assert!(st.len() <= 12, "capacity exceeded: {}", st.len());
}

#[test]
fn upsert_handles_long_object_ids() {
    // Real HA object_ids exceed 24 chars — the key is String<48>.
    let mut st = ClimateState::new();
    let long_id = "living_room_minisplit_thermostat"; // 32 chars
    st.upsert(long_id, entity_named("LR Minisplit", 71.0));
    assert_eq!(st.get(long_id).unwrap().set, Some(71.0));
    // repeat under the same long id → replace, not duplicate
    st.upsert(long_id, entity_named("LR Minisplit", 73.0));
    assert_eq!(st.len(), 1);
    assert_eq!(st.get(long_id).unwrap().set, Some(73.0));

    // an id longer than the 48-byte cap is clipped, never overflows/panics
    let huge_id: std::string::String = core::iter::repeat('z').take(200).collect();
    st.upsert(&huge_id, entity_named("Z", 70.0));
    assert!(st.len() <= 12);
}

#[test]
fn state_modes_mask_delegates_to_entity() {
    let mut st = ClimateState::new();
    st.upsert("lr", entity_named("LR", 70.0)); // modes off,heat
    assert_eq!(st.modes_mask("lr"), 0b11);
    assert_eq!(st.modes_mask("missing"), 0);
}

// ---------------------------------------------------------------------------
// Golden-vector gaps (wisp validation vs the real climate-bridge.flow.json):
// the bridge emits `set: target_temperature ?? null`, and heat pumps expose
// `heat_cool` (not `auto`). Both must parse correctly.
// ---------------------------------------------------------------------------

#[test]
fn null_set_is_none_like_null_cur() {
    // A device with no target temp: bridge publishes set:null → must be None
    // (not 0), with the rest intact. "fan" action → Idle.
    let json = br#"{"name":"Fan","cur":72.0,"set":null,"mode":"fan_only","action":"fan","min":60,"max":86,"step":1.0,"modes":["off","fan_only"]}"#;
    let e = parse_state(json).expect("null set still valid");
    assert_eq!(e.set, None);
    assert_eq!(e.cur, Some(72.0));
    assert_eq!(e.mode, HvacMode::FanOnly);
    assert_eq!(e.action, HvacAction::Idle);
}

#[test]
fn heat_cool_only_device_sets_auto_bit() {
    // Heat pump exposing "heat_cool" (not "auto") must fold to Auto and set the
    // Auto bit so the UI shows the segment (ingest side of the auto/heat_cool
    // asymmetry; the command side is the bridge's capability-aware remap).
    let json = br#"{"name":"HP","cur":70.0,"set":71.0,"mode":"heat_cool","action":"idle","min":50,"max":90,"step":1.0,"modes":["off","heat","cool","heat_cool"]}"#;
    let e = parse_state(json).expect("heat_cool-only valid");
    assert_eq!(e.mode, HvacMode::Auto);
    assert!(e.modes.contains(&HvacMode::Auto));
    assert_eq!(e.modes_mask(), 0b0000_1111); // off|heat|cool|auto = 15
}

// ---------------------------------------------------------------------------
// render_fingerprint — the change-gate that stops the per-tick Slint model
// rebuild that OOM-froze the Climate screen. STABLE when nothing rendered
// changed; DIFFERENT on any rendered field. If a rendered field mutates without
// moving the fingerprint, the UI would silently stop updating — so these lock
// each field the card reads.
// ---------------------------------------------------------------------------

#[test]
fn fingerprint_is_stable_for_identical_state() {
    let mut a = ClimateState::new();
    a.upsert("lr", entity_named("Living Room", 70.0));
    a.upsert("br", entity_named("Bedroom", 68.0));
    let mut b = ClimateState::new();
    b.upsert("lr", entity_named("Living Room", 70.0));
    b.upsert("br", entity_named("Bedroom", 68.0));
    assert_eq!(a.render_fingerprint(), b.render_fingerprint());
    // Idempotent across repeated calls (no interior mutability).
    assert_eq!(a.render_fingerprint(), a.render_fingerprint());
}

#[test]
fn fingerprint_changes_on_setpoint() {
    let mut a = ClimateState::new();
    a.upsert("lr", entity_named("Living Room", 70.0));
    let f0 = a.render_fingerprint();
    a.upsert("lr", entity_named("Living Room", 72.0)); // ±tap moves set
    assert_ne!(f0, a.render_fingerprint());
}

#[test]
fn fingerprint_changes_on_name_and_count() {
    let mut a = ClimateState::new();
    a.upsert("lr", entity_named("Living Room", 70.0));
    let f_one = a.render_fingerprint();

    let mut b = ClimateState::new();
    b.upsert("lr", entity_named("Lounge", 70.0)); // different name, same set
    assert_ne!(f_one, b.render_fingerprint());

    a.upsert("br", entity_named("Bedroom", 70.0)); // count 1 → 2
    assert_ne!(f_one, a.render_fingerprint());
}

#[test]
fn fingerprint_distinguishes_none_from_zero_cur() {
    // A device reporting cur:null must not collide with one reporting 0.0 —
    // the card renders "--" vs "0", a real visible difference.
    let null_cur = parse_state(br#"{"name":"X","cur":null,"set":70,"mode":"heat","action":"idle","min":50,"max":90,"step":1,"modes":["heat"]}"#).unwrap();
    let zero_cur = parse_state(br#"{"name":"X","cur":0,"set":70,"mode":"heat","action":"idle","min":50,"max":90,"step":1,"modes":["heat"]}"#).unwrap();
    let mut a = ClimateState::new();
    a.upsert("x", null_cur);
    let mut b = ClimateState::new();
    b.upsert("x", zero_cur);
    assert_ne!(a.render_fingerprint(), b.render_fingerprint());
}

#[test]
fn fingerprint_empty_differs_from_populated() {
    let empty = ClimateState::new();
    let mut one = ClimateState::new();
    one.upsert("lr", entity_named("LR", 70.0));
    assert_ne!(empty.render_fingerprint(), one.render_fingerprint());
}
