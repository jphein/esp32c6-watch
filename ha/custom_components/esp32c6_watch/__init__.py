"""ESP32-C6 Watch — a native HA component that serves plain HTTP to the watch.

Replaces the interim Node-RED + MQTT climate bridge. On setup it starts its own
``aiohttp`` web server on a dedicated plain-HTTP port (default 8124) bound to
``0.0.0.0`` inside HA's event loop. Because HA core is host-networked, the socket
answers on every VM leg — including the VLAN-11 leg (10.0.11.110) that is on the
same L2 as the watch's ``roam`` network. HA's own :8123 is HTTPS-only and the
watch has no TLS, so this cannot be a ``HomeAssistantView``.
"""

from __future__ import annotations

import logging
from typing import Any

from aiohttp import web

from homeassistant.config_entries import ConfigEntry
from homeassistant.const import EVENT_HOMEASSISTANT_STARTED, Platform
from homeassistant.core import HomeAssistant
from homeassistant.exceptions import ConfigEntryNotReady

from .announce import AnnounceQueue
from .api import (
    APP_ENTRY,
    APP_HASS,
    APP_QUEUE,
    async_register_routes,
    token_middleware,
)
from .const import (
    CONF_MAX_QUEUE_BYTES,
    CONF_PORT,
    DEFAULT_MAX_QUEUE_BYTES,
    DEFAULT_PORT,
    DOMAIN,
)
from .mqtt_bridge import WatchMqttBridge

_LOGGER = logging.getLogger(__name__)

# The speaker capability is an actual HA entity; the climate/energy endpoints
# are pure HTTP and create none.
PLATFORMS = [Platform.MEDIA_PLAYER]


async def async_setup_entry(hass: HomeAssistant, entry: ConfigEntry) -> bool:
    """Start the plain-HTTP listener for the watch."""
    hass.data.setdefault(DOMAIN, {})

    conf = {**entry.data, **entry.options}
    try:
        port = int(conf.get(CONF_PORT, DEFAULT_PORT))
    except (TypeError, ValueError):
        port = DEFAULT_PORT
    try:
        max_queue_bytes = int(conf.get(CONF_MAX_QUEUE_BYTES, DEFAULT_MAX_QUEUE_BYTES))
    except (TypeError, ValueError):
        max_queue_bytes = DEFAULT_MAX_QUEUE_BYTES

    # Shared by the media_player (producer) and the /watch/announce handlers
    # (consumer). Created before the app so the handlers see it immediately.
    queue = AnnounceQueue(max_queue_bytes)

    app = web.Application(middlewares=[token_middleware])
    app[APP_HASS] = hass
    app[APP_ENTRY] = entry
    app[APP_QUEUE] = queue
    async_register_routes(app)

    runner = web.AppRunner(app)
    await runner.setup()
    site = web.TCPSite(runner, "0.0.0.0", port)
    try:
        await site.start()
    except OSError as err:
        await runner.cleanup()
        raise ConfigEntryNotReady(f"cannot bind 0.0.0.0:{port}: {err}") from err

    hass.data[DOMAIN][entry.entry_id] = {"runner": runner, "queue": queue}
    _LOGGER.info("esp32c6_watch: serving watch HTTP on 0.0.0.0:%s", port)

    # #60: ALSO publish the watch/* retained MQTT topics the firmware parser
    # reads (the HTTP endpoints alone left Climate + Energy blank once the
    # Node-RED bridge was retired). Best-effort: a MQTT-less HA still serves
    # HTTP. Started after HA is running so the initial snapshot reads live
    # states, not boot-time `unavailable`.
    #
    # Registered BEFORE the platform forward on purpose: the MQTT bridge is the
    # climate/energy critical path, so it must not depend on the (secondary)
    # media_player platform loading — a platform import break must never take
    # the watch's data path down with it.
    bridge = WatchMqttBridge(hass, entry)
    hass.data[DOMAIN][entry.entry_id]["bridge"] = bridge
    if hass.is_running:
        await bridge.async_start()
    else:
        async def _start_bridge(_event: Any) -> None:
            await bridge.async_start()

        entry.async_on_unload(
            hass.bus.async_listen_once(EVENT_HOMEASSISTANT_STARTED, _start_bridge)
        )

    # The media_player platform reads the queue back out of hass.data on setup.
    await hass.config_entries.async_forward_entry_setups(entry, PLATFORMS)

    # Reload (rebind / re-read entity map) when the options flow saves changes.
    entry.async_on_unload(entry.add_update_listener(_async_update_listener))
    return True


async def async_unload_entry(hass: HomeAssistant, entry: ConfigEntry) -> bool:
    """Tear the listener + media_player down cleanly."""
    unload_ok = await hass.config_entries.async_unload_platforms(entry, PLATFORMS)
    if unload_ok:
        data = hass.data.get(DOMAIN, {}).pop(entry.entry_id, None)
        if data is not None:
            bridge: WatchMqttBridge | None = data.get("bridge")
            if bridge is not None:
                await bridge.async_stop()
            runner: web.AppRunner | None = data.get("runner")
            if runner is not None:
                await runner.cleanup()
    return unload_ok


async def _async_update_listener(hass: HomeAssistant, entry: ConfigEntry) -> None:
    """Options changed → reload the entry so the new port/config takes effect."""
    await hass.config_entries.async_reload(entry.entry_id)
