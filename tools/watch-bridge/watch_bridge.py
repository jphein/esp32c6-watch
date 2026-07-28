#!/usr/bin/env python3
"""
watch_bridge — thin LAN HTTP bridge: ESP32-C6 watch mic audio -> Azure STT.

The watch is plain-HTTP only (no TLS), so it POSTs raw mic PCM to this bridge on
the LAN; the bridge reuses speech-to-cli's Azure REST transcriber (speech.py) and
returns the transcript as JSON. The Azure key stays here (never on the watch).

Zero external deps beyond speech-to-cli itself (stdlib http.server + `requests`,
which speech.py already uses). v1 = one short clip per request (push-to-talk).

  POST /stt
    body: raw PCM (16 kHz, 16-bit signed LE, MONO, headerless)  -- what the watch sends
          OR a complete RIFF/WAV blob                            -- for `curl --data-binary @x.wav`
    resp: 200 {"text": "<transcript>"}        on success
          200 {"text": ""}                    recognized-but-empty / no speech
          400 {"error": "..."}                bad/empty body
          502 {"error": "azure: ..."}         upstream STT failure
  POST /tts
    body: {"text": "<what to say>", "voice": "<optional override>"}
    resp: 200 raw mono 16 kHz 16-bit-LE PCM  (Content-Length set, fully buffered)
          400 {"error": "..."}                bad/empty text
          502 {"error": "azure: ..."}         upstream TTS failure
  GET /health -> 200 {"ok": true, "region": ..., "voice": ..., "tts_format": ...}

Run:  python3 watch_bridge.py            # binds 0.0.0.0:8090
Env:  BRIDGE_PORT (default 8090). Azure creds via speech-to-cli's own config
      (~/.config/speech-to-cli/config.json or AZURE_SPEECH_KEY/REGION).
"""

import json
import os
import struct
import sys
import tempfile
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

# Import speech-to-cli's own Azure STT logic — do not reinvent Azure.
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import speech  # speech.transcribe(audio_path, key, region)  (Azure short-audio REST)
from state import load_config
import requests  # /tts talks to Azure TTS directly (speech.py is STT-only)

# --- TTS (#read-aloud) ------------------------------------------------------
# The watch plays mono 16 kHz s16le and nothing else, and Azure's DragonHD
# voices emit exactly that on request (verified 2026-07-27), so the bridge does
# NO transcoding: Azure's bytes are the watch's bytes.
TTS_FORMAT = "raw-16khz-16bit-mono-pcm"
TTS_BYTES_PER_SEC = 32000          # 16 kHz * 2 bytes, mono
MAX_TTS_CHARS = 400                # the watch sends <= ~224; this is the backstop
MAX_TTS_SECONDS = 30               # a malformed notify must not hold the amp up forever


def _ssml(text, voice):
    """Wrap text in SSML, XML-escaping it first.

    The text originates from retained MQTT notification payloads, i.e. it is
    attacker-influenced. Escaping happens HERE, on the side with a real XML
    story, rather than on the 186 KB microcontroller.
    """
    safe = (text.replace("&", "&amp;").replace("<", "&lt;")
                .replace(">", "&gt;").replace('"', "&quot;"))
    return (
        '<speak version="1.0" xmlns="http://www.w3.org/2001/10/synthesis" '
        f'xml:lang="en-US"><voice name="{voice}">{safe}</voice></speak>'
    )

PORT = int(os.environ.get("BRIDGE_PORT", "8090"))
MAX_BODY = 4 * 1024 * 1024  # 4 MB cap (~2 min of 16k mono) — Azure short-audio limit is 60 s


def _wav_from_pcm(pcm: bytes) -> bytes:
    """Wrap headerless 16 kHz / 16-bit / mono PCM in a 44-byte WAV container.
    Header layout mirrors speech-to-cli's stt._rest_stt_fallback."""
    return struct.pack(
        "<4sI4s4sIHHIIHH4sI",
        b"RIFF", 36 + len(pcm), b"WAVE", b"fmt ", 16, 1, 1,
        16000, 32000, 2, 16, b"data", len(pcm),
    ) + pcm


