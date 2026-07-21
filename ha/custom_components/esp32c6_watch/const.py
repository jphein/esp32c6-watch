"""Constants for the ESP32-C6 Watch integration."""

from __future__ import annotations

DOMAIN = "esp32c6_watch"

# Kept in lock-step with the ``version`` field in manifest.json and surfaced by
# GET /watch/version so the firmware / realm-sigil tooling can probe liveness.
VERSION = "0.1.0"

# --- Config / options keys -------------------------------------------------
CONF_PORT = "port"
CONF_TOKEN = "token"
CONF_CLIMATE_EXCLUDE = "climate_exclude"
CONF_BATTERY_PCT_ENTITY = "battery_pct_entity"
CONF_SOLAR_W_ENTITY = "solar_w_entity"
CONF_GRID_W_ENTITY = "grid_w_entity"
CONF_CHARGING_ENTITY = "charging_entity"

# --- Defaults (discovered from live HA, 2026-07-21) ------------------------
# Dedicated plain-HTTP port; bypasses HA's TLS/auth on :8123 (see design doc).
DEFAULT_PORT = 8124

# The two ``*_mqtt_hvac`` duplicates that mirror the minisplits are hidden by
# default. Stored as a comma-separated list of object ids (``climate.`` prefix
# optional — it is stripped when parsed).
DEFAULT_CLIMATE_EXCLUDE = "kitchen_mqtt_hvac, bedroom_mqtt_hvac"

DEFAULT_BATTERY_PCT_ENTITY = "sensor.battery_average_soc"
DEFAULT_SOLAR_W_ENTITY = "sensor.total_solar_power"
DEFAULT_GRID_W_ENTITY = "sensor.solar_arbitrage_grid_draw"
DEFAULT_CHARGING_ENTITY = ""

# HTTP header carrying the optional shared secret.
TOKEN_HEADER = "X-Watch-Token"
