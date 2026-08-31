#!/usr/bin/env bash
# tools/quality-gate.sh: コミット前に通す。引数は比較元のコミット（既定 origin/main か初回 commit）
set -euo pipefail
cd "$(dirname "$0")/.."
base="${1:-$(git merge-base HEAD main 2>/dev/null || git rev-list --max-parents=0 HEAD)}"
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo llvm-cov --lib --tests --summary-only --fail-under-lines 80
cargo mutants --in-diff <(git diff "$base"..HEAD -- src/) --no-shuffle --timeout 120
bash tools/crap.sh
