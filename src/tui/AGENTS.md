# AGENTS.md (src/tui)

Scope: applies to `src/tui/`.

Read `AGENTS.md` and `src/AGENTS.md` first.

## TUI State and Rendering Rules

- Keep app state transitions in `app.rs`; keep view modules render-focused.
- For modal/popup UX:
  - Add explicit `App` state.
  - Block unrelated key handling while popup is visible.
  - Support dismissal via `Enter` and `Esc`.
- Maintain overlay precedence intentionally (help/popup/startup warnings ordering).
- For log-driven warnings, normalize comparisons (case-insensitive) and test with realistic log lines.
- Keep terminal-size handling and layout guards intact.

## Interaction Rules

- Mouse hit-testing should use cached rects already maintained in `App`.
- Any new keyboard shortcut must not conflict with existing tab-local keybindings.

## Required Checks for This Area

- `cargo test tui::app::tests --bin agcp`
- `cargo clippy -- -D warnings`
