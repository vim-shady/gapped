#!/usr/bin/env python3

"""
Plot benchmark results produced by run_bench.sh / experiment.sh.

Reads the CSV written to ./bench-results.csv (or the path given via --input)
and produces four figures:

  1. Wall-clock time per phase vs dataset size
  2. Throughput (MiB/s) per phase vs dataset size
  3. Peak RSS per phase vs dataset size
  4. CPU utilisation per phase vs dataset size

Usage:
    bench/plot.py                              # defaults
    bench/plot.py --input ./bench-results.csv --out-dir ./plots
"""

import argparse
import csv
import sys
from collections import defaultdict
from pathlib import Path

try:
    import matplotlib
    matplotlib.use("Agg")
    import matplotlib.pyplot as plt
    import matplotlib.ticker as ticker
except ImportError:
    print("error: matplotlib is required  (pip install matplotlib)", file=sys.stderr)
    sys.exit(1)

GiB = 1024 ** 3
MiB = 1024 ** 2

PHASE_ORDER = ["snapshot1", "diff", "apply", "verify"]
PHASE_COLORS = {
    "snapshot1": "#1f77b4",
    "diff":      "#ff7f0e",
    "apply":     "#2ca02c",
    "verify":    "#d62728",
}


def parse_csv(path: Path) -> list[dict]:
    with open(path) as f:
        return list(csv.DictReader(f))


def group_by_label(rows: list[dict]) -> dict[str, list[dict]]:
    groups: dict[str, list[dict]] = defaultdict(list)
    for row in rows:
        groups[row["label"]].append(row)
    return groups


def sort_key(label: str) -> float:
    """Extract byte count from first row with this label for sorting."""
    return 0.0  # caller overrides


def size_label(b: float) -> str:
    if b >= GiB:
        return f"{b / GiB:.0f} GiB"
    return f"{b / MiB:.0f} MiB"


def plot_metric(
    ax: plt.Axes,
    labels: list[str],
    sizes: list[float],
    phase_values: dict[str, list[float]],
    ylabel: str,
    title: str,
    fmt_func=None,
):
    x = range(len(labels))
    width = 0.18
    offsets = {p: (i - 1.5) * width for i, p in enumerate(PHASE_ORDER)}

    for phase in PHASE_ORDER:
        vals = phase_values.get(phase)
        if not vals:
            continue
        positions = [xi + offsets[phase] for xi in x]
        ax.bar(positions, vals, width, label=phase, color=PHASE_COLORS[phase])

    ax.set_xticks(list(x))
    ax.set_xticklabels([size_label(s) for s in sizes])
    ax.set_xlabel("Dataset size")
    ax.set_ylabel(ylabel)
    ax.set_title(title)
    ax.legend(fontsize="small")
    if fmt_func:
        ax.yaxis.set_major_formatter(ticker.FuncFormatter(fmt_func))


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.strip().splitlines()[0])
    ap.add_argument("--input", type=Path, default=Path("./bench-results.csv"))
    ap.add_argument("--out-dir", type=Path, default=Path("./plots"))
    args = ap.parse_args()

    if not args.input.exists():
        print(f"error: {args.input} not found", file=sys.stderr)
        return 1

    rows = parse_csv(args.input)
    if not rows:
        print("error: CSV is empty", file=sys.stderr)
        return 1

    groups = group_by_label(rows)

    # srt labels by dataset byte count (ascending).
    label_size: dict[str, float] = {}
    for label, label_rows in groups.items():
        label_size[label] = float(label_rows[0]["bytes"])
    sorted_labels = sorted(groups.keys(), key=lambda l: label_size[l])
    sizes = [label_size[l] for l in sorted_labels]

    # collect per-phase metric vectors aligned with sorted_labels.
    def collect(metric: str) -> dict[str, list[float]]:
        out: dict[str, list[float]] = defaultdict(list)
        for label in sorted_labels:
            by_phase = {r["phase"]: r for r in groups[label]}
            for phase in PHASE_ORDER:
                row = by_phase.get(phase)
                out[phase].append(float(row[metric]) if row else 0.0)
        return out

    wall = collect("wall_s")
    rss = collect("peak_rss_kb")
    cpu = collect("cpu_pct")

    # throughput = bytes / wall_s  (MiB/s).
    throughput: dict[str, list[float]] = defaultdict(list)
    for phase in PHASE_ORDER:
        for i, label in enumerate(sorted_labels):
            w = wall[phase][i]
            throughput[phase].append(sizes[i] / MiB / w if w > 0 else 0.0)

    # KiB to MiB for readability.
    for phase in rss:
        rss[phase] = [v / 1024 for v in rss[phase]]

    args.out_dir.mkdir(parents=True, exist_ok=True)

    # --- wall time ---
    fig, ax = plt.subplots(figsize=(8, 5))
    plot_metric(ax, sorted_labels, sizes, wall, "Seconds", "Wall-clock time per phase")
    fig.tight_layout()
    fig.savefig(args.out_dir / "wall_time.png", dpi=150)
    plt.close(fig)

    # --- throughput ---
    fig, ax = plt.subplots(figsize=(8, 5))
    plot_metric(ax, sorted_labels, sizes, throughput, "MiB/s", "Throughput per phase")
    fig.tight_layout()
    fig.savefig(args.out_dir / "throughput.png", dpi=150)
    plt.close(fig)

    # --- peak RSS ---
    fig, ax = plt.subplots(figsize=(8, 5))
    plot_metric(ax, sorted_labels, sizes, rss, "MiB", "Peak RSS per phase")
    fig.tight_layout()
    fig.savefig(args.out_dir / "peak_rss.png", dpi=150)
    plt.close(fig)

    # --- CPU utilisation ---
    fig, ax = plt.subplots(figsize=(8, 5))
    plot_metric(ax, sorted_labels, sizes, cpu, "%", "CPU utilisation per phase")
    fig.tight_layout()
    fig.savefig(args.out_dir / "cpu_util.png", dpi=150)
    plt.close(fig)

    print(f"Wrote 4 plots to {args.out_dir}/", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
