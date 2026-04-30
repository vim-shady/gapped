# gapped

Offline file synchronizer for air-gapped systems.

`gapped` synchronizes directory trees between machines that have no direct network connection. It works by capturing filesystem state as snapshots, computing minimal diffs that bundle both metadata and file content, and applying those diffs on the target side. Diffs can be transported on USB drives or any other physical medium.

## Features

- **Streaming I/O** — file content is never fully materialized in memory, keeping RAM usage bounded even for large trees
- **Parallel hashing** — files are hashed in parallel using XXH3-128 for fast content comparison
- **Hash caching** — unchanged files (same size + mtime) reuse their previous hash, skipping redundant reads
- **Zstandard compression** — optional zstd compression for snapshots and diffs
- **Split diffs** — large diffs can be split into size-limited chunks for transport on capacity-constrained media
- **Integrity verification** — every file includes an XXH3-128 checksum; a dedicated `verify` command simulates apply without writing
- **Full metadata preservation** — permissions, ownership (uid/gid), modification times, and symlink targets

## Installation

### From source

Requires the [Rust toolchain](https://rustup.rs/) (edition 2024).

```bash
git clone <repo-url>
cd gapped
cargo build --release
```

The binary is at `./target/release/gapped`. Copy it to a location on your `PATH`:

```bash
cp target/release/gapped ~/.local/bin/
```

## Usage

`gapped` follows a four-step workflow: **snapshot**, **diff**, **(transfer)**, **apply**.

### 1. Create an initial snapshot (source machine)

Capture the current state of the directory you want to synchronize:

```bash
gapped snapshot /src/data snapshot_v1.gapped
```

Transfer `snapshot_v1.gapped` to the target machine and apply it there as the baseline (the very first sync requires a full copy via rsync or similar — the snapshot alone contains no file content).

### 2. Compute a diff (source machine)

After files have changed, compute the differences against the previous snapshot:

```bash
gapped diff /src/data snapshot_v1.gapped diff_v2.gapped snapshot_v2.gapped
```

This reads the current filesystem, compares it to `snapshot_v1.gapped`, and produces:
- `diff_v2.gapped` — the diff containing all changes and new file content
- `snapshot_v2.gapped` — an updated snapshot of the current state (used as input for the next diff)

#### Options

Compress the output:

```bash
gapped diff /src/data snapshot_v1.gapped diff_v2.gapped snapshot_v2.gapped --compress
```

Split the diff into chunks (useful when transporting on size-limited media):

```bash
gapped diff /src/data snapshot_v1.gapped diff_v2.gapped snapshot_v2.gapped --split-size 4GB
```

This produces `diff_v2_001.gapped`, `diff_v2_002.gapped`, etc. The `--split-size` flag accepts values like `100MB`, `500KB`, `2GB`, or a raw byte count.

### 3. Transfer

Copy the diff file(s) to the target machine using whatever physical medium is available (USB drive, optical disc, etc.).

### 4. Apply the diff (target machine)

```bash
gapped apply /target/data diff_v2.gapped
```

If the diff was split, pass any chunk — `gapped` auto-detects and applies all parts in order:

```bash
gapped apply /target/data diff_v2_001.gapped
```

### Verify before applying

Simulate the apply and compare the result against the expected snapshot, without modifying the filesystem:

```bash
gapped verify /target/data diff_v2.gapped snapshot_v2.gapped
```

### Incremental snapshots

When creating a new snapshot, pass the previous snapshot to reuse cached hashes for unchanged files:

```bash
gapped snapshot /src/data snapshot_v2.gapped snapshot_v1.gapped
```


## Logging

Control log verbosity with the `RUST_LOG` environment variable:

```bash
RUST_LOG=info gapped diff /srv/data s1.gapped d2.gapped s2.gapped
RUST_LOG=debug gapped apply /srv/data d2.gapped
```

Progress bars are shown when stderr is a terminal and hidden otherwise.

## Running tests

```bash
cargo test
```

Integration tests perform full snapshot-diff-apply roundtrips and verify correctness against rsync.

## License

[MIT](LICENSE)
