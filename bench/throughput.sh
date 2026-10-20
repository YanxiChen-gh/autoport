#!/usr/bin/env bash
#
# throughput.sh - measure the throughput of many simultaneous SSH port forwards,
# the exact arrangement autoport creates.
#
# It starts N disposable "firehose" servers on the remote (each streams zeros
# with a Content-Length so curl can time it), forwards them, then sweeps how
# many forwards are pulling data at once and reports aggregate + per-stream MB/s.
#
# The point of interest: autoport puts every forward on ONE ssh ControlMaster
# (a single TCP connection + cipher stream). MODE=master measures that. MODE=
# separate gives every port its own ssh connection so you can see what the
# multiplexing costs (or saves) you. MODE=both runs the two back to back.
#
# Usage:
#   bench/throughput.sh [HOST]
#
# Env knobs (all optional):
#   HOST=172.18.128.11   remote host (also positional arg 1)
#   MAX=16               number of forwards to set up
#   SIZE_MB=128          bytes streamed per forward, per round
#   SWEEP="1 2 4 8 16"   stream counts to test (defaults derived from MAX)
#   MODE=master          master | separate | both
#   RBASE=47000          first remote port to use
#   LBASE=49000          first local port to use
#
# Requires only ssh + curl locally and python3 on the remote. Nothing is left
# running: a trap tears down forwards, masters, sockets and remote servers.

set -euo pipefail

HOST="${1:-${HOST:-172.18.128.11}}"
MAX="${MAX:-16}"
SIZE_MB="${SIZE_MB:-128}"
MODE="${MODE:-master}"
RBASE="${RBASE:-47000}"
LBASE="${LBASE:-49000}"
SERVER_TTL="${SERVER_TTL:-900}"           # remote servers self-exit after this
ROUNDS="${ROUNDS:-2}"                      # measured rounds per point; best is kept
SIZE_BYTES=$(( SIZE_MB * 1024 * 1024 ))
MARK="autoport_firehose"                  # tag so we can always kill remote servers

# Default sweep: 1,2,4,8,... capped at MAX, with MAX itself included.
if [[ -z "${SWEEP:-}" ]]; then
  SWEEP=""; k=1
  while (( k < MAX )); do SWEEP+="$k "; k=$(( k * 2 )); done
  SWEEP+="$MAX"
fi

WORK="$(mktemp -d)"
SOCK="$WORK/master.sock"
SRV_OUT="$WORK/server.out"
SEP_SOCKS=()           # control sockets for MODE=separate
SRV_PID=""

log() { printf '\033[36m[bench]\033[0m %s\n' "$*"; }

cleanup() {
  set +e
  # shared master + its forwards
  ssh -S "$SOCK" -O exit "$HOST" >/dev/null 2>&1
  # per-port connections (separate mode)
  for s in "${SEP_SOCKS[@]:-}"; do
    [[ -n "$s" ]] && ssh -S "$s" -O exit "$HOST" >/dev/null 2>&1
  done
  # remote firehose servers: forked children survive their parent ssh, so kill
  # them explicitly by marker (the TTL deadline is a last-resort backstop).
  [[ -n "$SRV_PID" ]] && kill "$SRV_PID" >/dev/null 2>&1
  ssh -o BatchMode=yes -o ConnectTimeout=10 "$HOST" \
      "pkill -f 'autoport[_]firehose'" >/dev/null 2>&1
  rm -rf "$WORK"
}
trap cleanup EXIT INT TERM

ssh_master_opts=(
  -o ControlMaster=yes -o ConnectTimeout=10
  -o ServerAliveInterval=15 -o ServerAliveCountMax=3
  -o ExitOnForwardFailure=yes -o BatchMode=yes
)

# ---- 1. launch remote firehose servers -------------------------------------
log "starting $MAX firehose servers on $HOST ports $RBASE..$((RBASE+MAX-1))"
# clear any servers left behind by an aborted previous run
if ssh -o BatchMode=yes -o ConnectTimeout=10 "$HOST" \
       "pkill -f 'autoport[_]firehose' 2>/dev/null" >/dev/null 2>&1; then
  sleep 0.5   # let the old listeners release their ports
fi
ssh -o BatchMode=yes -o ConnectTimeout=10 "$HOST" \
    python3 - "$RBASE" "$MAX" "$SIZE_BYTES" "$SERVER_TTL" "$MARK" >"$SRV_OUT" 2>&1 <<'PY' &
import os, socket, sys, time
base, n, size, ttl = (int(sys.argv[1]), int(sys.argv[2]),
                      int(sys.argv[3]), float(sys.argv[4]))
deadline = time.time() + ttl
buf = b"\0" * (1 << 20)  # 1 MiB

# Bind everything first so "READY" truly means ready.
socks = []
for i in range(n):
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(("0.0.0.0", base + i))
    s.listen(16)
    s.settimeout(1.0)
    socks.append(s)

def serve(s):
    while time.time() < deadline:
        try:
            c, _ = s.accept()
        except socket.timeout:
            continue
        except OSError:
            break
        try:
            c.recv(65536)  # swallow the request line/headers
            c.sendall(b"HTTP/1.1 200 OK\r\nContent-Length: %d\r\n"
                      b"Connection: close\r\n\r\n" % size)
            sent = 0
            while sent < size:
                m = min(len(buf), size - sent)
                c.sendall(buf[:m]); sent += m
        except (BrokenPipeError, ConnectionResetError, OSError):
            pass
        finally:
            try: c.close()
            except OSError: pass

