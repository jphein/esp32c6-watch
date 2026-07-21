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

from aiohttp import web

from homeassistant.config_entries import ConfigEntry
from homeassistant.core import HomeAssistant
from homeassistant.exceptions import ConfigEntryNotReady

from .api import (
    APP_ENTRY,
    APP_HASS,
    async_register_routes,
    token_middleware,
)
from .const import CONF_PORT, DEFAULT_PORT, DOMAIN

_LOGGER = logging.getLogger(__name__)


async def async_setup_entry(hass: HomeAssistant, entry: ConfigEntry) -> bool:
    """Start the plain-HTTP listener for the watch."""
    hass.data.setdefault(DOMAIN, {})

    conf = {**entry.data, **entry.options}
    try:
        port = int(conf.get(CONF_PORT, DEFAULT_PORT))
    except (TypeError, ValueError):
        port = DEFAULT_PORT

    app = web.Application(middlewares=[token_middleware])
    app[APP_HASS] = hass
    app[APP_ENTRY] = entry
    async_register_routes(app)

    runner = web.AppRunner(app)
    await runner.setup()
    site = web.TCPSite(runner, "0.0.0.0", port)
    try:
        await site.start()
    except OSError as err:
        await runner.cleanup()
        raise ConfigEntryNotReady(f"cannot bind 0.0.0.0:{port}: {err}") from err

    hass.data[DOMAIN][entry.entry_id] = runner
    _LOGGER.info("esp32c6_watch: serving watch HTTP on 0.0.0.0:%s", port)

    # Reload (rebind / re-read entity map) when the options flow saves changes.
    entry.async_on_unload(entry.add_update_listener(_async_update_listener))
    return True


async def async_unload_entry(hass: HomeAssistant, entry: ConfigEntry) -> bool:
    """Tear the listener down cleanly."""
    runner: web.AppRunner | None = hass.data.get(DOMAIN, {}).pop(entry.entry_id, None)
    if runner is not None:
        await runner.cleanup()
    return True


async def _async_update_listener(hass: HomeAssistant, entry: ConfigEntry) -> None:
    """Options changed → reload the entry so the new port/config takes effect."""
    await hass.config_entries.async_reload(entry.entry_id)
