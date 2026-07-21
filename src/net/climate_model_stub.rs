//! TEMPORARY stand-in for the `crates/climate-model` crate.
//!
//! `climate-model` is being built in parallel on branch `feat/climate-model`
//! (host-testable pure-logic crate, per the host-testable-crates pattern). It
//! is not present in this worktree yet, so this module mirrors the subset of
//! its **spec §B′** API that [`crate::net::mqtt_climate`] depends on, letting the
//! bidirectional MQTT session compile + link green on the target now.
//!
//! ## Integration (delete-on-merge)
//! When `climate-model` lands, the integrator:
//!   1. adds `climate-model = { path = "crates/climate-model" }` to `Cargo.toml`,
//!   2. changes the one alias in `mqtt_climate.rs`
//!      (`use crate::net::climate_model_stub as climate_model;`)
//!      to `use climate_model;`,
//!   3. deletes this file + its `pub mod climate_model_stub;` line in `mod.rs`.
//!
//! The API surface `mqtt_climate` actually calls is intentionally tiny:
//! `parse_state`, `ClimateState::upsert`, `encode_set_temp`, `encode_set_mode`,
//! `HvacMode`. Enum discriminants match the canonical UI contract
//! (luna's `feat/climate-ui`): mode `Off=0 Heat=1 Cool=2 Auto=3 FanOnly=4
//! Dry=5`, action `Idle=0 Heating=1 Cooling=2`. If the real crate drifts from
//! these signatures, coordinate via team-lead — see mqtt_climate's module docs
//! for the exact dependency list.

#![allow(dead_code)]

use core::fmt::Write as _;
use heapless::{String, Vec};

pub const NAME_CAP: usize = 32;
pub const OBJ_ID_CAP: usize = 48;
pub const CMD_CAP: usize = 24;
pub const MAX_MODES: usize = 7;
pub const MAX_ENTITIES: usize = 12;

/// HA HVAC mode. Discriminants are the canonical UI ints (heat_cool → Auto).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum HvacMode {
    Off = 0,
    Heat = 1,
    Cool = 2,
    Auto = 3,
    FanOnly = 4,
    Dry = 5,
}

impl HvacMode {
    /// Parse the HA `hvac_mode` string. Unknown → `Off` (never panics).
    pub fn from_ha(s: &str) -> Self {
        match s {
            "heat" => Self::Heat,
            "cool" => Self::Cool,
            "auto" | "heat_cool" => Self::Auto,
            "fan_only" => Self::FanOnly,
            "dry" => Self::Dry,
            _ => Self::Off,
        }
    }
    /// HA service-call string form.
    pub fn as_ha(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Heat => "heat",
            Self::Cool => "cool",
            Self::Auto => "heat_cool",
            Self::FanOnly => "fan_only",
            Self::Dry => "dry",
        }
    }
}

/// HA HVAC action (what the unit is doing right now).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum HvacAction {
    Idle = 0,
    Heating = 1,
    Cooling = 2,
}

impl HvacAction {
    pub fn from_ha(s: &str) -> Self {
        match s {
            "heating" => Self::Heating,
            "cooling" => Self::Cooling,
            _ => Self::Idle,
        }
    }
}

/// One climate entity's current state (spec §B′).
#[derive(Clone, Debug)]
pub struct ClimateEntity {
    pub name: String<NAME_CAP>,
    pub cur: Option<f32>,
    pub set: Option<f32>,
    pub mode: HvacMode,
    pub action: HvacAction,
    pub min: f32,
    pub max: f32,
    pub step: f32,
    pub modes: Vec<HvacMode, MAX_MODES>,
}

impl ClimateEntity {
    /// Bitmask of supported modes (bit m = mode discriminant m). Canonical UI
    /// contract expects a `u16`.
    pub fn modes_mask(&self) -> u16 {
        let mut mask = 0u16;
        for m in &self.modes {
            mask |= 1 << (*m as u16);
        }
        mask
    }
}

/// Full climate roster, upsert-by-object-id (spec §B′).
#[derive(Default)]
pub struct ClimateState {
    pub entities: Vec<(String<OBJ_ID_CAP>, ClimateEntity), MAX_ENTITIES>,
}

