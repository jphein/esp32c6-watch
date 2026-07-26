"""MQTT bridge — publish the ``watch/*`` topics the firmware parser reads (#60).

The watch firmware (``src/net/mqtt_climate.rs``) consumes **retained MQTT**, not
HTTP:

    watch/climate/<object_id>/state  (retained)  climate-model JSON per entity
    watch/climate/roster             (retained)  object-id array
    watch/energy/state               (retained)  {battery_pct,solar_w,grid_w,charging}
    watch/energy/avail               (retained)  "online" / "offline"

and it PUBLISHes commands to:

    watch/climate/<id>/set                        {"set":72.0} / {"mode":"heat"}

The old Node-RED bridge that fed these is gone and this component served HTTP
only, so all four topics sat EMPTY — the Climate + Energy apps showed nothing.
This bridge republishes the SAME data the HTTP endpoints already compute (the
shared helpers in ``api.py`` — one source of truth, no drift) onto MQTT: once
at startup and on every relevant HA state-change, and it subscribes the ``set``
topic so watch→HA setpoint/mode changes still work. The HTTP endpoints stay
(the speaker path + belt-and-suspenders).

All traffic rides HA's own ``mqtt`` integration (``async_publish`` /
``async_subscribe``) to the broker HA already uses — no second client. We can't
register a broker-level LWT through ``async_publish``, so availability is
best-effort: ``online`` retained at start, ``offline`` retained on a clean HA
stop. If MQTT isn't configured the bridge no-ops and the HTTP endpoints carry
on unaffected.
"""

from __future__ import annotations

import json
import logging
from typing import Any

from homeassistant.config_entries import ConfigEntry
from homeassistant.const import EVENT_HOMEASSISTANT_STOP
from homeassistant.core import CALLBACK_TYPE, Event, HomeAssistant, callback
from homeassistant.helpers.event import async_track_state_change_event

from . import api

_LOGGER = logging.getLogger(__name__)

# --- Topic layout (must match src/net/mqtt_climate.rs) ---------------------
CLIMATE_STATE_FMT = "watch/climate/{obj}/state"
CLIMATE_ROSTER_TOPIC = "watch/climate/roster"
CLIMATE_SET_WILDCARD = "watch/climate/+/set"
ENERGY_STATE_TOPIC = "watch/energy/state"
ENERGY_AVAIL_TOPIC = "watch/energy/avail"


