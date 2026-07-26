"""Plain-HTTP endpoints the ESP32-C6 watch talks to.

These are bare ``aiohttp`` handlers (not ``HomeAssistantView``\\ s) served by the
component's own ``AppRunner``/``TCPSite`` on a dedicated port — HA's :8123 is
HTTPS-only and the watch has no TLS. Everything is a read of ``hass.states`` or
a call into the ``climate.*`` services; no broker, no Node-RED.

The JSON shapes are exactly what ``crates/climate-model`` on the watch parses
(``parse_state`` / ``parse_energy``), plus a transparent ``"id"`` key on each
climate element. The crate ignores unknown keys, so the extra field is free.

The same app also serves the speaker/announce endpoints the watch pulls TTS
audio from (``/watch/announce`` + ``/watch/announce/pending``); the PCM is
produced and queued by ``media_player.py`` / ``announce.py``.

Every handler is wrapped so a bad request can never take the listener down.
"""

from __future__ import annotations

import functools
import hmac
import logging
from typing import Any, Callable

from aiohttp import web

from homeassistant.config_entries import ConfigEntry
from homeassistant.core import HomeAssistant

from .announce import AnnounceQueue
from .const import (
    CONF_BATTERY_PCT_ENTITY,
    CONF_CHARGING_ENTITY,
    CONF_CLIMATE_EXCLUDE,
    CONF_GRID_W_ENTITY,
    CONF_SOLAR_W_ENTITY,
    CONF_TOKEN,
    DEFAULT_BATTERY_PCT_ENTITY,
    DEFAULT_CLIMATE_EXCLUDE,
    DEFAULT_GRID_W_ENTITY,
    DEFAULT_SOLAR_W_ENTITY,
    DOMAIN,
    TOKEN_HEADER,
    VERSION,
)

_LOGGER = logging.getLogger(__name__)

# State strings that mean "no value" when reading a source sensor.
_NO_VALUE = frozenset({"unknown", "unavailable", "none", ""})
# States that read as "charging" for the optional charging entity.
_TRUTHY = frozenset({"on", "true", "charging", "1", "yes"})

# Keys stashed on the aiohttp app so the handlers can reach HA + the entry.
APP_HASS = "esp32c6_watch_hass"
APP_ENTRY = "esp32c6_watch_entry"
APP_QUEUE = "esp32c6_watch_queue"


# ---------------------------------------------------------------------------
# Small helpers
# ---------------------------------------------------------------------------
def _hass(request: web.Request) -> HomeAssistant:
    return request.app[APP_HASS]


def _conf(request: web.Request) -> dict[str, Any]:
    """Merged config: options win over the initial entry data."""
    entry: ConfigEntry = request.app[APP_ENTRY]
    return {**entry.data, **entry.options}


def _queue(request: web.Request) -> AnnounceQueue:
    return request.app[APP_QUEUE]


def _num(value: Any) -> float | None:
    """Coerce to float, or ``None`` for anything non-numeric."""
    if value is None:
        return None
    try:
        return float(value)
    except (ValueError, TypeError):
        return None


def _round_to_step(value: float, step: float) -> float:
    """Snap ``value`` to the nearest ``step`` (dual-setpoint midpoint)."""
    if not step:
        return round(value, 1)
    return round(round(value / step) * step, 3)


def _exclude_set(conf: dict[str, Any]) -> set[str]:
    """Excluded object ids, ``climate.`` prefix tolerated and stripped."""
    raw = conf.get(CONF_CLIMATE_EXCLUDE, DEFAULT_CLIMATE_EXCLUDE)
    parts = raw.split(",") if isinstance(raw, str) else list(raw)
    out: set[str] = set()
    for part in parts:
        obj = str(part).strip()
        if not obj:
            continue
        if obj.startswith("climate."):
            obj = obj[len("climate.") :]
        out.add(obj)
    return out


