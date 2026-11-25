#!/usr/bin/env python3
"""Read the hardware counters of an oracle and of every binary compared with it, in one run.

Cycles, IPC and real cache and branch misses depend on what the machine is doing, so they
are comparable only between runs interleaved with each other: one unchanged binary against
itself reads an IPC ratio more than one percent from 1. A reading of the oracle therefore
cannot be kept and quoted later. It can be SHARED: each round runs the oracle once and each
binary once, and every binary is paired with that one reading. Four binaries cost five runs
a round, where four separate pairs cost eight.

The order rotates every round, so no binary always runs on the hotter core. Startup -- the
net load and the line tables -- is a second, `quit`-only run of the same binary in the same
round, subtracted from it: it is a large share of a shallow bench and differs between ports.

It REFUSES, rather than reporting a ratio that means something else:

  - a binary built by another compiler than the oracle, read from `.comment`. rustc names no
    clang, so its LLVM major, from its linker, is held to the oracle's;
  - a binary that searched another tree, or evaluated with another net;
  - a counter the kernel multiplexed, which covers only part of the run.

It cannot see the tier or the profile a binary was built with. Name the ones it was built
with: --arch and --pgo choose the oracle's, and a mismatch measures the build.

fcfish, mcfish, rfish and the golden's `refish` branch carry this file byte for byte.

Usage:
    oracle_counters.py [options] NAME=BINARY [NAME=BINARY ...]
      --comp C      gcc or clang, the oracle's compiler (default: clang)
      --arch A      the oracle's ARCH (default: x86-64-sse41-popcnt)
      --pgo         the oracle's profile build
      --rounds N    (default: 5)
      --depth N     bench depth (default: 13)

Exit: 0 measured, 1 VOID -- the trees differ, 2 the rig is wrong or a binary is refused.
"""

import argparse
import ctypes
import os
import platform
import re
import statistics
import subprocess
import sys
from pathlib import Path
from typing import NoReturn

ORACLE_SCRIPT = Path(__file__).resolve().parent / "upstream_oracle.sh"
CORE = int(os.environ.get("CORE", "0"))

# PERF_TYPE_HARDWARE configs, from linux/perf_event.h.
EVENTS = {
    "cycles": 0,
    "instructions": 1,
    "cache refs": 2,
    "cache misses": 3,
    "branches": 4,
    "branch misses": 5,
}
# disabled | inherit | exclude_kernel | exclude_hv | enable_on_exec
FLAGS = 1 | 1 << 1 | 1 << 5 | 1 << 6 | 1 << 12
SYSCALL = {"x86_64": 298, "aarch64": 241}

ROWS = ["instructions", "cycles", "IPC", "cache misses", "branch misses", "search time"]


class Attr(ctypes.Structure):
    """perf_event_attr, as far as these events need it; the kernel accepts a zeroed tail."""

    _fields_ = [
        ("type", ctypes.c_uint32),
        ("size", ctypes.c_uint32),
        ("config", ctypes.c_uint64),
        ("sample_period", ctypes.c_uint64),
        ("sample_type", ctypes.c_uint64),
        ("read_format", ctypes.c_uint64),
        ("flags", ctypes.c_uint64),
        ("tail", ctypes.c_uint64 * 10),
    ]


def refuse(message, code=2) -> NoReturn:
    sys.stderr.write(f"oracle-counters: {message}\n")
    sys.exit(code)


def open_counter(config, pid):
    number = SYSCALL.get(platform.machine())
    if number is None:
        refuse(f"no perf_event_open number for {platform.machine()}")
    # read_format: time enabled and time running follow the value.
    attr = Attr(type=0, size=ctypes.sizeof(Attr), config=config, read_format=3, flags=FLAGS)
    libc = ctypes.CDLL(None, use_errno=True)
    fd = libc.syscall(number, ctypes.byref(attr), pid, -1, -1, 0)
    if fd < 0:
        reason = os.strerror(ctypes.get_errno())
        refuse(f"perf_event_open: {reason} (perf_event_paranoid must be 2 or lower)")
    return fd


def counted(argv, cwd):
    """Run argv under the counters; return ({event: count}, output)."""
    hold_read, hold_write = os.pipe()
    out_read, out_write = os.pipe()
    pid = os.fork()
    if pid == 0:
        os.close(hold_write)
        os.sched_setaffinity(0, {CORE})
        os.read(hold_read, 1)
        devnull = os.open(os.devnull, os.O_RDONLY)
        for source, target in ((devnull, 0), (out_write, 1), (out_write, 2)):
            os.dup2(source, target)
        os.chdir(cwd)
        os.execv(argv[0], argv)
    os.close(out_write)
    os.close(hold_read)
    fds = {name: open_counter(config, pid) for name, config in EVENTS.items()}
    os.write(hold_write, b"x")
    os.close(hold_write)
    with os.fdopen(out_read, "rb") as stream:
        output = stream.read().decode(errors="replace")
    _, status = os.waitpid(pid, 0)
    if status != 0:
        refuse(f"{argv[0]} ended with status {status}: {output[-200:]}")
    counts = {}
    for name, fd in fds.items():
        raw = os.read(fd, 24)
        os.close(fd)
        value, enabled, running = (int.from_bytes(raw[i : i + 8], "little") for i in (0, 8, 16))
        if running < enabled:
            refuse(f"the kernel multiplexed '{name}', so it covers part of the run")
        counts[name] = value
    return counts, output


