# Build automation for Vector Editor

# Default recipe
default: build

# Debug build
build:
    cargo build

# Release build
release:
    cargo build --release

# Run the editor (debug)
run:
    cargo run -p vector-editor

# Run the editor (release, faster rendering)
run-release:
    cargo run -p vector-editor --release

# Run the editor with Tracy profiling enabled. Launch `tracy-profiler` (the
# Tracy capture UI) first, or start it after — it will connect to a running
# client. Use release to get realistic timings.
run-tracy:
    cargo run -p vector-editor --release --features tracy

# Run all tests
test:
    cargo test --workspace

# Lint with clippy
lint:
    cargo clippy --workspace --all-targets -- -D warnings

# Check formatting
fmt-check:
    cargo fmt --all -- --check

# Format code
fmt:
    cargo fmt --all

# Full CI check (format + lint + test)
ci: fmt-check lint test

# Clean build artifacts
clean:
    cargo clean
