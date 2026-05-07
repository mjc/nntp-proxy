#!/usr/bin/env bash
set -euo pipefail

toolchain="$(
    sed -n 's/^channel = "\(.*\)"/\1/p' rust-toolchain.toml
)"

if [[ -z "$toolchain" ]]; then
    echo "Could not read channel from rust-toolchain.toml" >&2
    exit 1
fi

msrv="${toolchain%.*}"
cargo_msrv="$(
    sed -n 's/^rust-version = "\(.*\)"/\1/p' Cargo.toml
)"

if [[ "$cargo_msrv" != "$msrv" ]]; then
    echo "Cargo.toml rust-version is $cargo_msrv, expected $msrv from rust-toolchain.toml ($toolchain)" >&2
    exit 1
fi

echo "Rust version pins are consistent: rust-toolchain.toml=$toolchain, Cargo.toml rust-version=$cargo_msrv"
