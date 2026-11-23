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
fixture="$work/fixture"
modes=(full priority)
failure='test z_value ... FAILED'
if [[ -n ${2:-} ]]; then
    git -C "$2" archive a8e7406af9d9483fb06506895b6639eb31107c8f rama-error/src | tar -x -C "$work"
    fixture="$work/rama-error"
    cat >"$fixture/Cargo.toml" <<'EOF'
[package]
name = "rama-error"
version = "0.5.0"
edition = "2024"
[features]
default = ["std"]
std = []
[profile.dev]
debug = false
opt-level = 1
EOF
    original="$work/context.rs"
    cp "$fixture/src/ext/context.rs" "$original"
    edit() {
        if [[ $1 == 0 ]]; then
            cp "$original" "$fixture/src/ext/context.rs"
        else
            sed 's/if idx == MAX_CAUSE_DEPTH/if idx != MAX_CAUSE_DEPTH/' "$original" >"$fixture/src/ext/context.rs"
        fi
    }
    modes+=(instance)
    failure='test ext::context::tests::cause_chain_truncation_marker_requires_an_omitted_cause ... FAILED'
else
    mkdir -p "$fixture/src"
    cat >"$fixture/Cargo.toml" <<'EOF'
[package]
name = "latency"
version = "0.1.0"
edition = "2024"
EOF
    edit() {
        cat >"$fixture/src/lib.rs" <<EOF
pub fn value(n: usize) -> usize { n + $1 }
#[test] fn a_unrelated() { std::thread::sleep(std::time::Duration::from_millis(300)); }
#[test] fn z_value() { assert_eq!(value(3),3); }
EOF
    }
fi
for iteration in 1 2 3; do
    for mode in "${modes[@]}"; do
        args=(dev --mir-rustc "$rustc" --manifest-path "$fixture/Cargo.toml" --lib)
        if [[ $mode == instance ]]; then args+=(--instance-codegen); fi
        edit 0
        "$runner" "${args[@]}" --full >"$work/control.log" 2>&1 || { cat "$work/control.log"; exit 1; }
        edit 1
        if [[ $mode == full ]]; then args+=(--full); fi
        status=0
        "$runner" "${args[@]}" >"$work/changed.log" 2>&1 || status=$?
        if [[ $status != 1 ]] || ! grep -Fq "$failure" "$work/changed.log"; then
            cat "$work/changed.log"
            exit 1
        fi
        printf '%s %s ' "$iteration" "$mode"
        grep 'First failure in' "$work/changed.log"
    done
done