impl ClimateState {
    pub const fn new() -> Self {
        Self { entities: Vec::new() }
    }

    /// Insert or replace the entity for `obj`. Full roster → drop silently
    /// (bounded, never panics).
    pub fn upsert(&mut self, obj: &str, entity: ClimateEntity) {
        for e in self.entities.iter_mut() {
            if e.0.as_str() == obj {
                e.1 = entity;
                return;
            }
        }
        let mut id: String<OBJ_ID_CAP> = String::new();
        if id.push_str(obj).is_ok() {
            let _ = self.entities.push((id, entity));
        }
    }
}

/// Parse a `watch/climate/<id>/state` JSON payload. Bounded, never panics;
/// returns `None` on empty / non-UTF-8 / unparseable input (caller skips the
/// entity). This stub is a best-effort field scanner — the real crate is the
/// host-tested source of truth.
pub fn parse_state(bytes: &[u8]) -> Option<ClimateEntity> {
    if bytes.is_empty() {
        return None;
    }
    let s = core::str::from_utf8(bytes).ok()?;

    let mut name: String<NAME_CAP> = String::new();
    if let Some(n) = str_field(s, "name") {
        let _ = name.push_str(&n[..n.len().min(NAME_CAP)]);
    }
    let mode = str_field(s, "mode")
        .map(|m| HvacMode::from_ha(&m))
        .unwrap_or(HvacMode::Off);
    let action = str_field(s, "action")
        .map(|a| HvacAction::from_ha(&a))
        .unwrap_or(HvacAction::Idle);

    let mut modes: Vec<HvacMode, MAX_MODES> = Vec::new();
    let _ = modes.push(HvacMode::Off);
    let _ = modes.push(mode);

    Some(ClimateEntity {
        name,
        cur: num_field(s, "cur"),
        set: num_field(s, "set"),
        mode,
        action,
        min: num_field(s, "min").unwrap_or(50.0),
        max: num_field(s, "max").unwrap_or(90.0),
        step: num_field(s, "step").unwrap_or(1.0),
        modes,
    })
}

/// Command payload for `climate.set_temperature`.
pub fn encode_set_temp(temp: f32) -> String<CMD_CAP> {
    let mut s: String<CMD_CAP> = String::new();
    let _ = write!(s, "{{\"set\":{}}}", temp);
    s
}

/// Command payload for `climate.set_hvac_mode`.
pub fn encode_set_mode(mode: HvacMode) -> String<CMD_CAP> {
    let mut s: String<CMD_CAP> = String::new();
    let _ = write!(s, "{{\"mode\":\"{}\"}}", mode.as_ha());
    s
}

/// Step a setpoint by `delta` steps, clamped to `[min, max]`.
pub fn clamp_step(cur_set: f32, delta: f32, min: f32, max: f32, step: f32) -> f32 {
    (cur_set + delta * step).clamp(min, max)
}

// --- tiny bounded JSON field scanners (stub-local) --------------------------

/// Extract a string value for `"<key>":"..."`. No escape handling (stub).
fn str_field(s: &str, key: &str) -> Option<String<NAME_CAP>> {
    let after = value_after_key(s, key)?;
    let rest = after.strip_prefix('"')?;
    let end = rest.find('"')?;
    let mut out: String<NAME_CAP> = String::new();
    let _ = out.push_str(&rest[..end.min(NAME_CAP)]);
    Some(out)
}

/// Extract a numeric value for `"<key>":<number>`.
fn num_field(s: &str, key: &str) -> Option<f32> {
    let after = value_after_key(s, key)?;
    let end = after
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+'))
        .unwrap_or(after.len());
    after[..end].parse::<f32>().ok()
}

/// Slice starting at the first non-space char after `"<key>":`.
fn value_after_key<'a>(s: &'a str, key: &str) -> Option<&'a str> {
    let mut needle: String<40> = String::new();
    needle.push('"').ok()?;
    needle.push_str(key).ok()?;
    needle.push('"').ok()?;
    let idx = s.find(needle.as_str())?;
    let after_key = &s[idx + needle.len()..];
    let colon = after_key.find(':')?;
    Some(after_key[colon + 1..].trim_start())
}
