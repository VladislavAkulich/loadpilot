# LoadPilot — local dev commands
# Install just: https://github.com/casey/just

# List available commands
default:
    @just --list

# ── Python ────────────────────────────────────────────────────────────────────

# Run Python tests with coverage
test-py:
    cd cli && uv run pytest tests/ -v

# Run Python linter
lint:
    cd cli && uv run ruff check .

# Run Python formatter
fmt-py:
    cd cli && uv run ruff format .

# Lint + format check (no writes) — same as CI
check:
    cd cli && uv run ruff check . && uv run ruff format --check .

# Fix lint issues and format
fix:
    cd cli && uv run ruff check --fix . && uv run ruff format .

# ── Rust ──────────────────────────────────────────────────────────────────────

# Run Rust tests
test-rust:
    cd engine && cargo test

# Build Rust binaries (debug)
build:
    cd engine && cargo build

# Build Rust binaries (release)
build-release:
    cd engine && cargo build --release

# Run Rust linter
lint-rust:
    cd engine && cargo clippy -- -D warnings

# Format Rust code
fmt-rust:
    cd engine && cargo fmt

# ── Combined ──────────────────────────────────────────────────────────────────

# Run all tests (Python + Rust)
test: test-rust test-py

# Run all formatters
fmt: fmt-rust fmt-py

# Full CI check: lint + tests
ci: check test-rust test-py
