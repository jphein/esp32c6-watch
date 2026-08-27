#!/usr/bin/env python3
"""Analyse a watch serial capture — compute the conclusions instead of eyeballing.

    python3 tools/capture_analyze.py <capture.txt> [capture2.txt ...]

WHY THIS EXISTS. Every wrong call in the CYD-C5 performance campaign came from an
instrument that could not discriminate between the live hypotheses, or from a
statistic quoted past the data that supported it:

  * a burst DURATION inferred by multiplying a frame COUNT by a guessed per-frame
    cost — twice, and both times the guess agreed with the wrong hypothesis;
  * `dt` (time since last paint) read as per-paint COST, which is only true for
    back-to-back frames and is pure PACING for 1 Hz-spaced ones;
  * "65-80 % is render CPU", derived from `dt` and retired once `render=` existed;
  * `strings | grep -c` run on two artifacts and generalised to ten;
  * a readiness banner treated as proof that the path it announced works.

So this tool refuses to report a number it cannot support. Every derived statistic
carries its sample count, and anything under MIN_SAMPLES is printed as
INSUFFICIENT rather than as a value. A conclusion you cannot audit is a guess with
better formatting.

It reads captures only — no serial port, no device. Safe to run any time.
"""
from __future__ import annotations

import re
import sys
from collections import Counter
from pathlib import Path

MIN_SAMPLES = 5  # below this, report INSUFFICIENT rather than a statistic

RE_RENDER = re.compile(
    r"\[RENDER-DBG\] draw=true lines=(\d+) spans=(\d+) dt=(\d+)ms(?: render=(\d+)ms)?"
)
RE_LOOP = re.compile(
    r"\[LOOP\] beat=(\d+) up=(\d+)s heap=(\d+) low=(\d+) main=(\d+) recl=(\d+)"
    r"(?: psram=(\d+))? maxblk=(\d+)"
)
RE_PSRAM = re.compile(r"\[PSRAM\] region(\d+) registered: size=(\d+) B .*external=(\w+)")
RE_PSRAM_OLD = re.compile(r"\[PSRAM\].*?(\d+) KB mapped as External")
RE_PSRAM_BAD = re.compile(r"\[PSRAM\] .*MISMATCH|\[PSRAM\] init reported 0")
RE_LAUNCH_OK = re.compile(r"\[LAUNCHER\] open attempt: total_free=(\d+)")
RE_LAUNCH_NO = re.compile(r"\[LAUNCHER\] DECLINED open: total_free=(\d+)")
RE_DBGCON = re.compile(r"\[DBGCON\] ready \(([^)]+)\)")
RE_PERF = re.compile(
    r"\[DBGCON\] perf .*?frames_us=\[([0-9,\s]*)\].*?max_us=(\d+).*?avg_us=(\d+)"
)
RE_DBGCON_FAIL = re.compile(r"\[DBGCON\].*UART0 RX unavailable")
RE_PANIC = re.compile(r"\[PANIC")
RE_BOOT = re.compile(r"=== smol watch")


def pct(vals, p):
    """Percentile without numpy; vals must be non-empty and sorted-able."""
    s = sorted(vals)
    if not s:
        return None
    k = (len(s) - 1) * p / 100.0
    lo, hi = int(k), min(int(k) + 1, len(s) - 1)
    return s[lo] if lo == hi else s[lo] + (s[hi] - s[lo]) * (k - lo)


def stat_line(label, vals, unit="ms"):
    n = len(vals)
    if n == 0:
        return f"  {label:<34} no samples"
    if n < MIN_SAMPLES:
        return (
            f"  {label:<34} INSUFFICIENT (n={n}<{MIN_SAMPLES}) "
            f"raw={sorted(vals)}{unit}"
        )
    return (
        f"  {label:<34} n={n:<5} min={min(vals)}{unit} "
        f"p50={pct(vals,50):.0f}{unit} p95={pct(vals,95):.0f}{unit} "
        f"max={max(vals)}{unit}"
    )


