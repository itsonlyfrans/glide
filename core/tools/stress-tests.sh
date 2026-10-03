#!/usr/bin/env bash
# Usage: stress-tests.sh [filter] [iterations] [--workspace] [--test-threads=N]
set -euo pipefail
core=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
repo=$(cd -- "$core/.." && pwd -P)
filter=${1:-}
iterations=${2:-30}
[[ $iterations =~ ^[1-9][0-9]*$ && $iterations -le 10000 ]] || { echo 'Iterations must be 1..10000.' >&2; exit 2; }
shift "$(( $# < 2 ? $# : 2 ))"
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-${TMPDIR:-/tmp}/glide-stress-target}
[[ $CARGO_TARGET_DIR = /* ]] || { echo 'CARGO_TARGET_DIR must be absolute and outside the repository.' >&2; exit 2; }
# Resolve existing ancestors (including symlinks) before creating any target directory.
ancestor=$CARGO_TARGET_DIR
suffix=
while [[ ! -d $ancestor ]]; do
    suffix="/$(basename -- "$ancestor")$suffix"
    ancestor=$(dirname -- "$ancestor")
done
CARGO_TARGET_DIR="$(cd -- "$ancestor" && pwd -P)$suffix"
case "$CARGO_TARGET_DIR/" in "$repo/"*) echo 'CARGO_TARGET_DIR must be outside the repository.' >&2; exit 2;; esac
export CARGO_TARGET_DIR
[[ -d $CARGO_TARGET_DIR ]] || mkdir -p -- "$CARGO_TARGET_DIR"
logs=$(mktemp -d "$CARGO_TARGET_DIR/stress-XXXXXXXX")
selection=(-p glide-daemon --lib)
threads=()
for option in "$@"; do
    case $option in
        --workspace) selection=(--workspace);;
        --test-threads=*) threads+=("$option");;
        *) echo "Unknown option: $option" >&2; exit 2;;
    esac
done
args=(test --offline --locked "${selection[@]}")
[[ -z $filter ]] || args+=("$filter")
args+=(-- --nocapture "${threads[@]}")
passed=0
failed=0
cd -- "$core"
for ((run=1; run<=iterations; run++)); do
    code=0
    cargo "${args[@]}" >"$logs/$run.log" 2>&1 || code=$?
    if [[ $code -eq 0 ]] && grep -Eq 'test result: ok\. [1-9][0-9]* passed' "$logs/$run.log"; then
        passed=$((passed+1))
        echo "$run/$iterations PASS"
    else
        failed=$((failed+1))
        echo "$run/$iterations FAIL (exit $code; $logs/$run.log)"
        tail -n 30 "$logs/$run.log"
    fi
done
echo "Tally: $passed passed, $failed failed, $iterations runs. Logs: $logs"
[[ $failed -eq 0 ]]
