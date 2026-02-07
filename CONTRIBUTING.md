# Contributing to AGCP

Contributions are welcome. This document covers what you need to get started.

## Prerequisites

- **Rust 1.93+** (install via [rustup](https://rustup.rs/))
- A **Google Cloud account** with Cloud Code API access
- Git

## Building

```bash
cargo build            # Debug build
cargo build --release  # Optimized build with LTO
```

## Testing

```bash
cargo test
```

## Linting and Formatting

Both must pass before submitting a PR:

```bash
cargo clippy -- -D warnings
cargo fmt --check
```

## Code Style

Follow the conventions documented in [AGENTS.md](./AGENTS.md). Key points:

- Import ordering: std, external crates, internal modules (separated by blank lines)
- Use `thiserror` for error types with `suggestion()` methods for user-facing hints
- Inline tests in source files under `#[cfg(test)] mod tests`
- `tokio` for async, `Arc<RwLock<T>>` for shared state

## Submitting Changes

1. Fork the repository
2. Create a branch from `main` (`git checkout -b feat/my-change`)
3. Make your changes
4. Ensure `cargo test`, `cargo clippy -- -D warnings`, and `cargo fmt --check` all pass
5. Commit using [Conventional Commits](https://www.conventionalcommits.org/):
   - `feat:` -- new functionality
   - `fix:` -- bug fix
   - `chore:` -- maintenance, dependencies, CI
   - `perf:` -- performance improvement
   - `docs:` -- documentation changes
6. Open a pull request against `main`

Keep PRs focused. One logical change per PR is easier to review than a large mixed changeset.

## Reporting Issues

Open an issue on [GitHub](https://github.com/skyline69/agcp/issues). Include reproduction steps, expected vs. actual behavior, and your platform/Rust version.

For security vulnerabilities, see [SECURITY.md](./SECURITY.md).

## License

By contributing, you agree that your contributions will be licensed under the MIT License.
