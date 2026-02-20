# agcp justfile — run `just` to list all recipes

# Default: list recipes
default:
    @just --list

# Build (debug)
build:
    cargo build

# Build (optimized release with LTO)
release:
    cargo build --release

# Run in release mode (default port 8080, host 127.0.0.1)
run:
    cargo run --release

# Run with custom options (e.g. `just run-opts --port 3000 --host 0.0.0.0 --debug`)
run-opts *args:
    cargo run --release -- {{ args }}

# First-time setup: OAuth login
login:
    cargo run --release -- --login

# Run all tests
test:
    cargo test

# Run a specific test (e.g. `just test-one test_model_family`)
test-one name:
    cargo test {{ name }}

# Format source code
fmt:
    cargo fmt

# Lint with clippy
lint:
    cargo clippy -- -D warnings

# Format + lint
check: fmt lint

# Clean build artifacts
clean:
    cargo clean

# Build Tailwind CSS for the docs site
css:
    bunx @tailwindcss/cli -i docs/input.css -o docs/style.css --minify
