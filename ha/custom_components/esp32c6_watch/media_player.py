"""media_player platform: turns the watch into a TTS / announcement target.

HA automations and ``tts.speak`` target the "ESP32-C6 Watch" media_player. On
``async_play_media`` the entity resolves the media (a TTS-proxy URL or a
``media-source://`` URI), transcodes it to the watch's headerless 16 kHz mono
s16le PCM with HA's ffmpeg helper, and enqueues it on the shared
:class:`AnnounceQueue`. The watch pulls it via ``GET /watch/announce`` (see
``api.py``) — this platform never pushes to the device, so a media_player fits
the watch's existing HTTP-pull model without a persistent connection.
"""

from __future__ import annotations

import asyncio
import logging

from homeassistant.components import media_source
from homeassistant.components.ffmpeg import get_ffmpeg_manager
from homeassistant.components.media_player import (
    MediaPlayerEntity,
    MediaPlayerEntityFeature,
    MediaPlayerState,
    MediaType,
    async_process_play_media_url,
)
from homeassistant.config_entries import ConfigEntry
from homeassistant.core import HomeAssistant
from homeassistant.helpers.aiohttp_client import async_get_clientsession

try:
    # HA < 2024.1 shipped DeviceInfo in its own short-lived module...
    from homeassistant.helpers.device_info import DeviceInfo
except ImportError:  # ...removed in modern HA — it lives in device_registry now.
    from homeassistant.helpers.device_registry import DeviceInfo
from homeassistant.helpers.entity_platform import AddEntitiesCallback

from .announce import AnnounceQueue
from .const import (
    DOMAIN,
    MEDIA_PLAYER_UNIQUE_SUFFIX,
    PCM_CHANNELS,
    PCM_SAMPLE_FMT,
    PCM_SAMPLE_RATE,
)

_LOGGER = logging.getLogger(__name__)

# Upper bound on the source fetch + ffmpeg transcode, so a wedged render can
# never hang the play_media service call forever.
_TRANSCODE_TIMEOUT = 30


async def async_setup_entry(
    hass: HomeAssistant,
    entry: ConfigEntry,
    async_add_entities: AddEntitiesCallback,
) -> None:
    """Set up the single watch media_player from the config entry."""
    queue: AnnounceQueue = hass.data[DOMAIN][entry.entry_id]["queue"]
    async_add_entities([WatchMediaPlayer(entry, queue)])


async def _async_transcode_to_pcm(hass: HomeAssistant, url: str) -> bytes:
    """Fetch ``url`` and transcode it to headerless 16 kHz mono s16le PCM."""
    session = async_get_clientsession(hass)
    binary = get_ffmpeg_manager(hass).binary

    async with asyncio.timeout(_TRANSCODE_TIMEOUT):
        resp = await session.get(url)
        resp.raise_for_status()
        source = await resp.read()

        proc = await asyncio.create_subprocess_exec(
            binary,
            "-hide_banner",
            "-loglevel",
            "error",
            "-i",
            "pipe:0",
            "-f",
            PCM_SAMPLE_FMT,
            "-acodec",
            "pcm_s16le",
            "-ac",
            str(PCM_CHANNELS),
            "-ar",
            str(PCM_SAMPLE_RATE),
            "pipe:1",
            stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        out, err = await proc.communicate(source)

    if proc.returncode != 0:
        detail = err.decode("utf-8", "replace").strip()[:200]
        raise RuntimeError(f"ffmpeg exited {proc.returncode}: {detail}")
    return out


class WatchMediaPlayer(MediaPlayerEntity):
    """A play-only announcer whose audio the watch pulls on its own schedule."""

    _attr_has_entity_name = True
    _attr_name = None
    _attr_should_poll = False
    _attr_media_content_type = MediaType.MUSIC
    _attr_supported_features = (
        MediaPlayerEntityFeature.PLAY_MEDIA | MediaPlayerEntityFeature.MEDIA_ANNOUNCE
    )

    def __init__(self, entry: ConfigEntry, queue: AnnounceQueue) -> None:
        self._entry = entry
        self._queue = queue
        self._attr_unique_id = f"{entry.entry_id}_{MEDIA_PLAYER_UNIQUE_SUFFIX}"
        self._attr_device_info = DeviceInfo(
            identifiers={(DOMAIN, entry.entry_id)},
            name="ESP32-C6 Watch",
            manufacturer="jphein",
            model="ESP32-C6 Watch",
        )

    @property
    def state(self) -> MediaPlayerState:
        """PLAYING while clips are queued for the watch, else IDLE.

        We can't observe the watch's actual playback (it pulls), so "audio is
        waiting to be pulled" is the honest, useful proxy.
        """
        pending, _ = self._queue.pending()
        return MediaPlayerState.PLAYING if pending else MediaPlayerState.IDLE

    async def async_added_to_hass(self) -> None:
        """Refresh HA state whenever the queue gains or loses a clip."""
        self._queue.on_change = self._handle_queue_change

    async def async_will_remove_from_hass(self) -> None:
        self._queue.on_change = None

    def _handle_queue_change(self) -> None:
        self.async_write_ha_state()

    async def async_play_media(
        self, media_type: str, media_id: str, **kwargs
    ) -> None:
        """Render whatever HA hands us to PCM and enqueue it for the watch.

        Covers ``tts.speak`` (which resolves to a TTS-proxy URL) and the media
        browser / automations (``media-source://`` URIs). The ``announce`` kwarg
        that assist satellites pass is a no-op here — every clip is effectively
        an announcement.
        """
        if media_source.is_media_source_id(media_id):
            play_item = await media_source.async_resolve_media(
                self.hass, media_id, self.entity_id
            )
            media_id = play_item.url

        media_id = async_process_play_media_url(self.hass, media_id)
        pcm = await _async_transcode_to_pcm(self.hass, media_id)
        if not pcm:
            _LOGGER.warning("esp32c6_watch: transcode produced no PCM for %s", media_id)
            return
        await self._queue.put(pcm)
        _LOGGER.debug("esp32c6_watch: queued %d bytes of PCM for the watch", len(pcm))
