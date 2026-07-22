"""Bounded FIFO of PCM announce clips awaiting pickup by the watch.

The watch is a *pull* consumer, matching the rest of its net stack: HA renders
TTS / media to headerless 16 kHz mono s16le PCM (see ``media_player.py``) and
enqueues it here; the watch polls ``GET /watch/announce/pending`` and drains
``GET /watch/announce`` (see ``api.py``). Nothing is ever pushed to the device.

The queue is capped by *total bytes* — the oldest clips are dropped first when a
new clip would overflow the cap, so a watch that is asleep or off-net can never
grow HA's memory without bound.

Stdlib-only on purpose (no aiohttp / HA imports) so the queue logic is directly
unit-testable off-target.
"""

from __future__ import annotations

import asyncio
import logging
from collections import deque
from typing import Callable

_LOGGER = logging.getLogger(__name__)


class AnnounceQueue:
    """Byte-bounded FIFO of PCM clips (drop-oldest on overflow)."""

    def __init__(self, max_bytes: int) -> None:
        self._max_bytes = max(1, int(max_bytes))
        self._clips: deque[bytes] = deque()
        self._total = 0
        self._lock = asyncio.Lock()
        # Optional "queue changed" hook; the media_player uses it to refresh its
        # HA state when a clip is enqueued or drained.
        self.on_change: Callable[[], None] | None = None

    async def put(self, clip: bytes) -> None:
        """Enqueue a clip, dropping the oldest clips first to honor the cap."""
        if not clip:
            return
        async with self._lock:
            while self._clips and self._total + len(clip) > self._max_bytes:
                dropped = self._clips.popleft()
                self._total -= len(dropped)
                _LOGGER.warning(
                    "esp32c6_watch: announce queue full; dropped oldest clip "
                    "(%d bytes) to stay under %d",
                    len(dropped),
                    self._max_bytes,
                )
            if len(clip) > self._max_bytes:
                # A single clip larger than the whole cap is enqueued anyway —
                # dropping it would silence the announcement — but flagged.
                _LOGGER.warning(
                    "esp32c6_watch: announce clip (%d bytes) exceeds cap %d; "
                    "enqueued regardless",
                    len(clip),
                    self._max_bytes,
                )
            self._clips.append(clip)
            self._total += len(clip)
        self._notify()

    async def get(self) -> bytes | None:
        """Pop and return the oldest clip, or ``None`` if the queue is empty."""
        async with self._lock:
            if not self._clips:
                return None
            clip = self._clips.popleft()
            self._total -= len(clip)
        self._notify()
        return clip

    def pending(self) -> tuple[bool, int]:
        """Cheap snapshot for the poll endpoint: ``(has_clips, total_bytes)``.

        Lock-free by design — the event loop is single-threaded, so reading the
        two attributes together is consistent, and the poll must stay cheap.
        """
        return bool(self._clips), self._total

    def _notify(self) -> None:
        cb = self.on_change
        if cb is None:
            return
        try:
            cb()
        except Exception:  # noqa: BLE001 - a state-refresh bug must not break the queue
            _LOGGER.exception("esp32c6_watch: announce queue on_change hook failed")
