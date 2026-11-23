#!/usr/bin/env bash
# Run from the cargo-online checkout after cargo build --release.
set -euo pipefail
rustc=$(realpath "${1:?pass the pinned rustc executable}")
runner=$(realpath target/release/cargo-online)
mkdir -p .scratch
work=$(mktemp -d "$PWD/.scratch/edit-latency-XXXXXXXX")
trap 'rm -rf -- "$work"' EXIT
export TMPDIR="$work" CARGO_NET_OFFLINE=true RUST_TEST_THREADS=1
unset CARGO_TARGET_DIR
mkdir -p "$work/fixture/src"
cat >"$work/fixture/Cargo.toml" <<'EOF'
[package]
name = "latency"
version = "0.1.0"
edition = "2024"
EOF
edit() {
    cat >"$work/fixture/src/lib.rs" <<EOF
pub fn value(n: usize) -> usize { n + $1 }
#[test] fn a_unrelated() { std::thread::sleep(std::time::Duration::from_millis(300)); }
#[test] fn z_value() { assert_eq!(value(3),3); }
EOF
}
for iteration in 1 2 3; do
    for mode in full priority; do
        args=(dev --mir-rustc "$rustc" --manifest-path "$work/fixture/Cargo.toml" --lib)
        edit 0
        "$runner" "${args[@]}" --full >"$work/control.log" 2>&1 || { cat "$work/control.log"; exit 1; }
        edit 1
        if [[ $mode == full ]]; then args+=(--full); fi
        status=0
        "$runner" "${args[@]}" >"$work/changed.log" 2>&1 || status=$?
        if [[ $status != 1 ]] || ! grep -q 'test z_value ... FAILED' "$work/changed.log"; then
            cat "$work/changed.log"
            exit 1
        fi
        printf '%s %s ' "$iteration" "$mode"
        grep 'First failure in' "$work/changed.log"
    done
done
