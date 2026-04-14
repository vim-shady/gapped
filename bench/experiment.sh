#!/usr/bin/env bash
# Sweep driver: generate synthetic trees at multiple scales and run the
# bench harness against each. Appends all results to one CSV.
# Generated trees are persisted on disk (for reuse).
#
# Usage:
#   cargo build --release
#   bench/experiment.sh                  # default scales
#   SCALES="1G:500 5G:2500" bench/experiment.sh
#
# Environment:
#   SCALES        space-separated "BYTES:FILES" specs
#   DATA_DIR      where to cache generated trees (default: ./bench-data)
#   WORK_DIR      scratch dir for per-run artifacts (default: ./bench-work)
#   RESULTS       CSV output (default: ./bench-results.csv)
#   CHANGE_RATE   fraction of files modified between snapshots (default: 0.05)
#   COMPRESS=1    enable --compress for snapshot/diff
#   SPLIT_SIZE    if set, pass --split-size
#   DROP_CACHES=1 attempt to drop page cache before each phase (needs sudo)

set -euo pipefail

cd "$(dirname "$0")/.."

SCALES="${SCALES:-1G:500 2G:1000 5G:2500 10G:5000}"
DATA_DIR="${DATA_DIR:-./bench-data}"
WORK_DIR="${WORK_DIR:-./bench-work}"
RESULTS="${RESULTS:-./bench-results.csv}"
CHANGE_RATE="${CHANGE_RATE:-0.05}"

mkdir -p "$DATA_DIR" "$WORK_DIR"

if [[ ! -x ./target/release/gapped ]]; then
    echo "building release binary..." >&2
    cargo build --release
fi

export GAPPED="./target/release/gapped"
export RESULTS

extra_args=()
[[ "${COMPRESS:-0}" == "1" ]] && extra_args+=(--compress)
[[ -n "${SPLIT_SIZE:-}" ]] && extra_args+=(--split-size "$SPLIT_SIZE")

for spec in $SCALES; do
    bytes="${spec%%:*}"
    files="${spec##*:}"
    label="synth-${bytes}-${files}f"
    src="$DATA_DIR/$label"

    if [[ ! -d "$src" ]]; then
        echo "=== generating $label ($bytes, $files files) ===" >&2
        python3 bench/gen_synthetic.py \
            --out "$src" \
            --target-bytes "$bytes" \
            --target-files "$files"
    else
        echo "=== reusing cached $label ===" >&2
    fi

    bench/run_bench.sh \
        --source "$src" \
        --workdir "$WORK_DIR" \
        --label "$label" \
        --change-rate "$CHANGE_RATE" \
        "${extra_args[@]}"
done

echo "=== sweep complete; results in $RESULTS ===" >&2