def _exposed_states(hass: HomeAssistant, conf: dict[str, Any]) -> list[tuple[str, Any]]:
    """(object_id, state) for every non-excluded ``climate.*`` entity."""
    excluded = _exclude_set(conf)
    result: list[tuple[str, Any]] = []
    for state in hass.states.async_all("climate"):
        obj_id = state.entity_id.split(".", 1)[1]
        if obj_id in excluded:
            continue
        # Skip only genuinely absent states; unavailable ones are still listed
        # (with null cur/set) — the watch tolerates them.
        if state.state is None:
            continue
        result.append((obj_id, state))
    return result


def _read_state_float(hass: HomeAssistant, entity_id: str | None) -> float | None:
    """Read a source sensor as a float, or ``None`` if missing/unavailable."""
    if not entity_id:
        return None
    state = hass.states.get(entity_id)
    if state is None or str(state.state).lower() in _NO_VALUE:
        return None
    return _num(state.state)


def _climate_element(obj_id: str, state: Any) -> dict[str, Any]:
    """One element of GET /watch/climate/state (a climate-model object + id)."""
    attrs = state.attributes

    # Supported modes: heat_cool → auto, deduped (watch UI has one Auto button).
    modes: list[str] = []
    for mode in attrs.get("hvac_modes") or []:
        normalized = "auto" if mode == "heat_cool" else mode
        if normalized not in modes:
            modes.append(normalized)

    # Selected mode, same normalization.
    mode = state.state
    if mode == "heat_cool":
        mode = "auto"

    step = _num(attrs.get("target_temp_step"))
    if not step:
        step = 1.0

    # Setpoint: single ``temperature`` if present, else the dual midpoint.
    temp = _num(attrs.get("temperature"))
    if temp is not None:
        setpoint: float | None = temp
    else:
        low = _num(attrs.get("target_temp_low"))
        high = _num(attrs.get("target_temp_high"))
        if low is not None and high is not None:
            setpoint = _round_to_step((low + high) / 2.0, step)
        else:
            setpoint = None

    min_temp = _num(attrs.get("min_temp"))
    max_temp = _num(attrs.get("max_temp"))

    return {
        "id": obj_id,
        "name": attrs.get("friendly_name") or obj_id,
        "cur": _num(attrs.get("current_temperature")),
        "set": setpoint,
        "mode": mode,
        "action": attrs.get("hvac_action") or "idle",
        "min": min_temp if min_temp is not None else 45.0,
        "max": max_temp if max_temp is not None else 95.0,
        "step": step,
        "modes": modes,
    }


def climate_elements(hass: HomeAssistant, conf: dict[str, Any]) -> list[tuple[str, dict[str, Any]]]:
    """(object_id, climate-model dict) for every exposed climate entity.

    Shared by GET /watch/climate/state (the array) and the MQTT publisher
    (one retained ``watch/climate/<id>/state`` per element) — same
    ``_climate_element`` shape either way.
    """
    return [(obj_id, _climate_element(obj_id, state)) for obj_id, state in _exposed_states(hass, conf)]


def roster_ids(hass: HomeAssistant, conf: dict[str, Any]) -> list[str]:
    """Exposed climate object ids (the ``watch/climate/roster`` array)."""
    return [obj_id for obj_id, _ in _exposed_states(hass, conf)]


def energy_source_entities(conf: dict[str, Any]) -> list[str]:
    """The (configured) energy source entity_ids, for state-change tracking."""
    keys = (
        (CONF_BATTERY_PCT_ENTITY, DEFAULT_BATTERY_PCT_ENTITY),
        (CONF_SOLAR_W_ENTITY, DEFAULT_SOLAR_W_ENTITY),
        (CONF_GRID_W_ENTITY, DEFAULT_GRID_W_ENTITY),
        (CONF_CHARGING_ENTITY, ""),
    )
    out: list[str] = []
    for key, default in keys:
        ent = conf.get(key, default)
        if ent:
            out.append(ent)
    return out


def exposed_climate_entity_ids(hass: HomeAssistant, conf: dict[str, Any]) -> list[str]:
    """``climate.<id>`` entity_ids currently exposed (for state-change tracking)."""
    return [f"climate.{obj_id}" for obj_id, _ in _exposed_states(hass, conf)]


