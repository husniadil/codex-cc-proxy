# proxenos task runner

# Format, lint, and test — the full local gate
default: check

# Everything CI enforces
check: fmt-check lint test

# Run the test suite. All features, so the estimator comparison in
# docs/proxy-behavior.md §6.3 is part of the gate rather than an aside.
test:
    cargo nextest run --locked --all-features --status-level fail --final-status-level fail --failure-output final --success-output never

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
    RUST_LOG=proxenos=debug cargo run -p proxenos -- run {{ARGS}}

# Capture upstream exchanges as test fixtures
record *ARGS:
    cargo run -p proxenos -- record {{ARGS}}

# Probe live backend capabilities — spends real inference quota
doctor *ARGS:
    cargo run -p proxenos -- doctor {{ARGS}}

# Install development tooling
setup:
    mise install
    # `.tool-versions` pins the version but cannot carry components, and
    # `just check` is mostly rustfmt and clippy.
    rustup component add rustfmt clippy
    cargo install cargo-nextest --locked
    cargo install cargo-insta --locked
    git config core.hooksPath .githooks

# Build optimized binaries
build:
    cargo build --release --locked
