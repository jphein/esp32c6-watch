#!/usr/bin/env python3
"""provision.py — host-side writer for the watch's SWCFG7 config record.

Builds the exact record `src/peripherals/config.rs::save_slot` writes and
flashes it into the `config` partition (0xc10000, backup slot at +0x1000 —
both slots, like `save` does), via `espflash write-bin`. This is the CANONICAL
fresh-device provisioning path: the firmware itself only writes the record
from on-glass Settings changes, and neither node id nor WiFi creds are
reachable there on a fresh board.

Node id: 42 is the "unset" sentinel — the firmware then falls back to the
MAC-derived id. An ALLOCATED id (e.g. the CYD-C5's 176, smol's allocation
where the MAC fold says 121) is exactly what this override exists for; see
crates/sigil-id FLEET_NODES.

SECRETS: the PSK is never taken on argv (ps leaks argv). Use --pass-env NAME
or --pass-file PATH; e.g.  WATCH_PSK="$(bw get password jplovescl)" \\
    tools/provision.py --port /dev/ttyACMx --node-id 176 \\
        --ssid jplovescl --pass-env WATCH_PSK --write

MQTT broker is NOT in this record — it is a compile-time constant
(`option_env!("MQTT_BROKER")`, src/net/mqtt_ha.rs) from the build tree's
gitignored .cargo/config.toml [env]. Provision that at BUILD time.

Without --write this is a dry run: prints the record hex + a decode and
writes <out>.bin next to nothing. The flash write erases only the two 4 KB
config sectors — OTA slots, otadata and SPIFFS-neighbours are untouched.
"""

from __future__ import annotations

import argparse
import os
import subprocess
import sys
import tempfile

MAGIC = b"SWCFG7"
REC_LEN = 6 + 1 + 1 + 1 + 32 + 1 + 64 + 1 + 1 + 1 + 1 + 1 + 1 + 4 + 1 + 2  # 119
CONFIG_PART = 0xC10000
BACKUP_SLOT = 0x1000

# Bit masks — mirror config.rs (UNITS_*, RADIO_*, TOUCH_SOUND_OFF, VOL_*).
UNITS_TEMP_F = 0x01
UNITS_CLK_24H = 0x02
RADIO_BLE_ON = 0x01
RADIO_MESH_ON = 0x02
RADIO_WIFI_OFF = 0x04
TOUCH_SOUND_OFF = 0x08
VOL_MUTED_BIT = 0x10

# ButtonAction discriminants (config.rs enum order).
ACTIONS = ["none", "volup", "voldown", "mute", "powermenu", "shutdown",
           "launcher", "ping", "voice", "speak"]
SPEAK_MODES = ["off", "ondemand", "auto"]


