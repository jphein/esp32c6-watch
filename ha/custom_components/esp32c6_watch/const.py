"""Constants for the ESP32-C6 Watch integration."""

from __future__ import annotations

DOMAIN = "esp32c6_watch"

# Kept in lock-step with the ``version`` field in manifest.json and surfaced by
# GET /watch/version so the firmware / realm-sigil tooling can probe liveness.
# 0.3.0 (#60): adds the MQTT bridge — publishes the watch/* retained topics the
# firmware parser reads, alongside the existing HTTP endpoints.
VERSION = "0.3.0"

# --- Config / options keys -------------------------------------------------
CONF_PORT = "port"
CONF_TOKEN = "token"
CONF_CLIMATE_EXCLUDE = "climate_exclude"
CONF_BATTERY_PCT_ENTITY = "battery_pct_entity"
CONF_SOLAR_W_ENTITY = "solar_w_entity"
CONF_GRID_W_ENTITY = "grid_w_entity"
CONF_CHARGING_ENTITY = "charging_entity"
CONF_MAX_QUEUE_BYTES = "max_queue_bytes"

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

# --- Speaker / announce queue ----------------------------------------------
# The watch is a pull consumer: HA transcodes TTS / media to PCM and queues it;
# the watch drains the queue over HTTP. The FIFO is capped by total bytes so a
# sleeping / off-net watch can never grow HA's memory without bound.
# ~2 MiB ≈ 64 s of 16 kHz mono s16le audio.
DEFAULT_MAX_QUEUE_BYTES = 2 * 1024 * 1024

# PCM the watch consumes — identical to its STT upload format: headerless
# 16 kHz mono signed 16-bit little-endian. Fixed (not configurable): it must
# match the firmware's I2S TX expectation exactly.
PCM_SAMPLE_RATE = 16000
PCM_CHANNELS = 1
PCM_SAMPLE_FMT = "s16le"

# unique-id suffix for the single media_player entity.
MEDIA_PLAYER_UNIQUE_SUFFIX = "media_player"

# HTTP header carrying the optional shared secret.
TOKEN_HEADER = "X-Watch-Token"
