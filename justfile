# Common operations for zero-elo. Run `just` on its own to see them all.
#
# Anything that actually searches runs in the optimized profile: a debug build
# of this engine is slow enough to change what you conclude from watching it.

default:
    @just --list

# --- checking -------------------------------------------------------------

# Everything that should pass before a commit
[group('check')]
check: fmt-check lint test

# Format the whole workspace
[group('check')]
fmt:
    cargo fmt --all

# Fail if anything is unformatted, without rewriting it
[group('check')]
fmt-check:
    cargo fmt --all --check

# Clippy over every target and feature, warnings fatal
[group('check')]
lint:
    cargo clippy --workspace --all-targets --all-features -- -D warnings

# Advisory only: most of what it finds here is deliberate, so this pass is for
# reading, not for satisfying.
[doc('Clippy pedantic, as advice rather than a gate')]
[group('check')]
pedantic:
    cargo clippy --workspace --all-targets -- -W clippy::pedantic

# Run the tests. `just test it_loses` filters, as cargo does.
[group('check')]
test *ARGS:
    cargo test --release --workspace {{ ARGS }}

# Build the docs as docs.rs will render them, and open them
[group('check')]
doc:
    cargo doc --workspace --no-deps --open

# --- running --------------------------------------------------------------
#
# Every recipe here forwards extra arguments through, so the engine's own
# options work unchanged: `just analyse --depth 8 --model paranoid`.

# Speak UCI on stdin and stdout, as a GUI would drive it
[group('run')]
uci *ARGS:
    cargo run --release -q -p zero-elo-cli -- uci {{ ARGS }}

# Search a position and print each iteration
[group('run')]
analyse *ARGS:
    cargo run --release -q -p zero-elo-cli -- analyse {{ ARGS }}

# Play against it in the terminal
[group('run')]
play *ARGS:
    cargo run --release -q -p zero-elo-cli -- play {{ ARGS }}

# Watch it lose a full game to an ordinary engine
[group('run')]
selfplay *ARGS:
    cargo run --release -q -p zero-elo-cli -- selfplay {{ ARGS }}

# Count leaf nodes, to check move generation
[group('run')]
perft depth="5" *ARGS:
    cargo run --release -q -p zero-elo-cli -- perft {{ depth }} {{ ARGS }}

# Search a fixed set of positions and report the speed
[group('run')]
bench *ARGS:
    cargo run --release -q -p zero-elo-cli -- bench {{ ARGS }}

# --- shipping -------------------------------------------------------------

# Optimized build; the binary lands in target/release/zero-elo
[group('dist')]
build:
    cargo build --release

# Install the binary into ~/.cargo/bin from this checkout
[group('dist')]
install:
    cargo install --path crates/zero-elo-cli --locked

# List exactly what each crate would upload to crates.io
[group('dist')]
package:
    @echo "--- zero-elo ---"
    @cargo package --list -p zero-elo
    @echo "--- zero-elo-cli ---"
    @cargo package --list -p zero-elo-cli

# Package and verify both crates without uploading anything
[group('dist')]
publish-dry:
    cargo publish --workspace --dry-run

# Upload both crates, in dependency order. Not reversible.
[group('dist')]
publish: check
    cargo publish --workspace

# Checks the tag against the manifest first, so a mismatch fails here in a
# second rather than on CI after five builds have run.
[doc('Tag a version and push it, building the release binaries on CI')]
[group('dist')]
tag version:
    #!/usr/bin/env bash
    set -euo pipefail
    manifest=$(cargo metadata --format-version 1 --no-deps \
      | jq -r '.packages[] | select(.name == "zero-elo-cli") | .version')
    if [ "{{ version }}" != "$manifest" ]; then
      echo "error: v{{ version }} does not match zero-elo-cli $manifest" >&2
      exit 1
    fi
    if [ -n "$(git status --porcelain)" ]; then
      echo "error: working tree is dirty; commit before tagging" >&2
      exit 1
    fi
    git tag -a "v{{ version }}" -m "v{{ version }}"
    git push origin "v{{ version }}"
    echo "pushed v{{ version }} -- watch it with: just release-status"

# Follow the release build currently running on CI
[group('dist')]
release-status:
    gh run watch $(gh run list --workflow=release.yml --limit 1 --json databaseId --jq '.[0].databaseId')

# Throw away everything built
[group('dist')]
clean:
    cargo clean