def build_record(a: argparse.Namespace, psk: str) -> bytes:
    ssid = a.ssid.encode()
    pw = psk.encode()
    if len(ssid) > 32:
        sys.exit("ssid > 32 bytes")
    if len(pw) > 64:
        sys.exit("pass > 64 bytes")
    buf = bytearray(REC_LEN)
    buf[:6] = MAGIC
    buf[6] = a.node_id
    buf[7] = a.brightness
    buf[8] = len(ssid)
    buf[9:9 + len(ssid)] = ssid
    buf[41] = len(pw)
    buf[42:42 + len(pw)] = pw
    buf[106] = a.page
    buf[107] = (UNITS_CLK_24H if a.clk24 else 0) | (0 if a.celsius else UNITS_TEMP_F)
    buf[108] = a.theme
    buf[109] = ((RADIO_BLE_ON if a.ble else 0)
                | (RADIO_MESH_ON if a.mesh else 0)
                | (RADIO_WIFI_OFF if a.wifi_off else 0)
                | (TOUCH_SOUND_OFF if a.no_touch_sound else 0))
    buf[110] = a.mic_gain
    buf[111] = (a.volume & 0x0F) | (VOL_MUTED_BIT if a.muted else 0)
    buf[112] = ACTIONS.index(a.boot_short)
    buf[113] = ACTIONS.index(a.boot_long)
    buf[114] = ACTIONS.index(a.pwron_short)
    buf[115] = ACTIONS.index(a.pwron_long)
    buf[116] = SPEAK_MODES.index(a.speak)
    ck = sum(buf[:REC_LEN - 2]) & 0xFFFF
    buf[REC_LEN - 2:] = ck.to_bytes(2, "little")
    return bytes(buf)


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--port", help="serial port (required with --write)")
    p.add_argument("--node-id", type=int, required=True,
                   help="mesh node id (42 = unset sentinel; an allocated id like 176 overrides the MAC fold)")
    p.add_argument("--ssid", required=True)
    g = p.add_mutually_exclusive_group(required=True)
    g.add_argument("--pass-env", metavar="NAME", help="env var holding the PSK")
    g.add_argument("--pass-file", metavar="PATH", help="file holding the PSK (first line)")
    g.add_argument("--open-network", action="store_true", help="no PSK")
    p.add_argument("--brightness", type=lambda s: int(s, 0), default=0xD0)
    p.add_argument("--page", type=int, default=0, choices=range(4))
    p.add_argument("--celsius", action="store_true", help="default is Fahrenheit")
    p.add_argument("--clk24", action="store_true", help="default is 12h")
    p.add_argument("--theme", type=int, default=2, choices=range(4),
                   help="0 Midnight 1 Paper 2 Amber 3 Violet (default 2)")
    p.add_argument("--ble", action="store_true")
    p.add_argument("--mesh", action="store_true", help="start SMOLv1 mesh at boot")
    p.add_argument("--wifi-off", action="store_true", help="force WiFi OFF at boot")
    p.add_argument("--no-touch-sound", action="store_true")
    p.add_argument("--mic-gain", type=int, default=0)
    p.add_argument("--volume", type=int, default=11)
    p.add_argument("--muted", action="store_true")
    p.add_argument("--boot-short", default="volup", choices=ACTIONS)
    p.add_argument("--boot-long", default="launcher", choices=ACTIONS)
    p.add_argument("--pwron-short", default="voldown", choices=ACTIONS)
    p.add_argument("--pwron-long", default="powermenu", choices=ACTIONS)
    p.add_argument("--speak", default="ondemand", choices=SPEAK_MODES)
    p.add_argument("--chip", default="esp32c6",
                   help="espflash chip arg (esp32c6 | esp32c5 | esp32s3)")
    p.add_argument("--write", action="store_true",
                   help="actually flash both config slots (default: dry-run print)")
    a = p.parse_args()

    if a.open_network:
        psk = ""
    elif a.pass_env:
        psk = os.environ.get(a.pass_env) or sys.exit(f"env {a.pass_env} empty/unset")
    else:
        psk = open(a.pass_file).readline().rstrip("\n")

    rec = build_record(a, psk)
    print(f"record: SWCFG7 {len(rec)} B  node_id={a.node_id}  ssid={a.ssid!r} "
          f"psk={'<set:%d B>' % len(psk) if psk else '<open>'}  theme={a.theme} "
          f"flags=0x{rec[109]:02x}  checksum=0x{int.from_bytes(rec[-2:], 'little'):04x}")
    if not a.write:
        print("dry-run (no --write): nothing flashed. Hex:")
        print(rec.hex())
        return
    if not a.port:
        sys.exit("--write needs --port")

    with tempfile.NamedTemporaryFile(suffix=".bin", delete=False) as f:
        f.write(rec)
        tmp = f.name
    try:
        for off in (CONFIG_PART, CONFIG_PART + BACKUP_SLOT):
            cmd = [os.path.expanduser("~/.cargo/bin/espflash"), "write-bin",
                   "--chip", a.chip, "--port", a.port, f"0x{off:x}", tmp]
            print("+", " ".join(cmd))
            subprocess.run(cmd, check=True)
        print("provisioned BOTH slots. Reset the board; expect "
              f"[CFG] node id{a.node_id:03d}, ssid={a.ssid!r} in the boot log.")
    finally:
        os.unlink(tmp)


if __name__ == "__main__":
    main()
