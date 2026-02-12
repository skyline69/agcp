# AGENTS.md (src/cloudcode)

Scope: applies to `src/cloudcode/`.

Read `AGENTS.md` and `src/AGENTS.md` first.

## Cloud Code Client Rules

- Keep endpoint failover order stable unless explicitly required:
  - `daily-cloudcode-pa.googleapis.com`
  - `cloudcode-pa.googleapis.com`
- Keep retry/backoff behavior coherent with existing constants and shared rate-limit helpers.
- Avoid returning raw upstream provider payloads when a clear mapped error is possible.
- New error mapping must include a regression test with representative upstream payload text.
- Preserve request budget behavior (`MAX_WAIT_BEFORE_ERROR_MS`) and capacity retry limits.

## Streaming / SSE

- Keep SSE parser behavior compatible with existing event contracts.
- Do not change event ordering or stop semantics without tests proving compatibility.

## Required Checks for This Area

- `cargo test cloudcode::client::tests --bin agcp`
- `cargo test cloudcode::rate_limit::tests --bin agcp`
- `cargo test cloudcode::sse::tests --bin agcp`
