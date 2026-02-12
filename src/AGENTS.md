# AGENTS.md (src)

Scope: applies to everything under `src/`.

Start by reading the repository root `AGENTS.md`. This file adds source-tree specific rules.

## Source Rules

- Follow existing import order: `std` -> external crates -> `crate::*`.
- Preserve existing error flow (`crate::error::{Error, Result}`) instead of ad-hoc error types.
- Keep hot paths allocation-aware and prefer existing helpers/utilities.
- Keep tracing structured (`request_id`, `model`, `status`, etc.) and avoid free-form log noise.
- Any behavior change must include/adjust tests in the touched module.

## Verification by Area

- `src/server.rs` or request handlers changed:
  - `cargo test server::tests --bin agcp`
- `src/cloudcode/*` changed:
  - `cargo test cloudcode:: --bin agcp`
- `src/tui/*` changed:
  - `cargo test tui::app::tests --bin agcp`
- `src/format/*` changed:
  - `cargo test format:: --bin agcp`

Before final handoff, run:

- `cargo fmt`
- `cargo clippy -- -D warnings`
- `cargo test`
