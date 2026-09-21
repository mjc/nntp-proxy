#!/usr/bin/env bash
# Compile the real private capability contracts and verify that the invalid
# programs fail for the intended ownership or coordinate rule.
set -euo pipefail

cd "$(dirname "$0")/.."
log_dir=target/response-contracts
mkdir -p "$log_dir"

cargo rustc --lib -- --cfg response_contract --emit=metadata >"$log_dir/positive.log" 2>&1
echo "PASS: positive controls"

check_failure() {
    local name=$1
    local code=$2
    local needle=$3
    shift 3
    if cargo rustc --lib -- --cfg response_contract --cfg "response_contract=\"$name\"" --emit=metadata \
        >"$log_dir/$name.log" 2>&1; then
        echo "FAIL: $name unexpectedly compiled"
        exit 1
    fi
    test "$(rg -o "error\\[E[0-9]+\\]" "$log_dir/$name.log" | sort -u)" = "error[$code]"
    rg -q "$needle" "$log_dir/$name.log"
    echo "PASS: $name rejected with $code"
}

check_failure append_twice E0382 permit
check_failure append_alias E0499 buffer
check_failure response_twice E0502 response
check_failure classified_buffer_reuse E0382 buffer
check_failure chunk_coordinate E0308 ChunkConsumed
check_failure exchange_constructor E0624 'associated function `new` is private'
check_failure validated_view_mutation E0502 'cannot borrow `bytes` as mutable'
check_failure validated_storage_reuse E0505 'cannot move out of `article` because it is borrowed'
check_failure article_layout_rebind E0451 private
check_failure availability_coordinate E0308 GlobalBlockIndex
