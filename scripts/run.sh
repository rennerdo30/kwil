#!/usr/bin/env bash
# Build and start Kwil from this checkout, to check the current state.
#
#   scripts/run.sh                 open README.md in the window UI
#   scripts/run.sh some/file.txt   open a file
#   scripts/run.sh --pull          update to the latest main first (fast-forward only)
#   scripts/run.sh --release       optimized build (slower to build, the real speed)
#   scripts/run.sh --big           open the 1 GB test log (generated on first use)
#   scripts/run.sh --tui           terminal UI instead of the window
#
# Other options (for example --version, --help) are passed to kwil.
set -euo pipefail
cd "$(dirname "$0")/.."

pull=0 release=() big=0 file="" kwil_args=()
for arg in "$@"; do
  case "$arg" in
    --pull) pull=1 ;;
    --release) release=(--release) ;;
    --big) big=1 ;;
    -*) kwil_args+=("$arg") ;;
    *) file="$arg" ;;
  esac
done

if [ "$pull" = 1 ]; then
  echo "Updating to the latest main..."
  git pull --ff-only origin main
fi

# Use half of the CPU threads so the machine stays responsive while building.
cpus=$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 2)
jobs=$(( cpus / 2 > 0 ? cpus / 2 : 1 ))

if [ "$big" = 1 ]; then
  if [ ! -f fixtures/log-1g.log ]; then
    echo "Generating the 1 GB test log once, about a minute..."
    mkdir -p fixtures
    cargo run --release -j "$jobs" --quiet --manifest-path spike/textbench/Cargo.toml -- gen fixtures/log-1g.log 1024
  fi
  file=fixtures/log-1g.log
fi
if [ -z "$file" ] && [ ${#kwil_args[@]} -eq 0 ]; then file=README.md; fi

echo "Building Kwil ${release[*]:-}..."
cargo build "${release[@]}" -j "$jobs" --quiet -p kwil

KWIL_NO_UPDATE=1 exec cargo run "${release[@]}" -j "$jobs" --quiet -p kwil --bin kwil -- "${kwil_args[@]}" ${file:+"$file"}
