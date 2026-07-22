# Vendor Firmware Analysis — xiaozhi-esp32 on the Waveshare ESP32-C6-Touch-AMOLED-2.06

Reference / feature-roadmap breakdown of the **stock xiaozhi-esp32 firmware** JP
flashed to the watch, written as source material for our own Rust firmware
(`esp32c6-watch`). Everything here is traced from the actual vendor source, not
the README or general knowledge.

- **Vendor tree analysed** (shallow clone, read-only): `scratch/vendor-xiaozhi/` (GitHub `78/xiaozhi-esp32`)
- **Target board** (the one JP has): `main/boards/waveshare/esp32-c6-touch-amoled-2.06/`
- **Our firmware** (comparison): `src/` (this repo)
- File:line citations are relative to the vendor tree unless prefixed `src/` (ours). Vendor paths omit the `scratch/vendor-xiaozhi/` prefix.

> **The single most important finding is in [§10.3](#103-the-1-suspect-axp2101-aldo1-mic-power-rail): the vendor powers the microphone via the AXP2101 `ALDO1` rail (`0x90=0x03`, `0x92`=3.3 V). Our firmware never enables it. If our ES7210 init NACKs or the mic still reads silence after the driver port, this rail is the prime suspect.**

---

## 1. Executive summary

xiaozhi-esp32 is a **cloud voice-assistant** firmware ("An MCP-based Chatbot",
`README.md:1`). It is *not* a watch OS — on our exact board it is display +
audio + PMIC + one button, nothing else. It:

1. Boots → provisions Wi-Fi (SoftAP captive portal) → activates against a cloud
   server → idles showing a face.
2. Listens for an **on-device wake word** ("你好小智" / *Ni Hao Xiao Zhi*,
   `sdkconfig.defaults.esp32c6:5`) using ESP-SR WakeNet (runs fine on the C6, no
   PSRAM needed — see §4).
3. On wake/button, opens an audio channel to the backend, streams **Opus 16 kHz**
   mic audio up, receives **STT text + LLM emotion + TTS Opus audio** back, and
   **speaks the reply through the ES8311 speaker** while animating an emoji face.
4. Exposes **on-device MCP tools** so the cloud LLM can control the device
   (volume, brightness, theme, reboot, reconfigure-wifi, …).
5. Supports **OTA firmware update** and cloud-pushed **asset packs** (fonts,
   emojis, wake words).

The big capabilities we **lack** are the whole voice round-trip past STT:
**on-device wake word, TTS playback (it talks back), LLM conversation, and the
emoji-expression UI**. The big capabilities the vendor firmware **lacks vs ours**:
touch UI, IMU, dedicated RTC, watch faces, games, BLE, ESP-NOW mesh, HA/MQTT —
the vendor board file initializes **none** of those (§6).

---

## 2. Feature / app / mode enumeration

xiaozhi has no "apps"; it is one state machine (§3) with these user-facing modes
and background features. Official list: `README.md:24-37`.

| Feature / mode | What it does | Where |
|---|---|---|
| **Wake-word listen** | Always-on WakeNet detection while idle; wake → connect → listen | `application.cc:811` (`HandleWakeWordDetectedEvent`), §4 |
| **Push-to-talk / toggle chat** | BOOT button toggles a conversation turn | board `esp32-c6-touch-amoled-2.06.cc:192-201`; `application.cc:706` |
| **Voice conversation** | Full STT→LLM→TTS round trip over WS or MQTT+UDP | `application.cc:543-650`, §5 |
| **Listening modes** | `kListeningModeAutoStop` (server VAD ends turn), `kListeningModeManualStop`, `kListeningModeRealtime` (full-duplex, needs AEC) | `application.cc:1017-1024` |
| **Speaking (TTS playback)** | Decodes server Opus, plays through ES8311, shows assistant text | `application.cc:550-585`, §4 |
| **Emoji / expression face** | Cloud `llm.emotion` string → on-screen face | `application.cc:601-607`, §7 |
| **Alerts / notifications** | Status + emotion + message + sound | `application.cc:679-698` |
| **On-device MCP tool server** | LLM-driven device control (volume, brightness, theme, reboot, …) | `mcp_server.cc`, §8 |
| **Wi-Fi provisioning** | SoftAP + captive portal (`Xiaozhi-XXXX`), also BluFi / acoustic | `boards/common/wifi_board.cc:168-206`, §9 |
| **Audio test mode** | In Wi-Fi-config state, BOOT records ~10 s then plays it back (loopback mic check) | `application.cc:712-719`, `audio_service.cc:274-296,679-693` |
| **Activation** | Shows a numeric code, speaks the digits, HMAC challenge to bind device to account | `application.cc:655-677`, `ota.cc:421-456`, §5/§9 |
| **OTA firmware update** | Version check + download + `esp_ota` apply + reboot | `ota.cc`, §9 |
| **Asset OTA** | Cloud-pushed pack of fonts/emoji/wake-words to a flash partition | `application.cc:357-415` |
| **Power save / sleep** | Dim → sleep → PMIC power-off timer (60 s / 300 s) | board `.cc:148-159` |
| **Battery telemetry** | AXP2101 level / charging state → status bar | board `.cc:305-317`, §6 |
| **Multi-language** | 38 UI languages, localized voice prompts (`assets/locales/*`) | `assets/locales/`, `Kconfig.projbuild` |

Modes NOT built for our board (present in tree for other boards): camera vision,
LED strips, servos, cellular/4G, wired Ethernet, speaker recognition.

---

## 3. Architecture

### 3.1 Entry & singletons
`app_main` (`main.cc:16`) inits NVS then `Application::GetInstance().Initialize()`
+ `.Run()`. `Application` is a singleton (`application.h:44-49`); the current
`Board` is a `DECLARE_BOARD`-registered singleton (board `.cc:327`).

### 3.2 The main task = one FreeRTOS event loop
`Application::Run()` (`application.cc:168-273`) sets its own priority to 10 and
blocks on an event group (`xEventGroupWaitBits`, `application.cc:180`). All
cross-task work is funnelled here via **event bits** (`application.h:22-35`):
`SCHEDULE, SEND_AUDIO, WAKE_WORD_DETECTED, VAD_CHANGE, ERROR, ACTIVATION_DONE,
CLOCK_TICK, NETWORK_CONNECTED/DISCONNECTED, TOGGLE_CHAT, START/STOP_LISTENING,
STATE_CHANGED, PLAYBACK_DRAINED`. `Schedule(fn)` (`application.cc:1001`) queues a
closure onto `main_tasks_` and sets `SCHEDULE` — this is the universal
"run-on-main-task" thread-safety primitive (used everywhere to marshal work off
audio/network/timer tasks). A 1 Hz `esp_timer` posts `CLOCK_TICK` to refresh the
status bar and print heap stats every 10 s.

### 3.3 Device state machine
`DeviceStateMachine` (`device_state_machine.cc`) is a validated FSM with an
atomic current state + listener callbacks. States (`device_state.h`):
`Unknown → Starting → WifiConfiguring → Activating → (Upgrading) → Idle ⇄
Connecting ⇄ Listening ⇄ Speaking`, plus `AudioTesting` and `FatalError`.
Transition table: `device_state_machine.cc:34-102`. `OnStateChanged`
(`application.cc:906-969`) is where each state wires the audio engine + display:

- **Idle** → status "standby", emotion `neutral`, **voice processing off, wake-word on**.
- **Connecting** → open audio channel.
- **Listening** → enable voice processing (auto mode defers until playback drains, `application.cc:937-946`), wake-word off (unless `WAKE_WORD_DETECTION_IN_LISTENING`).
- **Speaking** → voice processing off, wake-word only if AFE-capable, reset decoder.

### 3.4 Task inventory (what runs concurrently)
| Task | Prio | Stack | Role | Source |
|---|---|---|---|---|
| `main` (Run loop) | 10 | — | event loop, state, UI, send-audio pump | `application.cc:168` |
| `audio_input` | 8 | 4–6 KB | read codec → resample 16 k → feed engine | `audio_service.cc:131-157,236` |
| `audio_output` | 4 | 2–4 KB | pop playback queue → write codec | `audio_service.cc:314` |
| `opus_codec` | 2 | 24 KB | Opus encode (mic) + decode (TTS) | `audio_service.cc:160-164,363` |
| `audio_afe` / (none on C6) | 3 | 4 KB | AFE fetch loop (S3/P4 only) | `afe_audio_engine.cc:173` |
| `activation` | 2 | 8 KB | one-shot: assets+version check, protocol init | `application.cc:287-294` |
| `clock_timer`, `audio_power_timer` | esp_timer | — | 1 Hz UI tick; 15 s idle codec power-down | `application.cc:35`, `audio_service.cc:109` |

Network/protocol callbacks run on the transport's own task and `Schedule()` back
to main.

### 3.5 Key abstractions (base classes to mirror)
- `Board` → `WifiBoard` → concrete board; provides `GetAudioCodec/GetDisplay/GetBacklight/GetBatteryLevel/GetLed`, network lifecycle.
- `AudioCodec` (abstract) → `BoxAudioCodec` (our board), `Es8311AudioCodec`, etc.
- `AudioEngine` (abstract, `audio_engine.h`) → `AfeAudioEngine` (S3/P4), **`LiteAudioEngine` (C6, ours)**.
- `WakeWord` (abstract, `wake_word.h`) → `EspWakeWord` (standalone WakeNet, ours), `CustomWakeWord` (MultiNet).
- `Protocol` (abstract) → `WebsocketProtocol`, `MqttProtocol`.
- `Display` → `LcdDisplay`/`SpiLcdDisplay` (ours), `OledDisplay`, `EmoteDisplay`.
- `McpServer` singleton (tool registry).

---

## 4. Audio pipeline (the part that matters most to us)

### 4.1 Two data flows (`audio_service.h:28-37`)
```
MIC  → [AudioEngine] → {encode queue} → [Opus enc] → {send queue}  → (server)
server → {decode queue} → [Opus dec]  → {playback queue} → SPEAKER
```
Dedicated tasks for input, output, and a shared Opus codec task; queues decouple
them. Backpressure = **drop oldest** (realtime audio is worthless stale):
`audio_service.cc:473-475` (send), `:560-564` (encode).

### 4.2 Codecs, rates, Opus
- **Opus** both directions. Encoder is **fixed 16 kHz mono, 60 ms frames, VBR, DTX on, complexity 0** (`audio_service.h:65-76`, `AS_OPUS_ENC_CONFIG`). Decoder rate = server rate (default 24 kHz, `protocol.h:77`), resampled to the codec's output rate (`audio_service.cc:500-534`).
- Our board's codec input/output are both **24 kHz** (board `config.h`), so the input path **resamples 24 k → 16 k** before Opus (`audio_service.cc:79-86,197-213`). Feed to the engine is chunked to **160 samples = 10 ms** (`audio_service.cc:299`).
- `esp_audio_codec` / `esp_opus_enc|dec` + `esp_ae_rate_cvt` resamplers (managed components).

### 4.3 Wake word — **runs on the C6, no PSRAM required**
Engine selection is compile-time by target (`audio_service.cc:25-29,88-92`):
**S3/P4 → `AfeAudioEngine`; everything else (incl. C6) → `LiteAudioEngine`.**

`LiteAudioEngine` (`lite_audio_engine.cc`):
- Wake word via **`EspWakeWord`** = raw ESP-SR **WakeNet** through `esp_wn_iface` at **`DET_MODE_95`** (`esp_wake_word.cc:47`). Feeds mono (extracts channel 0 from multi-channel, `esp_wake_word.cc:86-92`), detect loop `:94-108`.
- **No AFE, no AEC, no NS, no AGC, no on-device VAD** (`lite_audio_engine.cc:73-77` explicitly logs "Device AEC is not supported"). Voice processing is a **raw passthrough** — mic PCM straight to the encoder (`OutputRawAudio`, `:124-150`).
- Model on the C6: **`CONFIG_SR_WN_WN9S_NIHAOXIAOZHI`** — the WakeNet9 small "你好小智" model (`sdkconfig.defaults.esp32c6:5`). Config enables it via `CONFIG_USE_ESP_WAKE_WORD=y` (board `config.json`).

`AfeAudioEngine` (`afe_audio_engine.cc`, **S3/P4 only**) is the full ESP-SR AFE:
WakeNet **or** MultiNet custom commands, VAD, AEC (`AEC_MODE_VOIP_HIGH_PERF`,
`:136`), all allocated in **PSRAM** (`AFE_MEMORY_ALLOC_MORE_PSRAM`, `:150`;
wake-word encode stack in `MALLOC_CAP_SPIRAM`, `:483`). **This is why the C6
cannot run AFE/AEC/on-device-VAD — no PSRAM.**

Consequence for us: **on-device wake word is portable to the C6** (ESP-SR
WakeNet9s fits in internal RAM + flash), but **AEC and on-device VAD are not** —
those need the S3/P4 + PSRAM. Auto-stop end-of-turn on the C6 is **server-side
VAD**, not device VAD.

### 4.4 Idle power management
`audio_power_timer` (1 Hz) powers the codec input/output down after 15 s idle
(`audio_service.cc:778-796`). Note the **duplex guard** at `:788-791`: it will
**not** power output down while duplex RX is active, "otherwise RX may stall on
some boards" — the same shared-clock hazard our firmware fights with the
continuous silent TX.

### 4.5 The microphone codec (ES7210) — vendor vs ours
Our board's `GetAudioCodec()` returns a **`BoxAudioCodec`** (board `.cc:280-295`)
— the dual-chip codec that drives **ES8311 (speaker DAC) + ES7210 (4-ch mic
ADC)**. This confirms our own finding: **the mics are on the ES7210, not the
ES8311.**

**Vendor ES7210 setup** (`box_audio_codec.cc`):
- I2C ctrl at `AUDIO_CODEC_ES7210_ADDR` (default 0x40 7-bit), `es7210_codec_new` from **`esp_codec_dev`** (managed component — the actual register sequence is *not* in the repo; it lives in `espressif/esp_codec_dev ~1.5.6`). `:66-74`.
- **All four mics selected**: `ES7210_SEL_MIC1|MIC2|MIC3|MIC4` (`:72`).
- I2S RX is **TDM 4-slot** (`I2S_TDM_SLOT0..3`), 16-bit, `bclk_div=8`, MCLK=256·fs (`:145-179`). Input opened as **4 channels, mask channel 0** (`:199-203`), i.e. it captures 4 TDM slots but consumes only physical MIC1.
- SoC I2S is **master** (`I2S_ROLE_MASTER`, `:103`); ES7210 is the I2S **slave**.
- `input_gain_` default and per-channel gain via `esp_codec_dev_set_in_channel_gain` (`:210-218`).
- **Input rate 24 kHz** (board config), later resampled to 16 kHz for Opus.

**Our ES7210 setup** (`src/peripherals/es7210.rs`, mic-fix branch):
- Hand-ported register sequence from esp-adf `es7210.c` (`src/peripherals/es7210.rs:10-17`).
- **2 mics only, standard I2S stereo** (MIC1=Left, MIC2=Right), non-TDM (`reg 0x12=0x00`), 16-bit, `reg 0x11=0x60` serial format (`es7210.rs:71-75`).
- ES7210 as **I2S slave** (`reg 0x08=0x10`, `es7210.rs:59`); our SoC TX is master via `signal_loopback=true` and RX slaves to it (`src/main.rs:532-585`).
- **16 kHz directly** (MCLK 256·16 k = 4.096 MHz) — no resample step.
- Gain **+36 dB** (`reg 0x43/0x44 = 0x1D`), reasserted last (`es7210.rs:97-98`) — the driver documents the esp-adf footgun that `es7210_start` re-zeros the gain nibble.

**Divergences that matter:**
1. **Serial format**: vendor = TDM 4-slot @ 24 kHz; ours = standard-I2S 2-slot @ 16 kHz. Both are valid *provided the SoC RX slot config matches the ES7210's `reg 0x11` format* — ours does (standard-I2S RX). No functional problem, and ours is simpler.
2. **Mic count**: vendor enables 4; we enable 2. The board likely has ≤2 physical mics; consuming MIC1/MIC2 is fine.
3. **Power rail**: see §10.3 — the vendor's board file powers the ES7210 via **AXP2101 ALDO1**; ours does not. **This is the one that can silently break capture.**

### 4.6 TTS playback (round-trip speech — we don't have this)
Server sends `tts state=start` → device goes **Speaking** (`application.cc:555-559`),
incoming Opus packets are pushed to the decode queue **only while Speaking**
(`application.cc:518-522`), decoded (`OpusCodecTask`, `audio_service.cc:376-440`),
resampled to codec rate, and written to the ES8311 by `AudioOutputTask`
(`:314-361`). `tts state=stop` returns to Listening (auto) or Idle (manual)
(`application.cc:560-569`). Assistant text arrives on `tts sentence_start.text`
(`:570-585`). Local sound effects (activation digits, popup, error beeps) are
**Ogg/Opus** assets decoded through `OggDemuxer` + the same playback path
(`audio_service.cc:710-731`).

---

## 5. Voice / AI protocol (backend comms)

*(Full trace in `scratch/vendor-analysis/proto-scout.md`.)*

### 5.1 Transport is server-chosen, not compile-time
After the OTA/activation response, `InitializeProtocol` picks the transport from
what the server wrote to NVS: **MQTT if an `mqtt{}` block was returned, else
WebSocket, else MQTT default** (`application.cc:502-509`). Self-host vs
xiaozhi.me is entirely "which OTA URL you point at + what it returns".

### 5.2 Two transports
| | WebSocket (`websocket_protocol.cc`) | MQTT + UDP (`mqtt_protocol.cc`) |
|---|---|---|
| Open | lazy on `OpenAudioChannel()` (`:79`), `Connect(url)` (`:169`) | `Start`→`StartMqttClient` (`:69`), TLS broker :8883 (`:150`) |
| Config (NVS) | `websocket`: `url,token,version` | `mqtt`: `endpoint,client_id,username,password,keepalive=240,publish_topic` |
| Auth | `Authorization: Bearer <token>`, `Protocol-Version`, `Device-Id`=MAC, `Client-Id`=UUID (`:97-106`) | MQTT username/password (`:158`) |
| Control msgs | JSON **text frames** (`:141-152`) | JSON via MQTT publish (`:168-178`) |
| Audio | **binary frames**, same socket | **separate UDP socket, AES-128-CTR** (`:180-212,267-334`) |
| Handshake | `hello` → wait server hello (10 s) | `hello` over MQTT → server returns `udp{server,port,key,nonce}` (`:403-416`) |

### 5.3 Control messages (JSON `type`)
**Outgoing** (`protocol.cc`): `hello`; `listen` with state `start`(mode
realtime\|auto\|manual)/`stop`/`detect`(wake word) (`:67-92`); `abort`
(`:58-65`); `mcp` (`:94-98`); `goodbye` (MQTT, `mqtt_protocol.cc:227`).
**Incoming** (`application.cc:543-649`):

| type | fields | effect |
|---|---|---|
| `tts` | `state`=start/stop/**sentence_start**, `text` | speaking state; **assistant text** (`:550-585`) |
| `stt` | `text` | **user transcript** (`:586-600`) |
| `llm` | `emotion` | **emoji face** → `SetEmotion` (`:601-607`) |
| `mcp` | `payload` (JSON-RPC 2.0) | device tool call (`:608-612`) |
| `system` | `command` (`reboot`) | (`:613-623`) |
| `alert` | `status,message,emotion` | (`:624-633`) |
| `custom` | `payload` | gated `CONFIG_RECEIVE_CUSTOM_MESSAGE` (`:635-646`) |

STT text, assistant text, and emotion are **three separate JSON messages**, not
embedded in audio.

### 5.4 Audio on the wire
Opus, 16 kHz mono up / server-rate down, 60 ms frames. WebSocket framing by
negotiated version: v1 raw Opus; v2/v3 have small binary headers with
timestamp/type (`protocol.h:17-31`). MQTT/UDP datagram
(`mqtt_protocol.cc:268-272`): `|type|flags|len(2)|ssrc(4)|timestamp(4)|seq(4)|
payload|` where the **16-byte header doubles as the AES-CTR nonce**; monotonic
`seq` replay guard (`:299-328`).

### 5.5 Default endpoints
OTA `https://api.tenclass.net/xiaozhi/ota/` (`ota.cc:46-53`); MQTT broker :8883;
audio 16 k/mono/60 ms; server playback 24 k.

---

## 6. Board specifics — Waveshare ESP32-C6-Touch-AMOLED-2.06

Sources: board `config.h`, `config.json`, `esp32-c6-touch-amoled-2.06.cc`.

### 6.1 GPIO map (authoritative)
| Signal | GPIO | Notes |
|---|---|---|
| I2S MCLK | 19 | 256·fs |
| I2S BCLK | 20 | |
| I2S WS/LRCK | 22 | |
| I2S DIN (codec→SoC, **mic**) | **21** | ES7210 SDOUT1 → I2S_ASDOUT |
| I2S DOUT (SoC→codec, **speaker**) | 23 | ES8311 DSDIN |
| Speaker PA enable | 6 | keep low unless playing |
| Codec I2C SDA / SCL | 8 / 7 | shared bus: ES8311 (0x18), ES7210 (0x40), AXP2101 (0x34) |
| BOOT button | 9 | strapping pin, the *only* input |
| LCD CS / PCLK | 5 / 0 | QSPI |
| LCD D0–D3 | 1 / 2 / 3 / 4 | QSPI quad data |
| LCD RST | 11 | |
| LCD backlight | **N/C** | brightness is a **panel command 0x51**, not a GPIO/PWM (board `.cc:120-137`) |

Matches our `src/board.rs` pin table (GPIO19/20/22/21/23, PA=6, I2C 8/7).

### 6.2 Display
- **SH8601 AMOLED, 410×502, QSPI (SPI2, quad, 40 MHz), RGB565/16-bpp** (board `.cc:213-255`). Column gap 0x16 (22 px), even-alignment flush rounder required (`.cc:78-92`). Init table `.cc:60-73`. **Not** CO5300 — note our repo has a `co5300.rs` driver; the vendor uses SH8601 (`esp_lcd_sh8601`) for this board. Worth reconciling which controller our panel actually is.
- Backlight = LCD command 0x51 via `CustomBacklight` (board `.cc:120-137`).

### 6.3 PMIC — AXP2101 (I2C 0x34)
Vendor `Pmic` ctor (board `.cc:26-53`) does a **full rail config**:
- Power-off source + 4 s hold (`0x22=0b110`, `0x27=0x10`).
- **Disable all DC except DC1** (`0x80=0x01`); **disable all LDOs** (`0x90=0,0x91=0`), then **DC1=3.3 V** (`0x82`), **ALDO1=3.3 V** (`0x92`), **ALDO2/… =3.3 V** (`0x93`), then **enable ALDO1 (MIC)** (`0x90=0x03`).
- Charger: CV 4.1 V (`0x64=0x02`), precharge 50 mA (`0x61`), charge 400 mA (`0x62=0x0A`), term 25 mA (`0x63`).

Our `src/peripherals/power.rs` **deliberately does none of this** — it only
writes the battery-ADC-enable reg (0x30) and reads telemetry, with an explicit
comment that touching rails risks a panel brown-out (`power.rs:1-6`). **The
missing ALDO1 enable is the top mic suspect (§10.3).** The charger settings are a
secondary roadmap item (our board charges on Waveshare's defaults today).

### 6.4 Hardware the vendor firmware **ignores** on this board
The board `.cc` constructor only inits: power-save timer, codec I2C, AXP2101,
SPI, SH8601 display, BOOT button, MCP tools (`.cc:270-278`). It **does not
initialize**:
- **Touch** — no touch controller driver anywhere in the board file. The vendor UX on this board is **button-only**. (Our firmware uses the touchscreen — see `src/peripherals/touch.rs`.)
- **IMU** (the "six-axis sensor" in the README) — never read.
- **RTC chip** — none; the clock comes from **server time** (`ota.cc` `server_time` → `settimeofday`), and the status bar clock only shows once year ≥ 2025.

So the stock firmware is a *minimal voice puck* on hardware we drive as a full
watch. This asymmetry defines the roadmap (§10).

---

## 7. UI / display / emoji-expression system

*(Full trace in `scratch/vendor-analysis/ui-scout.md`.)*

### 7.1 Framework
LVGL v9 + `esp_lvgl_port`; `DisplayLockGuard` RAII around the port lock
(`lcd_display.cc:351-353`). Our board runs the **LVGL widget path**
(`CustomLcdDisplay : SpiLcdDisplay`), **default message style** (not WeChat
bubbles), partial 20-line single draw buffer, no PSRAM image cache
(`lcd_display.cc:99-181`).

### 7.2 Screen layout
One screen: centered **emoji box** (`emoji_label_`/`emoji_image_`), a **top/status
bar**, a bottom single-line scrolling **chat subtitle**, and a low-battery popup
(`lcd_display.cc:822-1016`). Status indicators, all driven from
`UpdateStatusBar` at 1 Hz (`lvgl_display.cc:191-301`): **mute** (codec vol==0),
**clock** (`%H:%M`, idle only, needs RTC year ≥2025), **battery** (8-level ramp /
charging bolt), **network** (`board.GetNetworkStateIcon()`, every 10 s).

### 7.3 The emoji / expression system (a KEY thing to replicate)
Cloud sends `llm.emotion` (a string) → `SetEmotion(str)` (`application.cc:601-607`).
`SetEmotion` resolves in order (`lcd_display.cc:1100-1186`): theme
`EmojiCollection` image/GIF → **noto_emoji color-font glyph** → material-symbols
glyph. **On our board, no `EmojiCollection` is registered, so emotions render as
noto_emoji color-font glyphs (static), not animated GIFs.** Animated GIF faces
(the `LvglGif` controller, `gif/lvgl_gif.cc`) are opt-in per board (Otto pattern).

**Canonical 21-emotion vocabulary** the backend can send:
`neutral, happy, laughing, funny, sad, angry, crying, loving, embarrassed,
surprised, shocked, thinking, winking, cool, relaxed, delicious, kissy,
confident, sleepy, silly, confused` (19 confirmed in-tree; `cool`/`kissy` live in
the external `78/xiaozhi-fonts` component). Plus firmware-internal strings:
`robot_2` (boot), `neutral`/`sleepy` (idle/power-save), `warning`, `link`
(activation). The emotion→glyph table itself ships in the **`78/xiaozhi-fonts`
noto_emoji** asset (fetched at build; not vendored).

### 7.4 Theming / fonts
Light + dark themes with fixed palettes + 4 fonts each (text / icon /
material_symbols_30_4 / noto_emoji_30_4), persisted to NVS `display.theme`
(`lcd_display.cc:27-82`). A **DynamicGlyphCache** builds a runtime font from
server-pushed glyph bitmaps for CJK/rare chars (`dynamic_glyph_cache.cc`).

### 7.5 Display API to mirror (`display.h`)
`SetStatus(const char*)` `:37`; `ShowNotification(const char*, int ms=3000)` `:38`;
`SetEmotion(const char*)` `:40`; `SetChatMessage(role, content)` `:41`
(role ∈ user/assistant/system); `ClearChatMessages()` `:42`; `SetTheme` `:43`;
`UpdateStatusBar(bool all=false)` `:45`; `SetPowerSaveMode(bool)` `:46`.

---

## 8. IoT / MCP device-control ("things")

*(Full trace in `scratch/vendor-analysis/mcp-scout.md`.)*

### 8.1 Design
MCP is **JSON-RPC 2.0 riding the same protocol channel** as everything else
(inbound `type:"mcp"` → `McpServer::ParseMessage`, `application.cc:608-612`;
outbound via `SendMcpMessage`, `:1141`). Both transports advertise
`features.mcp:true` in the hello. The **legacy IoT/"Thing" protocol is gone** —
MCP tools fully replaced it (no `iot/` dir, no `ThingManager`). `McpServer`
(singleton) holds a `vector<McpTool*>`; `AddTool(name, desc, PropertyList, cb)`.
`Property` is `variant<bool,int,string>` with optional int min/max → generates an
`inputSchema`. `tools/list` is cursor-paginated (8 KB budget); `tools/call`
coerces args and runs the callback **on the main task** via `Schedule`, returning
`{content:[{type:text|image}]}` (`mcp_server.cc:350-560`).

### 8.2 Built-in tools (AI-visible)
| tool | params | effect |
|---|---|---|
| `self.get_device_status` | — | speaker vol, brightness, theme, battery, network, chip temp (`mcp_server.cc:45`) |
| `self.audio_speaker.set_volume` | `volume` 0–100 | `codec->SetOutputVolume` (`:55`) |
| `self.screen.set_brightness` | `brightness` 0–100 | `backlight->SetBrightness` (`:66`) |
| `self.screen.set_theme` | `theme` light/dark | `display->SetTheme` (`:80`) |
| `self.system.reconfigure_wifi` | — | enter Wi-Fi config mode (**added by our board**, board `.cc:260`) |

User-only tools (hidden from AI, `withUserTools:true`): `self.get_system_info`,
`self.reboot`, `self.upgrade_firmware(url)`, `self.screen.get_info`,
`self.screen.snapshot`, `self.screen.preview_image(url)`,
`self.assets.set_download_url(url)` (`mcp_server.cc:128-298`).

This is the vendor's answer to "let the LLM control the watch". Our HA/MQTT layer
(`src/net/mqtt_ha.rs`, `mqtt_climate.rs`) is a *different* control surface (Home
Assistant, not an LLM tool protocol) — see §10.

---

## 9. Provisioning, OTA, settings, power

### 9.1 Wi-Fi provisioning (`boards/common/wifi_board.cc`, `78/esp-wifi-connect`)
Boot with stored creds → `StartStation` (60 s timeout). No creds → after 1.5 s →
`StartWifiConfigMode` → **SoftAP + captive portal** (`Xiaozhi-XXXX`, web URL from
`GetApWebUrl()`), alternatives BluFi / acoustic (Kconfig). Creds stored by
esp-wifi-connect's own `SsidManager` NVS namespace. `reconfigure_wifi` MCP tool →
`EnterWifiConfigMode` (`wifi_board.cc:208-247`). (`mcp-scout.md` has the full
step list.)

### 9.2 OTA + activation (`ota.cc`)
1. POST system-info JSON to OTA URL (NVS `wifi.ota_url` else `CONFIG_OTA_URL`), headers `Device-Id`(MAC)/`Client-Id`(UUID)/`Serial-Number`?/`Activation-Version` (`:55-97`).
2. Response → writes `mqtt{}`/`websocket{}` transport blocks to NVS, `server_time`→clock, `activation{code,challenge}`, `firmware{version,url,force}` semver-compared (`:124-241`).
3. New firmware → stream download → `esp_ota_begin/write/end` → set boot partition → reboot (`:267-387`).
4. **Activation**: POST `<ota_url>activate` with `{algorithm:"hmac-sha256", serial_number, challenge, hmac}` where `hmac`=HMAC-SHA256(challenge) using efuse **HMAC_KEY0** (`:421-456`); 202=pending (poll), 200=activated. Boot loop shows the code and speaks the digits (`application.cc:466-491,655-677`).

### 9.3 Settings (NVS namespaces, `settings.cc`)
`board`(uuid) · `wifi`(ota_url…) · `websocket`(url,token,version) ·
`mqtt`(endpoint,client_id,username,password,keepalive,publish_topic) ·
`audio`(output_volume) · `display`(theme) · `assets`(download_url). Wi-Fi creds
live in esp-wifi-connect's own namespace.

### 9.4 Power management
Board `PowerSaveTimer(-1, 60, 300)` (board `.cc:148-159`): 60 s → dim + power-save
display; 300 s → **PMIC power-off**. Only armed while discharging
(`.cc:305-316`). App-level `SetPowerSaveLevel(PERFORMANCE|LOW_POWER)` toggled
around audio channel open/close. Idle codec power-down after 15 s (§4.4).

---

## 10. Roadmap comparison — xiaozhi vs our Rust firmware

### 10.1 Capability matrix
Legend: ✅ have · ⚠️ partial · ❌ lack · — n/a.

| Capability | xiaozhi | ours (`esp32c6-watch`) | Notes / source |
|---|---|---|---|
| **On-device wake word** | ✅ WakeNet9s "你好小智" | ❌ | Portable to C6 (no PSRAM needed), §4.3 |
| **Mic capture (ES7210)** | ✅ TDM 4-ch @24 k | ⚠️ driver ported (mic-fix), unverified on glass | §4.5, §10.3 |
| **STT** | ✅ cloud (server ASR) | ✅ LAN bridge → Azure (`src/net/voice_stt.rs`) | ours is STT→text only |
| **LLM conversation** | ✅ cloud | ❌ | needs backend |
| **TTS playback (talks back)** | ✅ Opus→ES8311 | ❌ (we only show text) | §4.6 |
| **AEC (echo cancel)** | ⚠️ S3/P4+PSRAM only | ❌ | not feasible on C6 (no PSRAM), §4.3 |
| **On-device VAD** | ⚠️ AFE only (S3/P4) | ❌ | C6 relies on server VAD |
| **Emoji-expression UI** | ✅ 21 emotions (static glyphs on C6) | ❌ | §7.3; very portable |
| **Speaker (ES8311) playback** | ✅ | ✅ (beeps; `src/peripherals/audio.rs`) | we have the DAC path |
| **Opus codec** | ✅ | ❌ (raw PCM to LAN bridge) | ours streams headerless PCM |
| **Display** | ✅ SH8601 LVGL | ✅ SH8601/CO5300 + Slint | different UI stack |
| **Touch UI** | ❌ (button only) | ✅ | ours is richer here |
| **IMU / RTC** | ❌ (ignored) | ✅ | ours richer |
| **Watch faces / games** | ❌ | ✅ | ours richer |
| **BLE / ESP-NOW mesh** | ❌ | ✅ (`ble.rs`, `smol_mesh.rs`) | ours richer |
| **HA / MQTT integration** | ❌ (has LLM-MCP instead) | ✅ (`mqtt_ha.rs`, `mqtt_climate.rs`) | different control model |
| **Cloud MCP device control** | ✅ | ❌ | LLM tool protocol |
| **OTA firmware** | ✅ (esp_ota) | ✅ (`ota_http.rs`) | both have it |
| **Wi-Fi provisioning portal** | ✅ SoftAP captive | ⚠️ (config in `config.rs`) | check ours |
| **PMIC full rail config** | ✅ | ❌ (telemetry only) | §6.3, §10.3 |

### 10.2 The deltas worth porting (with C6-specific feasibility)

**Hard constraints that bite every audio feature on our board:**
- **Single radio (Wi-Fi XOR ESP-NOW mesh)** — the voice round-trip needs Wi-Fi up for the whole turn; it cannot coexist with an active mesh session. Any voice mode must arbitrate the radio.
- **No PSRAM** — kills AFE, AEC, on-device VAD, and LVGL image caching. Wake word (WakeNet9s) still fits in internal SRAM+flash, but it competes for RAM with our Slint UI + Wi-Fi + mesh.
- **Tight RAM / stack** — see the repo memory notes on the stack-floor guardrail (heap trimmed to grow the stack after the `ppRecycleRxPkt` crash). ESP-SR + an Opus codec + audio queues are a real RAM ask on top of that. Budget carefully before committing.
- **esp-hal, no ESP-IDF** — ESP-SR and `esp_codec_dev`/`esp_opus_*` are ESP-IDF C components. Using them from our esp-hal/no_std build means either FFI into those static libs or a Rust reimplementation. Opus has Rust crates; **WakeNet has no Rust equivalent** (proprietary ESP-SR blob) — porting on-device wake word realistically means linking the ESP-SR static lib.

| Delta | Effort | Feasibility on C6 | Recommendation |
|---|---|---|---|
| **Emoji-expression UI** | **Low** | ✅ Full | Cheapest, highest-visual-payoff port. Map a 21-string emotion enum → Slint images/font glyphs; static faces first (matches what the C6 vendor path actually shows). Pure UI, no radio/RAM risk. Do this first. |
| **TTS playback (speak replies)** | Medium | ✅ | We already have the ES8311 DAC + I2S TX. Need an audio decoder: cheapest is to have the **LAN bridge return raw 16 k PCM** (reuse `voice_stt` pattern) and stream it to the existing TX DMA — **no Opus on-device required**. Opus decode is the "correct" but heavier path. |
| **LLM conversation** | Medium (mostly backend) | ✅ | Device side is small once STT+TTS exist: POST transcript to an LLM endpoint on the bridge, get text+emotion+audio back. Keep the secrets on the bridge (same trust model as `voice_stt`). |
| **On-device wake word** | **High** | ⚠️ | Needs the ESP-SR WakeNet static lib linked into an esp-hal build (no Rust port exists) + ~internal-RAM budget. Big integration lift and RAM pressure alongside Slint+Wi-Fi. Consider a **button-to-talk** MVP (we already have that flow) before investing here. |
| **AEC / on-device VAD** | — | ❌ | Not feasible on C6 (PSRAM). Use **server/bridge VAD** for end-of-turn, and half-duplex (mute mic during playback) to avoid echo. |
| **Opus on the wire** | Medium | ✅ | Only worth it for cloud-server compatibility / bandwidth. For a LAN bridge, raw PCM is simpler and already works. |
| **Cloud MCP tool server** | Medium | ✅ | Only relevant if we adopt an LLM backend and want it to control the watch. Our HA/MQTT layer already covers home control differently. |
| **AXP2101 full rail config** | Low–Medium | ✅ (careful) | Needed for the mic rail (§10.3) and better charge control. Port **incrementally** — enable ALDO1 first, validate no brown-out, then charger settings. Do **not** blind-copy the vendor's "disable all DC/LDO" block (our `power.rs` warns it can brown out the panel). |

**Suggested ordering:** emoji UI → TTS playback via bridge (raw PCM) → LLM turn
via bridge → (optional) on-device wake word. Each stage is demoable and reuses
the `voice_stt` LAN-bridge trust model.

### 10.3 The #1 suspect: AXP2101 ALDO1 mic-power rail
The vendor board file **explicitly powers the microphone** by enabling AXP2101
**ALDO1** at 3.3 V (`esp32-c6-touch-amoled-2.06.cc:41-46`: `WriteReg(0x92, …)`
sets ALDO1=3.3 V; `WriteReg(0x90, 0x03)` enables it — comment *"Enable
ALDO1(MIC)"*).

Our firmware **never enables ALDO1**: `src/peripherals/power.rs` only writes the
battery-ADC reg (0x30) and explicitly avoids all rail writes
(`power.rs:1-6`); a grep of the mic-fix worktree `main.rs`/`power.rs` for
`aldo`/`0x90`/`0x92`/`ldo` finds only the "we deliberately do NOT touch" comment
— no enable.

Implication: if the ES7210 (and/or its mic bias) is supplied by ALDO1, then even
with our new `es7210.rs` driver:
- our I2C init would **NACK** (init prints `[ES7210] init FAILED` — `src/main.rs:625`), **or**
- the chip inits but the analog front end has no supply → **capture still reads silence**.

**Action for on-glass verification:** watch the boot log for
`[ES7210] init OK` vs `FAILED`. If FAILED, add an AXP2101 ALDO1 enable
(`0x92`=3.3 V then `0x90 |= 0x01`) **before** `mic_adc.init()` and re-test. This
is a ~3-line addition and is the most likely remaining blocker for the mic. (It
does not conflict with the "don't touch rails" caution — that caution is about
DC1/panel rails; ALDO1 is the mic rail the vendor proves is safe to enable.)

---

## 11. Quick file index (vendor)

| Concern | File |
|---|---|
| Entry | `main/main.cc` |
| Orchestration / state | `main/application.cc`, `application.h`, `device_state_machine.cc`, `device_state.h` |
| Audio service (task graph) | `main/audio/audio_service.cc/.h` |
| Engines | `main/audio/engines/lite_audio_engine.cc` (C6), `afe_audio_engine.cc` (S3/P4) |
| Wake word | `main/audio/wake_words/esp_wake_word.cc` (C6), `custom_wake_word.cc` (MultiNet) |
| Codecs | `main/audio/codecs/box_audio_codec.cc` (ES8311+ES7210, our board), `es8311_audio_codec.cc` |
| Protocol | `main/protocols/{protocol,websocket_protocol,mqtt_protocol}.cc` |
| OTA / activation | `main/ota.cc` |
| MCP tools | `main/mcp_server.cc/.h` |
| Display / emoji | `main/display/lcd_display.cc`, `lvgl_display/*`, `emote_display.cc` |
| Provisioning | `main/boards/common/wifi_board.cc` |
| Board | `main/boards/waveshare/esp32-c6-touch-amoled-2.06/{config.h,config.json,esp32-c6-touch-amoled-2.06.cc}` |
| Build config | `sdkconfig.defaults`, `sdkconfig.defaults.esp32c6`, `main/idf_component.yml`, `main/Kconfig.projbuild` |

Deep-dive working notes: `scratch/vendor-analysis/{proto-scout,mcp-scout,ui-scout,nebula}.md`.
