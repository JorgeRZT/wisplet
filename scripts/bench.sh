#!/usr/bin/env bash
# Measure wisplet's CPU and RAM on a local synthetic swarm.
#
#   scripts/bench.sh [size_mib=256] [piece_kib=256] [seeders=1]
#   CLIENT_FLAGS="--memory-mb 4" scripts/bench.sh 512 4096 8      # extra wisplet flags
#
# Runs the release binary against a seeder in a separate process (loopback), so the numbers
# are the downloader's own. Loopback removes network latency, so this is a *worst case for
# CPU per byte* (the client never waits on the network) -- see docs/PERFORMANCE.md.
set -euo pipefail

SIZE_MIB="${1:-256}"
PIECE_KIB="${2:-256}"
SEEDERS="${3:-1}"
CLIENT_FLAGS="${CLIENT_FLAGS:-}"
cd "$(dirname "$0")/.."

cargo build --release --bin wisplet --example bench_seeder >/dev/null 2>&1
BIN=./target/release/wisplet
WORK="$(mktemp -d)"
SEEDER_PID=""
cleanup() { [ -n "$SEEDER_PID" ] && kill "$SEEDER_PID" 2>/dev/null || true; rm -rf "$WORK"; }
trap cleanup EXIT

./target/release/examples/bench_seeder "$SIZE_MIB" "$WORK/bench.torrent" "$PIECE_KIB" "$SEEDERS" >"$WORK/seeder.log" &
SEEDER_PID=$!
for _ in $(seq 1 300); do [ "$(grep -c LISTEN "$WORK/seeder.log" 2>/dev/null || true)" -ge "$SEEDERS" ] && break; sleep 0.1; done
PEERS=""
for a in $(awk '/LISTEN/ {print $2}' "$WORK/seeder.log"); do PEERS="$PEERS --peer $a"; done
[ -n "$PEERS" ] || { echo "seeder failed to start" >&2; exit 1; }

echo "== one-shot download: ${SIZE_MIB} MiB, ${PIECE_KIB} KiB pieces, $SEEDERS seeder(s) ${CLIENT_FLAGS:+[$CLIENT_FLAGS]}"
if [ "$(uname)" = "Darwin" ]; then
  /usr/bin/time -l "$BIN" get "$WORK/bench.torrent" -o "$WORK/out" --no-dht $PEERS $CLIENT_FLAGS >/dev/null 2>"$WORK/time.txt"
  WALL=$(awk '/ real / {print $1}' "$WORK/time.txt")
  USER=$(awk '/ real / {print $3}' "$WORK/time.txt")
  SYS=$(awk '/ real / {print $5}' "$WORK/time.txt")
  RSS_KIB=$(awk '/maximum resident set size/ {printf "%d", $1/1024}' "$WORK/time.txt")
else
  /usr/bin/time -v "$BIN" get "$WORK/bench.torrent" -o "$WORK/out" --no-dht $PEERS $CLIENT_FLAGS >/dev/null 2>"$WORK/time.txt"
  USER=$(awk -F': ' '/User time/ {print $2}' "$WORK/time.txt")
  SYS=$(awk -F': ' '/System time/ {print $2}' "$WORK/time.txt")
  RSS_KIB=$(awk -F': ' '/Maximum resident/ {print $2}' "$WORK/time.txt")
  WALL=$(awk -F': ' '/Elapsed/ {n=split($2,a,":"); print (n==3? a[1]*3600+a[2]*60+a[3] : a[1]*60+a[2])}' "$WORK/time.txt")
fi
awk -v s="$SIZE_MIB" -v w="$WALL" -v u="$USER" -v y="$SYS" -v r="$RSS_KIB" 'BEGIN {
  cpu = u + y
  printf "wall time        : %.2f s\n", w
  printf "throughput       : %.0f MiB/s\n", s / w
  printf "CPU time         : %.2f s (user %.2f + sys %.2f)\n", cpu, u, y
  printf "CPU per GiB      : %.2f s\n", cpu / (s / 1024)
  printf "CPU utilisation  : %.0f %% of one core while downloading\n", 100 * cpu / w
  printf "peak RSS         : %.1f MiB\n", r / 1024
}'
# (every piece is SHA-1 verified by the client itself before it is written)

echo
echo "== idle daemon: 1 completed torrent, 20 s sample"
export WISPLET_HOME="$WORK/home"
"$BIN" start --no-dht $PEERS --download-dir "$WORK/out" >/dev/null
"$BIN" add "$WORK/bench.torrent" -o "$WORK/out" >/dev/null
for _ in $(seq 1 150); do "$BIN" list | grep -q complete && break; sleep 0.2; done
"$BIN" list | grep -q complete || { echo "ERROR: torrent did not complete; idle numbers would be meaningless" >&2; "$BIN" list >&2; "$BIN" stop >/dev/null; exit 1; }
DPID="$(pgrep -f "wisplet daemon" | head -1)"
cpu_ticks() { ps -o cputime= -p "$DPID" | awk -F'[:.]' '{ if (NF==3) print $1*60+$2+$3/100; else print $1*3600+$2*60+$3+$4/100 }'; }
C0="$(cpu_ticks)"; sleep 20; C1="$(cpu_ticks)"
RSS="$(ps -o rss= -p "$DPID" | awk '{printf "%.1f", $1/1024}')"
awk -v a="$C0" -v b="$C1" -v r="$RSS" 'BEGIN {
  printf "RSS              : %s MiB\n", r
  printf "CPU over 20 s    : %.2f s (%.2f %% of one core)\n", b - a, (b - a) / 20 * 100
}'
"$BIN" stop >/dev/null
