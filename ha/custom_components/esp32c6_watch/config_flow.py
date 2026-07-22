"""Config + options flow for the ESP32-C6 Watch integration.

Single instance. All fields have sensible defaults discovered from live HA
(2026-07-21), so "just install it" works with no editing.
"""

from __future__ import annotations

from typing import Any

import voluptuous as vol

from homeassistant.config_entries import (
    ConfigEntry,
    ConfigFlow,
    ConfigFlowResult,
    OptionsFlow,
)
from homeassistant.core import callback
from homeassistant.helpers import selector

from .const import (
    CONF_BATTERY_PCT_ENTITY,
    CONF_CHARGING_ENTITY,
    CONF_CLIMATE_EXCLUDE,
    CONF_GRID_W_ENTITY,
    CONF_MAX_QUEUE_BYTES,
    CONF_PORT,
    CONF_SOLAR_W_ENTITY,
    CONF_TOKEN,
    DEFAULT_BATTERY_PCT_ENTITY,
    DEFAULT_CLIMATE_EXCLUDE,
    DEFAULT_GRID_W_ENTITY,
    DEFAULT_MAX_QUEUE_BYTES,
    DEFAULT_PORT,
    DEFAULT_SOLAR_W_ENTITY,
    DOMAIN,
)


def _build_schema(defaults: dict[str, Any]) -> vol.Schema:
    """Form schema for both the config and options steps, pre-filled."""
    schema: dict[Any, Any] = {
        vol.Required(
            CONF_PORT, default=defaults.get(CONF_PORT, DEFAULT_PORT)
        ): selector.NumberSelector(
            selector.NumberSelectorConfig(
                min=1, max=65535, step=1, mode=selector.NumberSelectorMode.BOX
            )
        ),
        vol.Optional(
            CONF_TOKEN, default=defaults.get(CONF_TOKEN, "")
        ): selector.TextSelector(
            selector.TextSelectorConfig(type=selector.TextSelectorType.PASSWORD)
        ),
        vol.Required(
            CONF_MAX_QUEUE_BYTES,
            default=defaults.get(CONF_MAX_QUEUE_BYTES, DEFAULT_MAX_QUEUE_BYTES),
        ): selector.NumberSelector(
            selector.NumberSelectorConfig(
                min=32768,
                max=16777216,
                step=1,
                mode=selector.NumberSelectorMode.BOX,
                unit_of_measurement="bytes",
            )
        ),
        vol.Optional(
            CONF_CLIMATE_EXCLUDE,
            default=defaults.get(CONF_CLIMATE_EXCLUDE, DEFAULT_CLIMATE_EXCLUDE),
        ): selector.TextSelector(),
        vol.Required(
            CONF_BATTERY_PCT_ENTITY,
            default=defaults.get(CONF_BATTERY_PCT_ENTITY, DEFAULT_BATTERY_PCT_ENTITY),
        ): selector.EntitySelector(selector.EntitySelectorConfig(domain="sensor")),
        vol.Required(
            CONF_SOLAR_W_ENTITY,
            default=defaults.get(CONF_SOLAR_W_ENTITY, DEFAULT_SOLAR_W_ENTITY),
        ): selector.EntitySelector(selector.EntitySelectorConfig(domain="sensor")),
        vol.Required(
            CONF_GRID_W_ENTITY,
            default=defaults.get(CONF_GRID_W_ENTITY, DEFAULT_GRID_W_ENTITY),
        ): selector.EntitySelector(selector.EntitySelectorConfig(domain="sensor")),
    }

    # Charging is truly optional — only pre-fill a default when one is set, so
    # the entity selector isn't handed an empty string.
    charging_default = defaults.get(CONF_CHARGING_ENTITY)
    charging_selector = selector.EntitySelector(
        selector.EntitySelectorConfig(domain=["binary_sensor", "sensor", "switch"])
    )
    if charging_default:
        schema[vol.Optional(CONF_CHARGING_ENTITY, default=charging_default)] = charging_selector
    else:
        schema[vol.Optional(CONF_CHARGING_ENTITY)] = charging_selector

    return vol.Schema(schema)


class Esp32c6WatchConfigFlow(ConfigFlow, domain=DOMAIN):
    """Handle the initial config flow (single instance)."""

    VERSION = 1

    async def async_step_user(
        self, user_input: dict[str, Any] | None = None
    ) -> ConfigFlowResult:
        """Create the one entry, or abort if already configured."""
        if self._async_current_entries():
            return self.async_abort(reason="single_instance_allowed")

        if user_input is not None:
            return self.async_create_entry(title="ESP32-C6 Watch", data=user_input)

        return self.async_show_form(step_id="user", data_schema=_build_schema({}))

    @staticmethod
    @callback
    def async_get_options_flow(config_entry: ConfigEntry) -> OptionsFlow:
        """Return the options flow."""
        return Esp32c6WatchOptionsFlow(config_entry)


class Esp32c6WatchOptionsFlow(OptionsFlow):
    """Edit port / token / entity map after setup."""

    def __init__(self, config_entry: ConfigEntry) -> None:
        self.config_entry = config_entry

    async def async_step_init(
        self, user_input: dict[str, Any] | None = None
    ) -> ConfigFlowResult:
        """Show / persist the options form."""
        if user_input is not None:
            return self.async_create_entry(title="", data=user_input)

        current = {**self.config_entry.data, **self.config_entry.options}
        return self.async_show_form(step_id="init", data_schema=_build_schema(current))
