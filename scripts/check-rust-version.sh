#!/usr/bin/env bash
set -euo pipefail

toolchain="$(
    sed -n 's/^channel = "\(.*\)"/\1/p' rust-toolchain.toml
)"

if [[ -z "$toolchain" ]]; then
    echo "Could not read channel from rust-toolchain.toml" >&2
    exit 1
fi

cargo_msrv="$(
    sed -n 's/^rust-version = "\(.*\)"/\1/p' Cargo.toml
)"

if [[ -z "$cargo_msrv" ]]; then
    echo "Could not read rust-version from Cargo.toml" >&2
    exit 1
fi

lowest="$(
    printf '%s\n%s\n' "$toolchain" "$cargo_msrv" | sort -V | head -n1
)"

if [[ "$lowest" != "$cargo_msrv" ]]; then
    echo "Cargo.toml rust-version ($cargo_msrv) cannot exceed rust-toolchain.toml channel ($toolchain)" >&2
    exit 1
fi

echo "Rust version pins are consistent: rust-toolchain.toml=$toolchain, Cargo.toml rust-version=$cargo_msrv"
