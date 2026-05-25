# Build automation for Locus

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
    cargo run -p locus-editor

# Run the editor (release, faster rendering)
run-release:
    cargo run -p locus-editor --release

# Run the editor with Tracy profiling enabled. Launch `tracy-profiler` (the
# Tracy capture UI) first, or start it after — it will connect to a running
# client. Use release to get realistic timings.
run-tracy:
    cargo run -p locus-editor --release --features tracy

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

# Regenerate every derived icon asset (PNG sizes, .ico, .icns, raw RGBA)
# from assets/icon.svg + assets/icon-small.svg. Commit the results.
icons:
    ./scripts/gen-icons.sh

# Clean build artifacts
clean:
    cargo clean
