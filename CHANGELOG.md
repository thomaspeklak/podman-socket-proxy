# Changelog

## [v0.1.0] — 2026-03-13

### Features
- Implement PSP MVP broker (a940bf1)
- Add deny-by-default PSP policy engine (826c66f)
- Implement PSP host port resolution strategy (aff8e01)
- Add PSP session lifecycle and cleanup (0dc0ec6)
- Add PSP audit logging and deny diagnostics (c865277)
- Add PSP integration and compatibility suite (dd33a5c)
- Add modern TUI: interactive discover browser, status dashboard, doctor sections, image picker, profile wizard (4e7ca8c)
- Support GET /images/{name}/json for Java Testcontainers compatibility (706a432)
- Support GET /containers/json for Testcontainers reuse pattern (2b42207)
- Support exec endpoints for Testcontainers health checks (7d145b9)
- Add debug-level request/response body logging (1f39e3f)
- Add PSP_LOG_FILE for file-based logging (ff66018)
- Support POST /containers/{id}/stop for cleanup after timeout (47fa726)
- Let Testcontainers containers through the container list filter (e5394a5)
- Support PUT /containers/{id}/archive for file copy into container (01566d7)
- Skip startup sweep when keep_on_failure=true (31d8671)

### Bug Fixes
- style: format code and fix clippy warnings (0664776)
- Simplify CLI, policy matching, and session/socket cleanup (0a25c06)

### Chores / Other
- Document PSP compatibility contract (6a22902)
- Publish PSP operator and integration docs (c81705a)
- Expand PSP documentation and examples (8d42b2e)
- Add repo ignore and agent workflow docs (944bd78)
- Remove stale Linus-style code review artifact (bb1fbd4)
- Document response headers, 4MB body limit, and bind mount traversal protection (e67c8df)
- Add logo, MIT license, and experimental disclaimer to README (01ac4c9)
- Ignore *.log files (3380819)
- chore: add release prompt and tag workflow (fb5b571)
