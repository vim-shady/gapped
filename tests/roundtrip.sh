#!/usr/bin/env bash
set -euo pipefail

# Build the release binary
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
cargo build --release --manifest-path "$PROJECT_DIR/Cargo.toml" --quiet
GAPPED="$PROJECT_DIR/target/release/gapped"

# setup tmp dir in /tmp
WORK=$(mktemp -d)
# cleanup trap
trap 'rm -rf "$WORK"' EXIT

SRC="$WORK/source"
DST="$WORK/target"
mkdir -p "$SRC" "$DST"

pass=0
fail=0

# using rsync as oracle
check_rsync() {
    local test_name="$1"
    local diff_output
    diff_output=$(rsync -acvn --delete "$SRC/" "$DST/" 2>&1)
    local file_lines
    # filter out irrelevant rsync report lines
    file_lines=$(echo "$diff_output" | grep -v '^sending\|^$\|^sent\|^total' || true)
    # no changes reporteed
    if [ -z "$file_lines" ]; then
        echo "  PASS: $test_name"
        pass=$((pass + 1))
    else
        echo "  FAIL: $test_name"
        echo "  rsync found differences:"
        echo "$diff_output" | sed 's/^/    /'
        fail=$((fail + 1))
    fi
}

run_cycle() {
    local snap_in="$1"
    local diff_out="$2"
    local snap_out="$3"

    $GAPPED diff "$SRC" "$snap_in" "$diff_out" "$snap_out" 2>/dev/null
    $GAPPED apply "$DST" "$diff_out" 2>/dev/null
}

check_verify() {
    local test_name="$1"
    local diff_file="$2"
    local snap_file="$3"
    if $GAPPED verify "$DST" "$diff_file" "$snap_file" 2>/dev/null; then
        echo "  PASS: $test_name"
        pass=$((pass + 1))
    else
        echo "  FAIL: $test_name (verify should have succeeded)"
        fail=$((fail + 1))
    fi
}

# Verify should fail (non-zero exit) after tampering
check_verify_fails() {
    local test_name="$1"
    local diff_file="$2"
    local snap_file="$3"
    if $GAPPED verify "$DST" "$diff_file" "$snap_file" 2>/dev/null; then
        echo "  FAIL: $test_name (verify should have failed but succeeded)"
        fail=$((fail + 1))
    else
        echo "  PASS: $test_name"
        pass=$((pass + 1))
    fi
}

# --- Setup: initial state ---
mkdir -p "$SRC/subdir"
echo "hello world" > "$SRC/file1.txt"
echo "foo bar" > "$SRC/file2.txt"
echo "nested" > "$SRC/subdir/deep.txt"
ln -s file1.txt "$SRC/link1"
chmod 755 "$SRC/file2.txt"

$GAPPED snapshot "$SRC" "$WORK/snap1.json" 2>/dev/null
rsync -a "$SRC/" "$DST/"

echo "Test 1: Baseline after rsync"
check_rsync "baseline after rsync"

# --- Test 2: Modify file content ---
echo "Test 2: Modify file content"
echo "modified content" > "$SRC/file1.txt"
run_cycle "$WORK/snap1.json" "$WORK/diff1.json" "$WORK/snap2.json"
check_rsync "file content modification"

# --- Test 3: Add new files and directories ---
echo "Test 3: Add new files and directories"
echo "brand new" > "$SRC/newfile.txt"
mkdir -p "$SRC/newdir"
echo "inner" > "$SRC/newdir/inner.txt"
run_cycle "$WORK/snap2.json" "$WORK/diff2.json" "$WORK/snap3.json"
check_rsync "add files and directories"

# --- Test 4: Delete file ---
echo "Test 4: Delete file"
rm "$SRC/file2.txt"
run_cycle "$WORK/snap3.json" "$WORK/diff3.json" "$WORK/snap4.json"
check_rsync "delete file"

# --- Test 5: Delete directory recursively ---
echo "Test 5: Delete directory recursively"
rm -rf "$SRC/subdir"
run_cycle "$WORK/snap4.json" "$WORK/diff4.json" "$WORK/snap5.json"
check_rsync "delete directory"

# --- Test 6: Symlink target change ---
echo "Test 6: Symlink target change"
rm "$SRC/link1"
ln -s newfile.txt "$SRC/link1"
run_cycle "$WORK/snap5.json" "$WORK/diff5.json" "$WORK/snap6.json"
check_rsync "symlink target change"

# --- Test 7: Permission-only change ---
echo "Test 7: Permission-only change"
chmod 600 "$SRC/file1.txt"
run_cycle "$WORK/snap6.json" "$WORK/diff6.json" "$WORK/snap7.json"
check_rsync "permission change"

# --- Test 8: Kind change (file -> symlink) ---
echo "Test 8: Kind change (file -> symlink)"
rm "$SRC/newfile.txt"
ln -s file1.txt "$SRC/newfile.txt"
run_cycle "$WORK/snap7.json" "$WORK/diff7.json" "$WORK/snap8.json"
check_rsync "kind change file to symlink"

# --- Test 9: Deep nesting ---
echo "Test 9: Deep nesting"
mkdir -p "$SRC/a/b/c/d/e"
echo "deep" > "$SRC/a/b/c/d/e/leaf.txt"
run_cycle "$WORK/snap8.json" "$WORK/diff8.json" "$WORK/snap9.json"
check_rsync "deep nesting"

# --- Test 10: No changes (empty diff) ---
echo "Test 10: No changes"
run_cycle "$WORK/snap9.json" "$WORK/diff9.json" "$WORK/snap10.json"
check_rsync "no changes"

# --- Test 11: Verify succeeds when target matches ---
echo "Test 11: Verify succeeds when target matches"
# After test 10, DST is in sync. diff9 is empty, snap10 is the current state.
check_verify "verify passes on clean target" "$WORK/diff9.json" "$WORK/snap10.json"

# --- Test 12: Verify detects tampered file content ---
echo "Test 12: Verify detects tampered file content"
echo "tampered" > "$DST/file1.txt"
check_verify_fails "verify detects tampered content" "$WORK/diff9.json" "$WORK/snap10.json"
rsync -a "$SRC/" "$DST/"

# --- Test 13: Verify detects extra file ---
echo "Test 13: Verify detects extra file on target"
echo "extra" > "$DST/extra.txt"
check_verify_fails "verify detects extra file" "$WORK/diff9.json" "$WORK/snap10.json"
rsync -a --delete "$SRC/" "$DST/"

# --- Test 13: Verify detects missing file ---
echo "Test 14: Verify detects missing file on target"
rm "$DST/file1.txt"
check_verify_fails "verify detects missing file" "$WORK/diff9.json" "$WORK/snap10.json"
rsync -a "$SRC/" "$DST/"

# --- Summary ---
echo ""
echo "Results: $pass passed, $fail failed"
if [ "$fail" -gt 0 ]; then
    exit 1
fi
