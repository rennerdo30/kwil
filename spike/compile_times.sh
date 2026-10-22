#!/usr/bin/env bash
# Usage: compile_times.sh <crate dir> <target dir> [cargo feature args...]
# Clean release, clean dev and incremental dev rebuild (after touching
# src/main.rs), all with -j 8. Prints one line per measurement.
set -u
dir=$1; tdir=$2; shift 2
cd "$dir" || exit 1
export CARGO_TARGET_DIR=$tdir
t() { local s e; s=$(date +%s.%N); timeout 1800 cargo build -q -j 8 "$@" >/dev/null 2>&1; local rc=$?; e=$(date +%s.%N); python -c "print('%.1f' % ($e-$s))"; return $rc; }
rm -rf "$tdir"
echo "clean_release_s $(t --release "$@")"
rm -rf "$tdir"
echo "clean_dev_s $(t "$@")"
touch src/main.rs
echo "incr_dev_s $(t "$@")"
touch src/main.rs
echo "incr_dev2_s $(t "$@")"
echo "crates $(cargo tree -e normal --prefix none "$@" 2>/dev/null | sed 's/ (\*)//' | sort -u | wc -l)"
exe=$(ls "$tdir"/debug/*.exe | head -1)
echo "dev_exe_bytes $(stat -c %s "$exe")"
