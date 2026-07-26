#!/usr/bin/env python3
# tools/watch_soak.py — stability harness (born fighting the #61 WiFi crash loop).
# Resets a watch N times, classifies each boot (wifi-panic/brick/download/alive),
# reports crash-rate + time-to-crash. The loop's measurement gate: 0% = stable.
# Companion to watchctl; `watchctl soak` wraps this (follow-up).
# Stability probe: reset a watch N times, classify each boot outcome, report a
# crash rate + time-to-crash. The measurement harness for the WiFi-crash loop.
# Usage: stability_probe.py <port> <trials> <watch_seconds_per_trial>
import sys, time, subprocess, re
try:
    import serial
except Exception:
    print("pyserial required"); sys.exit(2)

PORT = sys.argv[1] if len(sys.argv) > 1 else "/dev/ttyACM3"
TRIALS = int(sys.argv[2]) if len(sys.argv) > 2 else 5
WATCH_S = float(sys.argv[3]) if len(sys.argv) > 3 else 12.0
ESPFLASH = "/home/jp/.cargo/bin/espflash"

def reset():
    subprocess.run([ESPFLASH, "reset", "--port", PORT],
                   capture_output=True, timeout=30)

outcomes = []
for i in range(TRIALS):
    try:
        p = serial.Serial(PORT, 115200, timeout=0.2)
    except Exception as e:
        print(f"trial {i}: port open failed ({e})"); outcomes.append("noport"); continue
    reset()
    t0 = time.time()
    buf = b""
    outcome = "alive"; t_event = None
    while time.time() - t0 < WATCH_S:
        ln = p.readline()
        if not ln: continue
        buf += ln
        s = ln.decode(errors="replace")
        if "ppRxFragmentProc" in s or ("Load access fault" in s):
            outcome = "wifi-panic"; t_event = time.time()-t0; break
        if "panicked" in s:
            outcome = "panic"; t_event = time.time()-t0; break
        if "Checksum failed" in s or "No bootable" in s:
            outcome = "brick"; t_event = time.time()-t0; break
        if "waiting for download" in s:
            outcome = "download-mode"; t_event = time.time()-t0; break
    p.close()
    # count boot banners = reboots within the window
    reboots = buf.count(b"ESP-ROM:esp32c6")
    tag = f"{outcome}" + (f" @{t_event:.1f}s" if t_event else "") + f" (boots={reboots})"
    print(f"trial {i+1}/{TRIALS}: {tag}")
    outcomes.append(outcome)

from collections import Counter
c = Counter(outcomes)
print("=== summary ===")
for k, v in c.most_common():
    print(f"  {k}: {v}/{TRIALS}")
crashes = c.get("wifi-panic",0)+c.get("panic",0)+c.get("brick",0)+c.get("download-mode",0)
print(f"CRASH RATE: {crashes}/{TRIALS} = {100*crashes/TRIALS:.0f}%")