class WatchMqttBridge:
    """Publishes watch/* retained state and services watch/climate/+/set."""

    def __init__(self, hass: HomeAssistant, entry: ConfigEntry) -> None:
        self._hass = hass
        self._entry = entry
        self._unsubs: list[CALLBACK_TYPE] = []
        self._started = False

    def _conf(self) -> dict[str, Any]:
        """Merged config (options win) — read fresh so a reload picks it up."""
        return {**self._entry.data, **self._entry.options}

    async def async_start(self) -> bool:
        """Publish the initial snapshot + wire listeners/command subscription.

        Returns ``True`` if the bridge came up, ``False`` if MQTT is
        unavailable (the component keeps serving HTTP regardless).
        """
        try:
            from homeassistant.components import mqtt  # noqa: PLC0415
        except ImportError:
            _LOGGER.warning("esp32c6_watch: mqtt integration not present - MQTT bridge disabled")
            return False

        # MQTT must have a live client before we publish/subscribe. This is
        # False when the mqtt integration isn't set up at all.
        try:
            available = await mqtt.async_wait_for_mqtt_client(self._hass)
        except Exception:  # noqa: BLE001 - never let a bridge problem fail the component
            available = False
        if not available:
            _LOGGER.warning(
                "esp32c6_watch: MQTT client unavailable - bridge disabled (HTTP still serves)"
            )
            return False

        self._mqtt = mqtt

        # 1) Command path first, so a watch→HA set issued during our startup
        #    publish isn't missed.
        self._unsubs.append(
            await mqtt.async_subscribe(self._hass, CLIMATE_SET_WILDCARD, self._on_set_message, qos=0)
        )

        # 2) Initial retained snapshot (climate + roster + energy + avail).
        await self.async_publish_climate()
        await self.async_publish_energy()
        await self._publish(ENERGY_AVAIL_TOPIC, "online")

        # 3) Live updates: republish on the exposed climate entities' changes
        #    and the energy source entities' changes (two trackers so climate
        #    churn doesn't rewrite energy and vice-versa).
        conf = self._conf()
        climate_ids = api.exposed_climate_entity_ids(self._hass, conf)
        if climate_ids:
            self._unsubs.append(
                async_track_state_change_event(self._hass, climate_ids, self._on_climate_change)
            )
        energy_ids = api.energy_source_entities(conf)
        if energy_ids:
            self._unsubs.append(
                async_track_state_change_event(self._hass, energy_ids, self._on_energy_change)
            )

        # 4) Best-effort offline on a clean HA stop (no per-topic LWT via
        #    async_publish).
        self._unsubs.append(
            self._hass.bus.async_listen_once(EVENT_HOMEASSISTANT_STOP, self._on_hass_stop)
        )

        self._started = True
        _LOGGER.info(
            "esp32c6_watch: MQTT bridge up (%d climate, %d energy sources)",
            len(climate_ids),
            len(energy_ids),
        )
        return True

    async def async_stop(self) -> None:
        """Drop listeners + the command subscription (entry unload/reload).

        Availability is left ``online`` on a reload — only a real HA stop
        publishes ``offline`` (via the once-listener). A reload re-runs
        ``async_start`` and re-asserts everything.
        """
        for unsub in self._unsubs:
            try:
                unsub()
            except Exception:  # noqa: BLE001
                _LOGGER.debug("esp32c6_watch: unsub failed", exc_info=True)
        self._unsubs.clear()
        self._started = False

    # --- Publishers --------------------------------------------------------
    async def _publish(self, topic: str, payload: str, retain: bool = True) -> None:
        try:
            await self._mqtt.async_publish(self._hass, topic, payload, qos=0, retain=retain)
        except Exception:  # noqa: BLE001 - a publish failure must never crash HA
            _LOGGER.exception("esp32c6_watch: publish to %s failed", topic)

    async def async_publish_climate(self) -> None:
        """One retained ``watch/climate/<id>/state`` per exposed entity + roster."""
        conf = self._conf()
        elements = api.climate_elements(self._hass, conf)
        for obj_id, element in elements:
            await self._publish(
                CLIMATE_STATE_FMT.format(obj=obj_id),
                json.dumps(element, separators=(",", ":")),
            )
        await self._publish(
            CLIMATE_ROSTER_TOPIC,
            json.dumps([obj_id for obj_id, _ in elements], separators=(",", ":")),
        )

    async def async_publish_energy(self) -> None:
        """Retained ``watch/energy/state`` in the firmware's parse_energy shape."""
        payload = api.energy_payload(self._hass, self._conf())
        await self._publish(ENERGY_STATE_TOPIC, json.dumps(payload, separators=(",", ":")))

    # --- Listeners ---------------------------------------------------------
    @callback
    def _on_climate_change(self, event: Event) -> None:
        # Republish all climate + roster (the exposed set is small; retained
        # publishes are idempotent). Scheduled as a task — the listener is sync.
        self._hass.async_create_task(self.async_publish_climate())

    @callback
    def _on_energy_change(self, event: Event) -> None:
        self._hass.async_create_task(self.async_publish_energy())

    async def _on_hass_stop(self, event: Event) -> None:
        await self._publish(ENERGY_AVAIL_TOPIC, "offline")

    async def _on_set_message(self, msg: Any) -> None:
        """Handle a watch→HA command on ``watch/climate/<id>/set``.

        The firmware publishes NON-retained (``{"set":72.0}`` / ``{"mode":"heat"}``);
        ignore any retained delivery so a stale command can't replay on our
        subscribe. Body shape is identical to the HTTP POST, so it goes through
        the same ``apply_climate_set``.
        """
        if getattr(msg, "retain", False):
            return  # never replay a retained command
        parts = msg.topic.split("/")
        # watch / climate / <obj> / set
        if len(parts) != 4:
            return
        obj_id = parts[2]
        raw = msg.payload
        if isinstance(raw, (bytes, bytearray)):
            try:
                raw = raw.decode("utf-8")
            except UnicodeDecodeError:
                return
        try:
            body = json.loads(raw)
        except (ValueError, TypeError):
            _LOGGER.debug("esp32c6_watch: bad set payload on %s: %r", msg.topic, msg.payload)
            return
        ok, reason = await api.apply_climate_set(self._hass, self._conf(), obj_id, body)
        if not ok:
            _LOGGER.debug("esp32c6_watch: set %s rejected (%s)", obj_id, reason)
        # HA's state-change listener will republish the entity's retained state
        # once the service call lands, closing the round trip for the watch.
