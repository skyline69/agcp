# AGENTS.md (src/format)

Scope: applies to `src/format/`.

Read `AGENTS.md` and `src/AGENTS.md` first.

## Conversion Rules

- Keep Anthropic compatibility as the external contract.
- When adding/changing fields, update both directions where relevant:
  - `to_google.rs`
  - `to_anthropic.rs`
  - OpenAI/Responses adapters when impacted.
- Do not silently drop meaningful fields without an explicit reason and tests.
- Preserve stop reasons, usage accounting, and tool-call semantics.

## Schema and Tooling Rules

- Keep schema sanitation deterministic and minimal.
- Preserve existing behavior for tool signatures and thinking blocks.

## Required Checks for This Area

- `cargo test format::to_google::tests --bin agcp`
- `cargo test format::to_anthropic::tests --bin agcp`
- `cargo test format::openai_convert::tests --bin agcp`