pids = []
for i, s in enumerate(socks):
    pid = os.fork()
    if pid == 0:
        for j, o in enumerate(socks):
            if j != i: o.close()
        serve(s)
        os._exit(0)
    s.close()
    pids.append(pid)
print("READY", flush=True)
for pid in pids:
    try: os.waitpid(pid, 0)
    except ChildProcessError: pass
PY
SRV_PID=$!

for _ in $(seq 1 60); do
  grep -q READY "$SRV_OUT" 2>/dev/null && break
  if ! kill -0 "$SRV_PID" 2>/dev/null; then
    log "remote server failed to start:"; cat "$SRV_OUT"; exit 1
  fi
  sleep 0.25
done
grep -q READY "$SRV_OUT" 2>/dev/null || { log "timed out waiting for servers"; cat "$SRV_OUT"; exit 1; }

# ---- 2. context ------------------------------------------------------------
cipher="$(ssh -G "$HOST" 2>/dev/null | awk '/^ciphers /{print $2; exit}' | cut -d, -f1)"
log "host=$HOST  forwards=$MAX  size/stream=${SIZE_MB}MB  cipher~=${cipher:-?}"

# pull one stream once to warm the path and prime measurements
warm() {
  curl -s -o /dev/null --max-time 60 "http://127.0.0.1:$((LBASE))/" || true
}

# One transfer through the first $1 local ports concurrently. Prints raw
# numbers: "<streams> <aggregate MB/s> <per-stream MB/s> <sum-of-curl MB/s>".
# Aggregate is total bytes / wall-clock; sum-of-curl adds the per-stream
# averages curl reports (the two agree on a shared pipe and diverge when
# independent streams stagger, which is itself informative).
one_transfer() {
  local k="$1" i t0 t1
  local sd="$WORK/round"; rm -rf "$sd"; mkdir -p "$sd"
  local pids=()
  t0="$(date +%s.%N)"
  for (( i = 0; i < k; i++ )); do
    curl -s -o /dev/null -w '%{speed_download}' --max-time 120 \
         "http://127.0.0.1:$((LBASE + i))/" >"$sd/$i" 2>/dev/null &
    pids+=("$!")
  done
  for p in "${pids[@]}"; do wait "$p" || true; done
  t1="$(date +%s.%N)"
  awk -v k="$k" -v sz="$SIZE_BYTES" -v t0="$t0" -v t1="$t1" -v dir="$sd" '
    BEGIN {
      dt = t1 - t0; if (dt <= 0) dt = 1e-6;
      agg = k * sz / dt / 1048576.0;
      sum = 0;
      for (i = 0; i < k; i++) {
        f = dir "/" i; getline v < f; close(f); sum += v / 1048576.0;
      }
      printf "%d %.1f %.1f %.1f\n", k, agg, agg/k, sum;
    }'
}

# Best-of-ROUNDS for one stream count, to shrug off straggler/scheduling noise.
run_round() {
  local k="$1" rows="$WORK/rows"; : > "$rows"
  for (( r = 0; r < ROUNDS; r++ )); do one_transfer "$k" >> "$rows"; done
  sort -k2 -gr "$rows" | head -1 \
    | awk '{ printf "%8d  %12.1f  %14.1f  %12.1f\n", $1, $2, $3, $4 }'
}

# ---- 3a. shared-master mode (what autoport does) ---------------------------
bench_master() {
  log "MODE=master  (single ControlMaster, $MAX forwards over one connection)"
  ssh -f -N "${ssh_master_opts[@]}" -S "$SOCK" "$HOST"
  local i
  for (( i = 0; i < MAX; i++ )); do
    ssh -S "$SOCK" -O forward -L "127.0.0.1:$((LBASE+i)):127.0.0.1:$((RBASE+i))" "$HOST"
  done
  warm
  printf '  streams   aggregate MB/s   per-stream MB/s   sum(curl) MB/s\n'
  printf '  -------   --------------   ---------------   --------------\n'
  for k in $SWEEP; do (( k <= MAX )) && run_round "$k"; done
  ssh -S "$SOCK" -O exit "$HOST" >/dev/null 2>&1
}

# ---- 3b. one-connection-per-port mode (for comparison) ---------------------
bench_separate() {
  log "MODE=separate  ($MAX independent ssh connections, one per forward)"
  local i
  for (( i = 0; i < MAX; i++ )); do
    local s="$WORK/sep-$i.sock"; SEP_SOCKS+=("$s")
    ssh -f -N "${ssh_master_opts[@]}" -S "$s" \
        -L "127.0.0.1:$((LBASE+i)):127.0.0.1:$((RBASE+i))" "$HOST"
  done
  warm
  printf '  streams   aggregate MB/s   per-stream MB/s   sum(curl) MB/s\n'
  printf '  -------   --------------   ---------------   --------------\n'
  for k in $SWEEP; do (( k <= MAX )) && run_round "$k"; done
  for s in "${SEP_SOCKS[@]}"; do ssh -S "$s" -O exit "$HOST" >/dev/null 2>&1; done
  SEP_SOCKS=()
}

case "$MODE" in
  master)   bench_master ;;
  separate) bench_separate ;;
  both)     bench_master; echo; bench_separate ;;
  *) echo "unknown MODE=$MODE (use master|separate|both)" >&2; exit 2 ;;
esac

log "done"
