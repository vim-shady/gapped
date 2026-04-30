#!/usr/bin/env bash

# Run gapped through all four phases against
# a given source tree, capturing per-phase time/memory metrics via
# /usr/bin/time -v. Appends one CSV row per phase to $RESULTS
#
# Example:
#   cargo build --release
#   bench/run_bench.sh \
#       --source ./bench-data/synth-1g \
#       --workdir ./bench-work \
#       --label synth-1g \
#       --change-rate 0.05
#
# Environment:
#   GAPPED   path to the gapped binary (default: ./target/release/gapped)
#   RESULTS  path to the CSV output file (default: ./bench-results.csv)
#   DROP_CACHES=1  attempt to drop page cache before each phase (needs sudo)

set -euo pipefail

SOURCE=""
WORKDIR=""
LABEL=""
CHANGE_RATE="0.05"
COMPRESS_FLAG=""
SPLIT_FLAG=()

usage() {
    cat >&2 <<EOF
Usage: $0 --source DIR --workdir DIR --label NAME [options]

Required:
  --source DIR        source tree to benchmark
  --workdir DIR       scratch dir (will be wiped)
  --label NAME        label for this run (appears in CSV)

Optional:
  --change-rate F     fraction of files to modify before diff (default: 0.05)
  --compress          pass --compress to gapped snapshot/diff
  --split-size SIZE   pass --split-size to gapped diff
EOF
    exit 1
}

# parse command arguments
# $# means 'current number of command line arguments'
while [[ $# -gt 0 ]]; do
    case "$1" in
        --source) SOURCE="$2"; shift 2 ;;
        --workdir) WORKDIR="$2"; shift 2 ;;
        --label) LABEL="$2"; shift 2 ;;
        --change-rate) CHANGE_RATE="$2"; shift 2 ;;
        --compress) COMPRESS_FLAG="--compress"; shift ;;
        --split-size) SPLIT_FLAG=(--split-size "$2"); shift 2 ;;
        -h|--help) usage ;;
        *) echo "unknown arg: $1" >&2; usage ;;
    esac
done

[[ -z "$SOURCE" || -z "$WORKDIR" || -z "$LABEL" ]] && usage
[[ -d "$SOURCE" ]] || { echo "source does not exist: $SOURCE" >&2; exit 2; }

# default overwritavle args
GAPPED="${GAPPED:-./target/release/gapped}"
RESULTS="${RESULTS:-./bench-results.csv}"

[[ -x "$GAPPED" ]] || { echo "gapped binary not found at $GAPPED (try: cargo build --release)" >&2; exit 2; }

# fresh workdir per run
mkdir -p "$WORKDIR"
rm -rf "$WORKDIR"/snap1 "$WORKDIR"/snap2 "$WORKDIR"/target "$WORKDIR"/diff.gapped*

SNAP1="$WORKDIR/snap1"
SNAP2="$WORKDIR/snap2"
DIFF_OUT="$WORKDIR/diff.gapped"
TARGET="$WORKDIR/target"

# measure source shape (pre-modifications)
BYTES=$(du -sb "$SOURCE" | awk '{print $1}')
FILES=$(find "$SOURCE" -type f | wc -l)

# CSV header
if [[ ! -f "$RESULTS" ]]; then
    echo "label,phase,bytes,files,wall_s,user_s,sys_s,cpu_pct,peak_rss_kb,minor_faults,major_faults,vol_ctx,inv_ctx,fs_in,fs_out" > "$RESULTS"
fi

drop_caches() {
    sync
    if [[ "${DROP_CACHES:-0}" == "1" ]]; then
        # requires passwordless sudo, enforces cold cache upon ecah run
        sudo sh -c 'echo 3 > /proc/sys/vm/drop_caches' 2>/dev/null || true
    fi
}

# run one phase under /usr/bin/time -v, parse output, emit CSV row.
run_phase() {
    local phase="$1"; shift
    local timef
    timef=$(mktemp)
    # enforce cold cache
    drop_caches
    if ! /usr/bin/time -v -o "$timef" "$@" >/dev/null 2>"$WORKDIR/stderr.log"; then
        echo "PHASE FAILED: $phase" >&2
        cat "$timef" >&2
        cat "$WORKDIR/stderr.log" >&2
        rm -f "$timef"
        exit 1
    fi

    local wall user sys cpu rss minf majf vctx ictx fsi fso
    wall=$(awk -F': ' '/Elapsed/ {print $NF}' "$timef" | awk -F: '{
        if (NF == 3) print $1*3600 + $2*60 + $3
        else if (NF == 2) print $1*60 + $2
        else print $1 }')
    user=$(awk -F': ' '/User time/ {print $NF}' "$timef")
    sys=$(awk -F': ' '/System time/ {print $NF}' "$timef")
    cpu=$(awk -F': ' '/Percent of CPU/ {gsub("%",""); print $NF}' "$timef")
    rss=$(awk -F': ' '/Maximum resident/ {print $NF}' "$timef")
    minf=$(awk -F': ' '/Minor \(reclaiming/ {print $NF}' "$timef")
    majf=$(awk -F': ' '/Major \(requiring/ {print $NF}' "$timef")
    vctx=$(awk -F': ' '/Voluntary context/ {print $NF}' "$timef")
    ictx=$(awk -F': ' '/Involuntary context/ {print $NF}' "$timef")
    fsi=$(awk -F': ' '/File system inputs/ {print $NF}' "$timef")
    fso=$(awk -F': ' '/File system outputs/ {print $NF}' "$timef")

    echo "$LABEL,$phase,$BYTES,$FILES,$wall,$user,$sys,$cpu,$rss,$minf,$majf,$vctx,$ictx,$fsi,$fso" \
        >> "$RESULTS"
    echo "  [$phase] wall=${wall}s rss=${rss}KB cpu=${cpu}%" >&2
    rm -f "$timef"
}

echo "=== run_bench: $LABEL ($BYTES bytes, $FILES files) ===" >&2

# --- snapshot ---
run_phase snapshot1 "$GAPPED" snapshot $COMPRESS_FLAG "$SOURCE" "$SNAP1"

# --- setup: target = copy of source ---
echo "  [setup] copying source -> target" >&2
cp -a "$SOURCE" "$TARGET"

# --- mutate source to simulate drift between snapshots ---
echo "  [setup] modifying $CHANGE_RATE of files in source" >&2
# HEREDOC instad of separate py file
python3 - "$SOURCE" "$CHANGE_RATE" <<'PY'
import os, random, sys
from pathlib import Path
src = Path(sys.argv[1])
rate = float(sys.argv[2])
rng = random.Random(42)
files = [p for p in src.rglob("*") if p.is_file()]
n = max(1, int(len(files) * rate))
picks = rng.sample(files, n)
blob = os.urandom(4 * 1024 * 1024)
for p in picks:
    size = p.stat().st_size
    with open(p, "wb") as f:
        remaining = size
        while remaining > 0:
            chunk = min(remaining, len(blob))
            f.write(blob[:chunk])
            remaining -= chunk
print(f"  modified {n} files", file=sys.stderr)
PY

# --- diff ---
run_phase diff "$GAPPED" diff $COMPRESS_FLAG "${SPLIT_FLAG[@]}" \
    "$SOURCE" "$SNAP1" "$DIFF_OUT" "$SNAP2"

# --- apply ---
run_phase apply "$GAPPED" apply "$TARGET" "$DIFF_OUT"

# --- verify ---
run_phase verify "$GAPPED" verify "$TARGET" "$DIFF_OUT" "$SNAP2"

echo "=== done: $LABEL ===" >&2