class Handler(BaseHTTPRequestHandler):
    def _json(self, code, obj):
        body = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, fmt, *args):  # concise one-line log to stderr
        sys.stderr.write("[watch_bridge] %s - %s\n" % (self.address_string(), fmt % args))

    def do_GET(self):
        if self.path.split("?")[0] != "/health":
            return self._json(404, {"error": "not found"})
        cfg = load_config()
        self._json(200, {
            "ok": bool(cfg.get("key")),
            "region": cfg.get("region"),
            "tts_region": cfg.get("tts_region") or cfg.get("region"),
            "voice": cfg.get("voice"),
            "tts_format": TTS_FORMAT,
        })

    def do_POST(self):
        path = self.path.split("?")[0]
        if path == "/stt":
            return self._do_stt()
        if path == "/tts":
            return self._do_tts()
        return self._json(404, {"error": "not found"})

    def _do_stt(self):
        n = int(self.headers.get("Content-Length") or 0)
        # Support Transfer-Encoding: chunked (what the streaming watch will send).
        if not n and self.headers.get("Transfer-Encoding", "").lower() == "chunked":
            body = self._read_chunked()
        else:
            if n <= 0 or n > MAX_BODY:
                return self._json(400, {"error": f"bad Content-Length {n}"})
            body = self.rfile.read(n)
        if not body or len(body) < 320:  # < ~10 ms of audio => nothing useful
            return self._json(400, {"error": "empty/too-short audio"})

        # Accept a ready WAV (curl --data-binary @x.wav) or raw PCM (the watch).
        wav = body if body[:4] == b"RIFF" else _wav_from_pcm(body)

        cfg = load_config()
        key, region = cfg.get("key"), cfg.get("region")
        if not key:
            return self._json(502, {"error": "azure: no key in speech-to-cli config"})

        tmp = tempfile.NamedTemporaryFile(suffix=".wav", delete=False)
        try:
            tmp.write(wav)
            tmp.flush()
            tmp.close()
            text = speech.transcribe(tmp.name, key, region)  # reuse speech-to-cli
        except Exception as e:  # noqa: BLE001 - surface upstream failure to the caller
            return self._json(502, {"error": f"azure: {e}"})
        finally:
            try:
                os.unlink(tmp.name)
            except OSError:
                pass

        self._json(200, {"text": text or ""})

    def _do_tts(self):
        """text -> speech. Returns raw mono 16 kHz s16le PCM, FULLY buffered.

        Why fully buffered: Azure's own response stream stalls 255-782 ms
        between chunks (measured), while the watch can only bridge a 64 ms gap
        behind a 128 ms queue. Relaying live would underrun on every utterance
        -> the amp cycles -> chopped, popping speech. Synthesizing completely
        and then streaming at LAN line rate moves all that variance onto this
        host. It costs little: Azure's time-to-first-byte is ~1.2 s regardless
        of length, and full synthesis beats realtime past ~2 s of audio.
        """
        n = int(self.headers.get("Content-Length") or 0)
        if n <= 0 or n > 8192:
            return self._json(400, {"error": f"bad Content-Length {n}"})
        try:
            req = json.loads(self.rfile.read(n))
            text = (req.get("text") or "").strip()
        except Exception:  # noqa: BLE001 - malformed body is a client error
            return self._json(400, {"error": "bad json"})
        if not text:
            return self._json(400, {"error": "empty text"})
        text = text[:MAX_TTS_CHARS]

        cfg = load_config()
        # DragonHD voices are region-limited, so speech-to-cli keeps a separate
        # tts_region/tts_key; fall back to the STT pair when unset.
        key = cfg.get("tts_key") or cfg.get("key")
        region = cfg.get("tts_region") or cfg.get("region")
        voice = req.get("voice") or cfg.get("voice")
        if not key:
            return self._json(502, {"error": "azure: no key in speech-to-cli config"})

        url = f"https://{region}.tts.speech.microsoft.com/cognitiveservices/v1"
        headers = {
            "Ocp-Apim-Subscription-Key": key,
            "Content-Type": "application/ssml+xml",
            "X-Microsoft-OutputFormat": TTS_FORMAT,
            "User-Agent": "watch_bridge",
        }
        try:
            r = requests.post(url, headers=headers,
                              data=_ssml(text, voice).encode("utf-8"), timeout=60)
        except Exception as e:  # noqa: BLE001 - surface upstream failure
            return self._json(502, {"error": f"azure: {e}"})
        if r.status_code != 200:
            return self._json(502, {"error": f"azure {r.status_code}: {r.text[:160]}"})

        pcm = r.content
        cap = MAX_TTS_SECONDS * TTS_BYTES_PER_SEC
        if len(pcm) > cap:
            pcm = pcm[:cap]          # cap is even -> never splits a sample
        sys.stderr.write(
            f"[watch_bridge] tts {len(text)} chars -> {len(pcm)} B "
            f"({len(pcm)/TTS_BYTES_PER_SEC:.2f}s) voice={voice}\n")

        self.send_response(200)
        self.send_header("Content-Type", "application/octet-stream")
        self.send_header("Content-Length", str(len(pcm)))
        self.send_header("X-Audio-Format", TTS_FORMAT)
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(pcm)

    def _read_chunked(self) -> bytes:
        out = bytearray()
        while len(out) < MAX_BODY:
            line = self.rfile.readline().strip()
            if not line:
                continue
            size = int(line.split(b";")[0], 16)
            if size == 0:
                self.rfile.readline()  # trailing CRLF
                break
            out += self.rfile.read(size)
            self.rfile.readline()  # CRLF after each chunk
        return bytes(out)


def main():
    cfg = load_config()
    region = cfg.get("region")
    print(f"[watch_bridge] Azure region={region} key={'set' if cfg.get('key') else 'MISSING'}",
          file=sys.stderr)
    srv = ThreadingHTTPServer(("0.0.0.0", PORT), Handler)
    print(f"[watch_bridge] listening on 0.0.0.0:{PORT}  (POST /stt, GET /health)", file=sys.stderr)
    srv.serve_forever()


if __name__ == "__main__":
    main()
