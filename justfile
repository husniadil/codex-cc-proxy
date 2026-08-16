# codex-cc-proxy task runner

# Format, lint, and test — the full local gate
default: check

# Everything CI enforces
check: fmt-check lint test

# Run the test suite
test:
    cargo nextest run --locked --status-level fail --final-status-level fail --failure-output final --success-output never

# Run one test filter, e.g. `just test-one incremental`
test-one filter:
    cargo nextest run --locked "{{filter}}" --status-level fail --final-status-level fail --failure-output final

# Lint with warnings as errors
lint:
    cargo clippy --all-targets --all-features --locked -- -D warnings

# Verify formatting
fmt-check:
    cargo fmt --all --check

# Apply formatting
fmt:
    cargo fmt --all

# Review pending snapshot changes
snapshots:
    cargo insta review

# Run the daemon locally with verbose logging
run *ARGS:
    RUST_LOG=codex_cc_proxy=debug cargo run -p codex-cc-proxy -- run {{ARGS}}

# Capture upstream exchanges as test fixtures
record *ARGS:
    cargo run -p codex-cc-proxy -- record {{ARGS}}

# Probe live backend capabilities — spends real inference quota
doctor *ARGS:
    cargo run -p codex-cc-proxy -- doctor {{ARGS}}

# Install development tooling
setup:
    mise install
    cargo install cargo-nextest --locked
    cargo install cargo-insta --locked
    git config core.hooksPath .githooks

# Build optimized binaries
build:
    cargo build --release --locked
