#!/usr/bin/env bash
# Print the path of a callgrind profile of an oracle, taking it only if nobody has.
#
# callgrind simulates, so a profile is a fact about the binary, the arguments and the
# valgrind that took it: a second one costs fifty times the run and says the same thing.
# The first tree to need one leaves it beside the oracle, and every other tree reads it.
#
# AN ORACLE ONLY. Its directory is keyed by what it was built from and never rewritten, so
# a profile beside it cannot outlive its binary. A port's own binary changes under one
# path, and caching its profile would report the previous build's numbers.
#
# HARDWARE COUNTERS AND TIME ARE NOT CACHED, here or anywhere: they depend on what the
# machine was doing, and are comparable only inside one interleaved run. That run is
# oracle_counters.py's.
#
# fcfish, mcfish, rfish and the golden's `refish` branch carry this file byte for byte.
#
# Usage:
#   oracle_profile.sh <oracle> [bench-args...]   `bench` with them (default: 16 1 8)
#   oracle_profile.sh <oracle> --startup         load, then quit
#
# Beside the printed <path>: <path>.log, both streams of the run, and <path>.nodes.
#
# Exit: 0 printed a path, 1 the run searched nothing, 2 the rig is wrong or not an oracle.
set -euo pipefail

die() { local rc=$1; shift; printf '\033[31moracle-profile: %s\033[0m\n' "$@" >&2; exit "$rc"; }

[[ $# -ge 1 ]] || die 2 "usage: oracle_profile.sh <oracle> [bench-args... | --startup]"
[[ -x $1 ]] || die 2 "$1 is not executable"
bin=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
shift
[[ -f $(dirname "$bin")/BUILD ]] || die 2 "$bin is not an oracle: no BUILD file beside it"
command -v valgrind >/dev/null || die 2 "valgrind is not installed"

args=("$@")
[[ ${#args[@]} -gt 0 ]] || args=(16 1 8)
for a in "${args[@]}"; do
  # A file names its content by a path that can change, so it cannot be part of a key.
  [[ $a =~ ^(--startup|[A-Za-z0-9]+)$ ]] || die 2 "argument '$a' cannot key a profile"
done

dir=$(dirname "$bin")/callgrind-$(valgrind --version)
name=$(IFS=-; printf '%s' "${args[*]#--}")
out=$dir/$name.out

if [[ ! -s $out ]]; then
  mkdir -p "$dir"
  exec 9> "$dir/.lock"
  if command -v flock >/dev/null; then flock 9; fi
fi

if [[ ! -s $out ]]; then
  printf '\033[36m==>\033[0m oracle-profile: %s, %s (once per oracle, this is slow)\n' "${bin#"$(dirname "$(dirname "$(dirname "$bin")")")"/}" "${args[*]}" >&2
  tmp=$(mktemp -d "$dir/.run.XXXXXX")
  trap 'rm -rf -- "$tmp"' EXIT
  run=("$bin" bench "${args[@]}")
  [[ ${args[0]} != --startup ]] || run=("$bin")
  (cd "$(dirname "$bin")" && printf 'quit\n' |
    valgrind --tool=callgrind --callgrind-out-file="$tmp/out" --cache-sim=yes --branch-sim=yes \
      "${run[@]}") > "$tmp/log" 2>&1 9>&- || die 2 "valgrind failed: $(tail -1 "$tmp/log")"
  sed -n 's/^Nodes searched *: *\([0-9]*\).*/\1/p' "$tmp/log" > "$tmp/nodes"
  # A profile of a run that never searched looks plausible and is worthless.
  [[ ${args[0]} == --startup || -s $tmp/nodes ]] || die 1 "the run carries no node count"
  mv -- "$tmp/log" "$out.log"
  mv -- "$tmp/nodes" "$out.nodes"
  mv -- "$tmp/out" "$out"
fi

printf '%s\n' "$out"
