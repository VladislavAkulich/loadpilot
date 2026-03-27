# LoadPilot — local dev commands
# Install just: https://github.com/casey/just

# List available commands
default:
    @just --list

# ── Python ────────────────────────────────────────────────────────────────────

# Run pure unit tests (no coordinator binary required)
test-unit:
    cd cli && uv run pytest tests/ -v --ignore=tests/test_e2e_smoke.py --ignore=tests/test_integration.py

# Run integration + e2e tests in parallel (requires coordinator + agent binaries)
test-e2e:
    cd cli && uv run pytest tests/test_integration.py tests/test_e2e_smoke.py -v -n auto --timeout=120

# Run all Python tests (unit + integration + e2e)
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

# Security audit (requires: cargo install cargo-audit)
# CVE-2026-4539: pygments — fix not yet released on PyPI (2026-03-27).
audit:
    cd engine && cargo audit
    cd cli && uv run pip-audit --ignore-vuln CVE-2026-4539

# ── Combined ──────────────────────────────────────────────────────────────────

# Run all tests (Python + Rust)
test: test-rust test-py

# Run all formatters
fmt: fmt-rust fmt-py

# Full CI check: lint + tests (mirrors CI pipeline)
ci: check test-rust test-unit test-e2e