def energy_payload(hass: HomeAssistant, conf: dict[str, Any]) -> dict[str, Any]:
    """The ``parse_energy`` shape: {battery_pct, solar_w, grid_w, charging}.

    Shared by GET /watch/energy and the MQTT ``watch/energy/state`` publisher —
    ONE source of truth so the HTTP and MQTT payloads can never drift. Keys +
    signedness match ``src/net/mqtt_climate.rs::parse_energy`` exactly
    (``grid_w`` >0 import / <0 export; numeric fields null-tolerant).
    """
    battery = _read_state_float(hass, conf.get(CONF_BATTERY_PCT_ENTITY, DEFAULT_BATTERY_PCT_ENTITY))
    solar = _read_state_float(hass, conf.get(CONF_SOLAR_W_ENTITY, DEFAULT_SOLAR_W_ENTITY))
    grid = _read_state_float(hass, conf.get(CONF_GRID_W_ENTITY, DEFAULT_GRID_W_ENTITY))

    charging = False
    charging_entity = conf.get(CONF_CHARGING_ENTITY)
    if charging_entity:
        charging_state = hass.states.get(charging_entity)
        if charging_state is not None and str(charging_state.state).lower() in _TRUTHY:
            charging = True

    return {
        "battery_pct": None if battery is None else max(0, min(100, int(round(battery)))),
        "solar_w": None if solar is None else int(round(solar)),
        "grid_w": None if grid is None else int(round(grid)),
        "charging": charging,
    }


async def apply_climate_set(
    hass: HomeAssistant, conf: dict[str, Any], obj_id: str, body: dict[str, Any]
) -> tuple[bool, str]:
    """Apply a ``{"set":<f>}`` / ``{"mode":<str>}`` command to ``climate.<obj_id>``.

    The command body is IDENTICAL whether it arrived over HTTP POST
    (:8124/watch/climate/<id>/set) or MQTT (``watch/climate/<id>/set``, which
    the firmware publishes as ``{"set":72.0}`` / ``{"mode":"heat"}`` via
    ``climate_model::encode_set_*``). Returns ``(ok, reason)``; ``reason`` is
    ``""`` on success, a short tag otherwise. Never raises for a bad request.
    """
    if obj_id in _exclude_set(conf):
        return False, "unknown entity"

    entity_id = f"climate.{obj_id}"
    state = hass.states.get(entity_id)
    if state is None:
        return False, "unknown entity"
    if not isinstance(body, dict):
        return False, "bad body"

    attrs = state.attributes

    if "set" in body:
        try:
            target = float(body["set"])
        except (ValueError, TypeError):
            return False, "bad body"

        low = _num(attrs.get("target_temp_low"))
        high = _num(attrs.get("target_temp_high"))
        single = _num(attrs.get("temperature"))
        # Dual-setpoint (heat_cool) when the entity currently exposes low/high
        # and no single temperature, or its state is explicitly heat_cool.
        dual = low is not None and high is not None and (single is None or state.state == "heat_cool")

        if dual:
            spread = high - low
            if spread <= 0:
                spread = 4.0  # default ±2°F if no current spread
            half = spread / 2.0
            data = {
                "entity_id": entity_id,
                "target_temp_low": round(target - half, 2),
                "target_temp_high": round(target + half, 2),
            }
        else:
            data = {"entity_id": entity_id, "temperature": target}

        await hass.services.async_call("climate", "set_temperature", data, blocking=False)
        return True, ""

    if "mode" in body:
        requested = str(body["mode"])
        hvac_modes = attrs.get("hvac_modes") or []
        if requested == "auto":
            if "auto" in hvac_modes:
                target_mode: str | None = "auto"
            elif "heat_cool" in hvac_modes:
                target_mode = "heat_cool"
            else:
                target_mode = None
        else:
            target_mode = requested if requested in hvac_modes else None

        if target_mode is not None:
            await hass.services.async_call(
                "climate",
                "set_hvac_mode",
                {"entity_id": entity_id, "hvac_mode": target_mode},
                blocking=False,
            )
        # Unsupported mode is silently ignored; the watch reconciles on poll.
        return True, ""

    return False, "bad body"