def analyse(paths):
    renders = []  # (lines, spans, dt, render_or_None)
    loops = []
    psram = []
    launch_ok, launch_no = [], []
    transports, dbgcon_fail = [], 0
    perf = []  # (max_us, avg_us, [frames_us])
    panics = boots = 0
    total_lines = 0

    for p in paths:
        for raw in Path(p).read_text(errors="replace").splitlines():
            total_lines += 1
            if m := RE_RENDER.search(raw):
                renders.append(
                    (int(m[1]), int(m[2]), int(m[3]), int(m[4]) if m[4] else None)
                )
            elif m := RE_LOOP.search(raw):
                loops.append(m.groups())
            elif m := RE_PSRAM.search(raw):
                psram.append(("new", int(m[1]), int(m[2]), m[3]))
            elif m := RE_PSRAM_OLD.search(raw):
                psram.append(("old", None, int(m[1]) * 1024, "true"))
            elif m := RE_LAUNCH_OK.search(raw):
                launch_ok.append(int(m[1]))
            elif m := RE_LAUNCH_NO.search(raw):
                launch_no.append(int(m[1]))
            elif m := RE_PERF.search(raw):
                frames = [int(x) for x in m[1].split(",") if x.strip()]
                perf.append((int(m[2]), int(m[3]), frames))
            elif m := RE_DBGCON.search(raw):
                transports.append(m[1])
            if RE_DBGCON_FAIL.search(raw):
                dbgcon_fail += 1
            if RE_PANIC.search(raw):
                panics += 1
            if RE_BOOT.search(raw):
                boots += 1

    print(f"\n{'='*72}\nCAPTURE: {', '.join(str(p) for p in paths)}")
    print(f"{total_lines} lines · {boots} boot banner(s) · {panics} panic line(s)\n")

    # ---- transport ------------------------------------------------------
    print("TRANSPORT / CONSOLE")
    if transports:
        for t, c in Counter(transports).items():
            print(f"  [DBGCON] ready ({t})  x{c}")
        print("  ⚠️  A 'ready' banner proves CONSTRUCTION ONLY. It is printed by the")
        print("      path under test, so it cannot evidence that the path works.")
        print("      The acceptance signal is an ANSWERED command, not this line.")
    else:
        print("  no [DBGCON] ready line — console absent or capture missed boot")
    if dbgcon_fail:
        print(f"  🔴 'UART0 RX unavailable' x{dbgcon_fail} — construction FAILED")

    # ---- psram ----------------------------------------------------------
    print("\nEXTERNAL MEMORY")
    if psram:
        for kind, idx, size, ext in psram:
            where = f"region{idx}" if idx is not None else "region?"
            print(
                f"  {where}: {size} B ({size/1048576:.2f} MB) external={ext} "
                f"[{kind}-style line]"
            )
            if idx is not None and idx != 0:
                print("  🔴 NOT at index 0 — capability-less allocs will prefer an")
                print("      earlier region; the ordering fix is not in effect.")
            if size == 0:
                print("  🔴 ZERO BYTES — chip did not answer the ID probe. This build")
                print("      is running on internal RAM only while claiming otherwise.")
    else:
        print("  no [PSRAM] line — either absent, or the attach gap ate the boot.")
        print("  NOTE: absence here is NOT evidence the region is missing.")

    # ---- render ---------------------------------------------------------
    print("\nRENDER")
    if not renders:
        print("  no [RENDER-DBG] lines (needs --features touch-telemetry)")
    else:
        phantom = [r for r in renders if r[0] == 0 and r[1] == 0]
        real = [r for r in renders if r[0] > 0]
        print(f"  paints={len(renders)}  real={len(real)}  phantom(lines=0)={len(phantom)}")
        if phantom:
            print(f"  🔴 {len(phantom)} EMPTY-REGION WALKS — full scene walk, zero pixels.")
            ph_r = [r[3] for r in phantom if r[3] is not None]
            if ph_r:
                print(stat_line("phantom render= (wasted CPU)", ph_r))
            else:
                print("      (no render= field — cost unmeasured; rebuild with it)")
        else:
            print("  ✅ no empty-region walks in this capture")

        have_render = [r for r in real if r[3] is not None]
        if not have_render:
            print("  ⚠️  no render= field: per-paint COST IS UNMEASURED. `dt` alone")
            print("      conflates interval and cost and must not be read as cost.")
        else:
            print(stat_line("render= (real paints)", [r[3] for r in have_render]))
            print(stat_line("dt= (real paints)", [r[2] for r in have_render]))
            # per-line model
            per = [r[3] / r[0] for r in have_render if r[0] > 0 and r[3] > 0]
            if len(per) >= MIN_SAMPLES:
                print(
                    f"  {'per-line cost':<34} n={len(per)} "
                    f"min={min(per):.3f} p50={pct(per,50):.3f} max={max(per):.3f} ms/line"
                )
            elif per:
                print(
                    f"  {'per-line cost':<34} INSUFFICIENT (n={len(per)}) "
                    f"raw={[round(x,3) for x in per]} ms/line"
                )
            # THE DECOMPOSITION — and the trap inside it.
            #
            # ⚠️ AN EARLIER VERSION OF THIS BLOCK GOT THIS WRONG, in exactly the
            # error class this tool exists to prevent. It computed
            # render/dt and, on a low share, printed "suspect loop-body latency".
            # But `dt` is time since the last paint, so the remainder is IDLE
            # WAITING whenever the loop is not painting continuously — and on a
            # real capture the dt max was 40,274 ms, i.e. forty seconds of an idle
            # board. Calling that "latency" is the same mistake as reading `dt` as
            # cost, wearing a different hat. The remainder is only work if the
            # paints were back-to-back, which is exactly what dt cannot tell you.
            #
            # The honest discriminator is `perf`, which times the LOOP BODY
            # directly: if worst-loop ≈ worst-render, there is no hidden non-render
            # work, whatever dt says. That comparison is made below.
            rem = [(r[2] - r[3]) for r in have_render if r[2] >= r[3]]
            dts = [r[2] for r in have_render]
            if len(rem) >= MIN_SAMPLES:
                print("\n  DECOMPOSITION  dt = render + inter-paint remainder")
                print(stat_line("remainder (render-free interval)", rem))
                p95d = pct(dts, 95) or 0
                if max(dts) > 3 * max(p95d, 1):
                    print(f"  ⚠️  dt is IDLE-DOMINATED: max={max(dts)}ms vs "
                          f"p95={p95d:.0f}ms.")
                    print("      The remainder here is mostly the board WAITING, not")
                    print("      working. Do NOT read it as loop latency — use `perf`.")
                else:
                    print("  dt spread is not idle-dominated; remainder may be real work,")
                    print("  but `perf` is still the instrument that can prove it.")
            elif rem:
                print(
                    f"\n  DECOMPOSITION  INSUFFICIENT (n={len(rem)}<{MIN_SAMPLES}) "
                    f"remainder raw={sorted(rem)}ms"
                )

    # ---- loop body, from perf -------------------------------------------
    # THE instrument for "is there non-render work in the loop". `perf` times the
    # loop BODY, so unlike `dt` it cannot confuse waiting with working.
    if perf:
        print("\nLOOP BODY (from `perf` — the only instrument that can settle this)")
        all_frames = [f for _, _, frames in perf for f in frames]
        max_us = max(m for m, _, _ in perf)
        print(f"  perf reports={len(perf)}  frames={len(all_frames)}  "
              f"worst loop body={max_us/1000:.1f}ms")
        if all_frames:
            # The distribution is expected to be BIMODAL: idle iterations cost
            # microseconds, painting iterations cost render-sized milliseconds.
            # A middle band would be the interesting thing — work that is neither.
            idle = [f for f in all_frames if f < 1000]
            work = [f for f in all_frames if f >= 1000]
            print(f"  idle frames (<1ms): n={len(idle)}"
                  + (f" range={min(idle)}-{max(idle)}µs" if idle else ""))
            print(f"  work frames (>=1ms): n={len(work)}"
                  + (f" range={min(work)/1000:.1f}-{max(work)/1000:.1f}ms" if work else ""))
        if renders:
            rmax = max((r[3] for r in renders if r[3] is not None), default=None)
            if rmax is not None:
                gap = max_us / 1000.0 - rmax
                print(f"\n  worst loop body  {max_us/1000:.1f}ms")
                print(f"  worst render=    {rmax}ms")
                print(f"  UNACCOUNTED      {gap:+.1f}ms")
                if abs(gap) <= max(10.0, 0.15 * rmax):
                    print("  ✅ The worst loop iteration IS the worst render. There is")
                    print("     NO hidden non-render work of consequence — whatever the")
                    print("     dt remainder suggests, it was idleness.")
                else:
                    print("  🔴 A real gap: the worst loop iteration carries work beyond")
                    print("     rendering. THIS is the number worth hunting (dt is not).")
        else:
            print("  no render= to correlate against — cannot attribute the loop cost")

    # ---- launcher / heap ------------------------------------------------
    print("\nLAUNCHER")
    if not (launch_ok or launch_no):
        print("  no [LAUNCHER] lines — launcher never attempted in this capture")
    else:
        print(f"  opened={len(launch_ok)}  DECLINED={len(launch_no)}")
        if launch_ok:
            print(f"    free at open: {launch_ok}")
        if launch_no:
            print(f"  🔴 DECLINED at free={launch_no}")
            print("      On a PSRAM build a DECLINE means the external region did NOT")
            print("      register. Check the [PSRAM] line FIRST, not the floor value.")

    if loops:
        print("\nHEAP (from [LOOP] beats)")
        psram_col = [int(l[6]) for l in loops if l[6] is not None]
        main_col = [int(l[4]) for l in loops]
        recl_col = [int(l[5]) for l in loops]
        print(f"  beats={len(loops)}")
        print(f"  main  min={min(main_col)} max={max(main_col)}")
        print(f"  recl  min={min(recl_col)} max={max(recl_col)}")
        if psram_col:
            print(f"  psram min={min(psram_col)} max={max(psram_col)}")
        else:
            print("  psram field absent — pre-PSRAM build, or labels are POSITIONAL")
            print("  and may be misreporting which region is which.")
        mb = [int(l[7]) for l in loops]
        print(f"  maxblk min={min(mb)} max={max(mb)}")
        print("  NOTE: on a PSRAM build `maxblk_main` is structurally 0 — nothing")
        print("  reaches main. That is the DESIGNED state, not exhaustion.")

        # ---- SAMPLING-GAP CROSS-CHECK ----------------------------------
        # Two instruments, two cadences: [LOOP] samples on a ~2.7 s beat, while
        # the launcher pre-flight reads at the INSTANT of an attempt. If the
        # event-triggered reading is below the beat minimum, the beat cadence
        # never saw the real trough — and any "steady at X" claim drawn from
        # beat data understates it. This check exists because that exact error
        # was made by hand: a beat-sampled figure was quoted as the floor while
        # the allocation that actually failed happened between two beats.
        event_lows = launch_ok + launch_no
        if event_lows and loops:
            beat_min_total = min(int(l[2]) for l in loops)  # heap= column
            worst_event = min(event_lows)
            if worst_event < beat_min_total:
                print("\n  🔴 SAMPLING GAP DETECTED — the beat cadence missed the trough")
                print(f"     event-triggered low : {worst_event} B (launcher attempt)")
                print(f"     beat-sampled low    : {beat_min_total} B ([LOOP] heap=)")
                print(f"     unseen by beats     : {beat_min_total - worst_event} B")
                print("     ⇒ Do NOT quote the beat minimum as the floor. A statistic")
                print("       from this column describes the sampling, not the heap.")
            else:
                print(f"\n  ✅ no sampling gap: worst event low ({worst_event} B) >= "
                      f"beat low ({beat_min_total} B)")
    print(f"\n{'='*72}\n")


if __name__ == "__main__":
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    analyse([Path(a) for a in sys.argv[1:]])
