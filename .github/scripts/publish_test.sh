#! /bin/bash
set -e

export TERM=xterm-256color
# The README is inlined as the crate docs, so a broken link there is a broken doc build.
export RUSTDOCFLAGS="-D warnings"

# Statements waiting to be executed
statements=(
    "cargo fmt --check"
    "cargo clippy --all-targets -- -D warnings"
    # `dummy` replaces the whole platform module rather than adding to it, so it is a
    # separate compilation of the crate, not another feature of the one above.
    "cargo clippy --all-targets --features dummy -- -D warnings"
    # Backends are cfg-gated on target_os, so a host build only ever compiles one of the
    # three. These two are the only way to catch breakage in a backend CI cannot run;
    # clippy checks without linking, which is all pure Rust bindings need. The workflow
    # installs both targets with `rustup target add`.
    "cargo clippy --all-targets --target x86_64-pc-windows-msvc -- -D warnings"
    "cargo clippy --all-targets --target aarch64-apple-darwin -- -D warnings"
    # `capture_test` needs a display unless the dummy backend is in, which is what makes
    # this one headless. It also runs the README doctests.
    "cargo test --features dummy"
    "cargo doc --no-deps"
    # --allow-dirty: CI stamps the release version into Cargo.toml without committing
    # it, so the working tree is meant to differ from the commit being packaged.
    "cargo publish --dry-run --allow-dirty"
)

# loop echo and executing statements
for statement in "${statements[@]}"; do
    echo "$(tput setaf 3)$statement$(tput sgr0)"
    eval $statement
    echo
done