def _safe(handler: Callable) -> Callable:
    """Wrap a handler so an unexpected error becomes a 500, never a crash."""

    @functools.wraps(handler)
    async def wrapper(request: web.Request) -> web.Response:
        try:
            return await handler(request)
        except Exception:  # noqa: BLE001 - the listener must survive any handler bug
            _LOGGER.exception("esp32c6_watch: %s failed", handler.__name__)
            return web.json_response({"error": "internal"}, status=500)

    return wrapper


# ---------------------------------------------------------------------------
# Token middleware
# ---------------------------------------------------------------------------
@web.middleware
async def token_middleware(request: web.Request, handler: Callable) -> web.Response:
    """Constant-time ``X-Watch-Token`` check when a token is configured."""
    token = _conf(request).get(CONF_TOKEN) or ""
    if token:
        provided = request.headers.get(TOKEN_HEADER, "")
        if not hmac.compare_digest(str(provided), str(token)):
            return web.json_response({"error": "unauthorized"}, status=401)
    return await handler(request)


# ---------------------------------------------------------------------------
# Handlers
# ---------------------------------------------------------------------------
@_safe
async def handle_state(request: web.Request) -> web.Response:
    """GET /watch/climate/state → array of climate-model state objects."""
    hass = _hass(request)
    conf = _conf(request)
    payload = [_climate_element(obj_id, state) for obj_id, state in _exposed_states(hass, conf)]
    return web.json_response(payload)


@_safe
async def handle_set(request: web.Request) -> web.Response:
    """POST /watch/climate/<object_id>/set → set_temperature / set_hvac_mode."""
    hass = _hass(request)
    conf = _conf(request)
    obj_id = request.match_info["object_id"]

    try:
        body = await request.json()
    except Exception:  # noqa: BLE001 - malformed JSON is a client error, not a bug
        return web.json_response({"error": "bad body"}, status=400)

    ok, reason = await apply_climate_set(hass, conf, obj_id, body)
    if ok:
        return web.json_response({"ok": True})
    status = 404 if reason == "unknown entity" else 400
    return web.json_response({"error": reason}, status=status)


@_safe
async def handle_energy(request: web.Request) -> web.Response:
    """GET /watch/energy → the parse_energy summary shape."""
    return web.json_response(energy_payload(_hass(request), _conf(request)))


@_safe
async def handle_roster(request: web.Request) -> web.Response:
    """GET /watch/climate/roster → informational array of exposed object ids."""
    hass = _hass(request)
    conf = _conf(request)
    return web.json_response([obj_id for obj_id, _ in _exposed_states(hass, conf)])


@_safe
async def handle_version(request: web.Request) -> web.Response:
    """GET /watch/version → component/version/entity-count liveness probe."""
    hass = _hass(request)
    conf = _conf(request)
    return web.json_response(
        {
            "component": DOMAIN,
            "version": VERSION,
            "entities": len(_exposed_states(hass, conf)),
        }
    )


@_safe
async def handle_announce_pending(request: web.Request) -> web.Response:
    """GET /watch/announce/pending → {"pending": bool, "bytes": int}.

    A cheap poll the watch hits often to decide whether to fetch audio.
    """
    pending, total = _queue(request).pending()
    return web.json_response({"pending": pending, "bytes": total})


@_safe
async def handle_announce(request: web.Request) -> web.Response:
    """GET /watch/announce → next queued PCM clip (and dequeue it), or 204.

    Body is raw headerless 16 kHz mono s16le PCM (``application/octet-stream``)
    — exactly the format the watch feeds its shared I2S TX. ``204 No Content``
    when the queue is empty.
    """
    clip = await _queue(request).get()
    if clip is None:
        return web.Response(status=204)
    return web.Response(body=clip, content_type="application/octet-stream")


def async_register_routes(app: web.Application) -> None:
    """Wire the endpoints onto the component's aiohttp app."""
    app.router.add_get("/watch/climate/state", handle_state)
    app.router.add_get("/watch/climate/roster", handle_roster)
    app.router.add_post("/watch/climate/{object_id}/set", handle_set)
    app.router.add_get("/watch/energy", handle_energy)
    app.router.add_get("/watch/version", handle_version)
    app.router.add_get("/watch/announce/pending", handle_announce_pending)
    app.router.add_get("/watch/announce", handle_announce)