def compiler(binary):
    """What built a binary: ('clang', '23.1.6'), ('gcc', '15.2.0') or ('llvm', '23')."""
    comment = subprocess.run(
        ["readelf", "-p", ".comment", str(binary)], capture_output=True, text=True, check=False
    ).stdout
    if "rustc version" in comment:
        linker = re.search(r"LLD (\d+)\.", comment)
        return ("llvm", linker.group(1)) if linker else ("rustc", "unknown")
    for kind, pattern in (("clang", r"clang version (\S+)"), ("gcc", r"GCC: \([^)]*\) (\S+)")):
        versions = sorted(set(re.findall(pattern, comment)))
        if versions:
            return (kind, "+".join(versions))
    return ("unknown", "unknown")


def same_compiler(ours, oracle):
    if ours[0] == "llvm":
        return oracle[0] == "clang" and oracle[1].split(".")[0] == ours[1]
    return ours == oracle and "unknown" not in ours and "+" not in ours[1]


def searched(output, name):
    """(nodes, search milliseconds, net) a bench printed."""
    nodes = re.search(r"Nodes searched\s*:\s*(\d+)", output)
    took = re.search(r"Total time \(ms\)\s*:\s*(\d+)", output)
    net = re.search(r"NNUE evaluation using (\S+\.nnue)", output)
    if not nodes or not took:
        refuse(f"{name} printed no bench result")
    if not net:
        refuse(f"{name} loaded no net, so it is another engine")
    return int(nodes.group(1)), int(took.group(1)), net.group(1)


def measure(name, binary, depth, cwd):
    """One binary, one round: search-only counters, nodes, net."""
    whole, output = counted([str(binary), "bench", "16", "1", str(depth)], cwd)
    startup, _ = counted([str(binary), "quit"], cwd)
    nodes, took, net = searched(output, name)
    row = {event: whole[event] - startup[event] for event in EVENTS}
    if min(row.values()) <= 0:
        refuse(f"{name}: startup counted more than the run; raise --depth")
    row["IPC"] = row["instructions"] / row["cycles"]
    row["search time"] = took
    return row, nodes, net


def report(name, rounds, nodes):
    print(f"\n{name} / oracle: median of {len(rounds)} paired ratios [min, max]")
    for row in ROWS:
        ratios = [ours[row] / theirs[row] for ours, theirs in rounds]
        low, high = min(ratios), max(ratios)
        verdict = "" if low > 1 or high < 1 else "   no direction: the spread straddles 1"
        median = statistics.median(ratios)
        print(f"  {row:<14} {median:7.4f}  [{low:.4f}, {high:.4f}]{verdict}")
    for side, who in ((0, name), (1, "oracle")):
        per = {
            e: statistics.median(r[side][e] for r in rounds) / nodes
            for e in ("instructions", "cycles", "cache misses", "branch misses")
        }
        print(
            f"  per node, {who:<8} {per['instructions']:9.1f} instr {per['cycles']:9.1f} cycles "
            f"{per['cache misses']:7.2f} cache miss {per['branch misses']:7.2f} branch miss"
        )


def main():
    ap = argparse.ArgumentParser(add_help=True)
    ap.add_argument("--comp", default="clang", choices=["gcc", "clang"])
    ap.add_argument("--arch", default="x86-64-sse41-popcnt")
    ap.add_argument("--pgo", action="store_true")
    ap.add_argument("--rounds", type=int, default=5)
    ap.add_argument("--depth", type=int, default=13)
    ap.add_argument("binaries", nargs="+", metavar="NAME=BINARY")
    args = ap.parse_args()

    binaries = {}
    for spec in args.binaries:
        name, _, path = spec.partition("=")
        binary = Path(path).resolve()
        if not path or not os.access(binary, os.X_OK) or name in binaries or name == "oracle":
            refuse(f"'{spec}' is not NAME=BINARY with its own name and an executable")
        binaries[name] = binary

    env = dict(
        os.environ,
        ORACLE_COMP=args.comp,
        ORACLE_ARCH=args.arch,
        ORACLE_TARGET="profile-build" if args.pgo else "build",
    )
    built = subprocess.run(
        ["bash", str(ORACLE_SCRIPT)], env=env, stdout=subprocess.PIPE, text=True, check=False
    )
    if built.returncode != 0:
        refuse("no oracle")
    oracle = Path(built.stdout.strip())

    theirs = compiler(oracle)
    for name, binary in binaries.items():
        ours = compiler(binary)
        if not same_compiler(ours, theirs):
            refuse(f"{name} is {' '.join(ours)}, the oracle {' '.join(theirs)}: not comparable")
    mode = "PGO" if args.pgo else "no profile"
    print(f"oracle-counters: {oracle.parent.parent.name}, {' '.join(theirs)}, {args.arch}, {mode}")
    print(f"oracle-counters: bench 16 1 {args.depth}, {args.rounds} rounds, core {CORE}")

    # Every binary runs where the oracle's net is, so none falls back to another evaluation.
    everyone = {"oracle": oracle, **binaries}
    order = list(everyone)
    rounds = {name: [] for name in binaries}
    tree, net = ("", 0), ("", "")
    for _ in range(args.rounds):
        seen = {}
        for name in order:
            seen[name], nodes, used = measure(name, everyone[name], args.depth, oracle.parent)
            if not tree[0]:
                tree, net = (name, nodes), (name, used)
            if nodes != tree[1]:
                refuse(f"VOID: {name} searched {nodes} nodes, {tree[0]} {tree[1]}", 1)
            if used != net[1]:
                refuse(f"{name} evaluated with {used}, {net[0]} with {net[1]}")
        for name in binaries:
            rounds[name].append((seen[name], seen["oracle"]))
        order = order[1:] + order[:1]

    print(f"oracle-counters: {tree[1]} nodes on every side")
    for name in binaries:
        report(name, rounds[name], tree[1])
    return 0


if __name__ == "__main__":
    sys.exit(main())
