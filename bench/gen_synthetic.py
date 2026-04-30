#!/usr/bin/env python3

"""
Generate an apt-like synthetic tree for benchmarking.

Produces a pool/main/<L>/<pkg>/<pkg>_<ver>_amd64.deb + dists/ layout that
roughly mirrors the shape of a (Debian) apt mirror. File sizes are sampled
from a tri-modal distribution (small metadata, medium .debs, rare large
.debs). Content is pseudo-random and non-compressible, so zstd comparisons
still mean something.

Example:
    bench/gen_synthetic.py --out ./bench-data/synth-1g \\
        --target-bytes 1G --target-files 500
"""

import argparse
import os
import random
import sys
from pathlib import Path

KB = 1024
MB = 1024 * KB
GB = 1024 * MB


def parse_size(s: str) -> int:
    s = s.strip().upper()
    mult = 1
    if s.endswith("G"):
        mult, s = GB, s[:-1]
    elif s.endswith("M"):
        mult, s = MB, s[:-1]
    elif s.endswith("K"):
        mult, s = KB, s[:-1]
    return int(float(s) * mult)


def sample_sizes(rng: random.Random, n: int, total_bytes: int) -> list[int]:
    """Sample n file sizes summing to ~total_bytes.

    Distribution):
      - 20%: 1-10 KiB (metadata-ish)
      - 70%: 100 KiB - 5 MiB (typical .deb)
      - 10%: 10-100 MiB (rare big packages, kernels, etc.)
    Sizes are then rescaled so the sum matches target_bytes exactly.
    """
    raw = []
    for _ in range(n):
        r = rng.random()
        if r < 0.20:
            raw.append(rng.randint(1 * KB, 10 * KB))
        elif r < 0.90:
            raw.append(rng.randint(100 * KB, 5 * MB))
        else:
            raw.append(rng.randint(10 * MB, 100 * MB))
    actual = sum(raw)
    if actual == 0:
        return raw
    scale = total_bytes / actual
    return [max(1, int(s * scale)) for s in raw]


def fill_file(path: Path, size: int, blob: bytes, rng: random.Random) -> None:
    """Write `size` bytes of non-compressible content to `path`.

    Reuse a single random blob to avoid paying /dev/urandom latency per
    file. Small files get a random slice; large files tile the blob with a
    per-file prefix so content differs between files
    """
    with open(path, "wb") as f:
        if size <= len(blob):
            start = rng.randrange(0, len(blob) - size + 1)
            f.write(blob[start : start + size])
            return
        # Unique prefix → ensures distinct content per file
        prefix = rng.randbytes(min(64, size))
        f.write(prefix)
        remaining = size - len(prefix)
        while remaining > 0:
            chunk = min(remaining, len(blob))
            f.write(blob[:chunk])
            remaining -= chunk


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[1] if __doc__ else "")
    ap.add_argument("--out", required=True, type=Path)
    ap.add_argument("--target-bytes", required=True, type=parse_size,
                    help="Total size (suffixes: K/M/G)")
    ap.add_argument("--target-files", required=True, type=int)
    ap.add_argument("--seed", type=int, default=1)
    args = ap.parse_args()

    if args.out.exists() and any(args.out.iterdir()):
        print(f"error: {args.out} already exists and is non-empty", file=sys.stderr)
        return 2

    rng = random.Random(args.seed)
    sizes = sample_sizes(rng, args.target_files, args.target_bytes)

    blob = os.urandom(16 * MB)

    pool = args.out / "pool" / "main"
    pool.mkdir(parents=True, exist_ok=True)

    report_every = max(1, args.target_files // 20)
    for i, size in enumerate(sizes):
        letter = chr(ord("a") + (i % 26))
        pkg = f"pkg{i // 26:05d}"
        pkg_dir = pool / letter / pkg
        pkg_dir.mkdir(parents=True, exist_ok=True)
        path = pkg_dir / f"{pkg}_1.0-{i}_amd64.deb"
        fill_file(path, size, blob, rng)
        if (i + 1) % report_every == 0:
            done_bytes = sum(sizes[: i + 1])
            print(
                f"  [{i + 1}/{args.target_files}] "
                f"{done_bytes / GB:.2f} GiB written",
                file=sys.stderr,
            )

    # dists/<suite>/<component>/binary-amd64 metadata. Small, just for shape.
    dists = args.out / "dists" / "bookworm" / "main" / "binary-amd64"
    dists.mkdir(parents=True, exist_ok=True)
    (dists / "Packages").write_bytes(os.urandom(64 * KB))
    (dists / "Packages.gz").write_bytes(os.urandom(16 * KB))
    (dists / "Packages.xz").write_bytes(os.urandom(12 * KB))
    (dists / "Release").write_bytes(os.urandom(4 * KB))
    (args.out / "dists" / "bookworm" / "Release").write_bytes(os.urandom(8 * KB))
    (args.out / "dists" / "bookworm" / "InRelease").write_bytes(os.urandom(10 * KB))

    total = sum(p.stat().st_size for p in args.out.rglob("*") if p.is_file())
    count = sum(1 for p in args.out.rglob("*") if p.is_file())
    print(
        f"Generated {count} files, {total / GB:.3f} GiB into {args.out}",
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
