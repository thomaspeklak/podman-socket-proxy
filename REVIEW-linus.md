# PSP Code Review — Linus Style

*Reviewer: Your Friendly Neighborhood Tyrant*
*Date: 2026-03-11*
*Verdict: Shows promise, but has some real head-scratchers*

---

## The Good

I'll start with the positive because I'm not a complete monster.

The **scope is right**. This is a focused proxy that does one thing: sit between
a sandboxed client and Podman, enforce policy, and get out of the way. You didn't
try to reinvent Kubernetes. You didn't build a plugin system. You built a proxy.
Thank you.

The **policy engine** (`src/policy.rs`) is genuinely well-structured. Seven clear
rules with stable IDs (`PSP-POL-001` through `PSP-POL-007`), deny-by-default,
readable error messages. This is the kind of code where I can tell someone
actually thought about what happens when things go wrong, not just when they go
right. The separation between `evaluate_container_create` and
`evaluate_image_pull` is clean. The `Denial` type carrying a `&'static str`
rule_id is correct — you can't accidentally construct a dynamic rule ID at
runtime. Good.

The **session lifecycle** is simple and correct. Label injection, tracking,
startup sweep, shutdown cleanup. The `io.psp.managed=true` label convention is
exactly right — you can always find your mess and clean it up.

The **structured error responses** with `kind` + `rule_id` fields are exactly
what downstream consumers need. No guessing from HTTP status codes alone.

**390 lines** for the policy engine. **88 lines** for session management. These
are the right sizes for what they do. I wish I could say the same about lib.rs.

---

## The Bad

### lib.rs is a 1,471-line god module

This is the single biggest problem with this codebase. You have a file that is
simultaneously:

- Configuration parsing (`Config`, `BackendConfig`)
- HTTP client abstraction (`BackendClient` with two variants)
- Request routing and proxying (`proxy_request`)
- Response rewriting (`rewrite_response_body`)
- Error types and response formatting (`ProxyError`, `ErrorBody`)
- Server lifecycle (`serve_with_shutdown`, `shutdown_signal`)
- Path normalization utilities
- Audit context extraction
- **653 lines of test code** with 6 different mock backend spawners

You correctly split policy and session into their own modules. Then you dumped
*everything else* into lib.rs. The `BackendClient` alone deserves its own module.
The error types deserve their own module. The test helpers — which are
*duplicated* between lib.rs and integration_suite.rs — should be in a shared test
utility.

This isn't a style complaint. When someone needs to modify the proxy forwarding
logic, they have to navigate past config parsing, error types, audit context,
and 653 lines of test scaffolding to understand what they're changing. That's
how bugs get introduced.

### The test duplication is inexcusable

You have the `Calls` struct defined **identically** in both `src/lib.rs:830-847`
and `tests/integration_suite.rs:33-50`. You have `test_policy()` defined in both
places. You have `request_json()` and `request_text()` helpers in both places —
almost identical but with slightly different signatures (the integration suite
version takes an optional `session` parameter, the lib.rs version doesn't).

You have `normalize_versioned_path()` — a **public function** — duplicated in
`tests/integration_suite.rs:372-385` because the test file apparently couldn't
import it. Except it *could*, because it's `pub`. This copy just... exists.

You have **six** different `spawn_*_backend()` functions in lib.rs tests alone:
`spawn_backend`, `spawn_create_backend`, `spawn_sweep_backend`,
`spawn_delete_backend`, `spawn_inspect_backend`. Plus another
`spawn_lifecycle_backend` in the integration suite. That's seven mock backend
spawners, most of which share 80% of their code.

This is the kind of copy-paste that turns a codebase into a minefield. Change
the mock behavior in one place, forget the other six, and now your tests are
lying to you.

### The URL-encoded magic string in startup_sweep

```rust
.get_json("/containers/json?all=1&filters=%7B%22label%22%3A%5B%22io.psp.managed%3Dtrue%22%5D%7D")
```

*Are you serious?*

That decodes to `{"label":["io.psp.managed=true"]}`. You have a perfectly good
`LABEL_MANAGED` constant in session.rs. You have the `url` crate in your
dependencies. But no, let's just hardcode a URL-encoded JSON blob as a string
literal. If anyone ever changes the label name, they'll update the constant
and the filter will silently stop matching anything. Brilliant.

### The hop_by_hop_header function is doing unnecessary work

```rust
fn hop_by_hop_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str().to_ascii_lowercase().as_str(),
        "connection" | ...
    )
}
```

`HeaderName::as_str()` already returns a lowercase string. The HTTP crate
normalizes header names to lowercase on construction. You're allocating a new
`String` via `to_ascii_lowercase()` on every header of every response for
absolutely no reason. In a proxy that handles every single request, this is
death by a thousand paper cuts.

### is_supported_endpoint is a mess

You match four endpoints with a clean pattern match, then fall through to five
separate `_ if method == ... && normalized.starts_with(...) && normalized.ends_with(...)`
arms. This is trying to be a match statement and a chain of if-else at the same
time, and it's neither.

Either use a proper routing table (a `Vec<(Method, &str)>` you iterate, or even
a `HashSet`), or commit to the pattern match fully. The current code also has a
subtle correctness issue: `normalized.ends_with("/json")` would match
`/containers/foo/bar/json` — not just `/containers/{id}/json`. You're one
malicious path segment away from this being exploitable.

### BackendClient duplicates 90% of its logic between Http and Unix

Look at `BackendClient::send()`. The Http arm and the Unix arm do exactly the
same thing:
1. Build a path from the URI
2. Copy headers (skipping host and content-length)
3. Attach body
4. Send request
5. Collect response

The only difference is the client type and URI construction. This is screaming
for a trait or at minimum a shared helper. Instead you have two nearly-identical
40-line blocks that will inevitably drift apart.

### container_id_from_path vs extract_target_container

You have two functions that extract a container ID from a path:

- `container_id_from_path()` — returns `Option<&str>`, checks for `/` to ensure
  single segment
- `extract_target_container()` — returns `Option<String>`, splits on `/`, filters
  out "create"

They do the same thing differently. One returns a borrowed str, the other
allocates a String. One checks for `create`, the other doesn't. Pick one.

---

## The Ugly

### No request body size limits

You call `body.collect().await` on every incoming request with zero bounds.
An attacker can POST a 10GB JSON blob to `/containers/create` and your proxy
will happily try to buffer it all in memory. This is a security proxy. Act
like one.

### The bind mount allowlist is prefix-based with no canonicalization

```rust
fn is_allowed(&self, source: &str) -> bool {
    self.allowlist.iter().any(|prefix|
        source == prefix || source.starts_with(&format!("{prefix}/"))
    )
}
```

If I send a bind mount with source `/workspace/../etc/shadow`, your prefix
check passes (`/workspace/..` starts with `/workspace/`), and I just mounted
`/etc/shadow` into my container. This is the single most critical security
bug in this codebase.

You need `std::fs::canonicalize()` or at minimum a path traversal check before
comparing against the allowlist.

### Image matching is exact string comparison

`postgres:16` in the allowlist won't match `docker.io/library/postgres:16` or
`postgres:16.2` or `postgres@sha256:abc123`. Real Docker clients send all of
these variants. Your allowlist will either be uselessly specific or users will
give up and leave it empty.

### No timeout on backend requests

If Podman hangs (which it absolutely will at some point), your proxy hangs
with it. Every connection backs up. Eventually you run out of file descriptors.
This is a proxy — timeout discipline is non-negotiable.

---

## The Beads Situation

All 10 issues are closed. Zero open work items. The velocity chart shows all
10 closed in the same week with an average close time of 0.03 days.

That's either incredibly efficient execution or someone closed a bunch of
planning issues to feel good about progress. Given that the code still has
a path traversal vulnerability in a security proxy, I'm guessing the latter.

The issue tracker should reflect reality. If the items above aren't tracked,
the tracker is decoration, not tooling.

---

## Summary

| Area | Grade | Notes |
|------|-------|-------|
| Architecture | B+ | Right scope, right layers, wrong file boundaries |
| Policy Engine | A- | Clean, well-tested, stable rule IDs |
| Session Management | A | Simple, correct, right-sized |
| Security | D | Path traversal in bind mounts, no body limits, no timeouts |
| Code Organization | C | 1471-line god module, massive test duplication |
| Test Coverage | B | Good scenario coverage, terrible code organization |
| Operability | B+ | Structured logging, audit context, graceful shutdown |

The bones are good. The policy design is genuinely well-thought-out. But this
is supposed to be a *security* proxy, and it has a path traversal bug in its
bind mount validation. Fix that first. Then break up lib.rs. Then deduplicate
the test infrastructure. Then add body size limits and backend timeouts.

Stop closing issues that don't represent shipped, hardened features.

---

## Target Architecture

Every `src/*.rs` file under 500 LoC. Tests live outside source files.

```
src/
├── main.rs           (~12)  entry point — unchanged
├── lib.rs            (~120) module decls, re-exports, AppState, router(),
│                            serve_with_shutdown(), shutdown_signal()
├── config.rs         (~70)  Config, BackendConfig
├── error.rs         (~100)  ProxyError, ErrorBody, json_response, IntoResponse
├── paths.rs          (~80)  normalize_versioned_path, is_supported_endpoint,
│                            path_segment_count, container_id_from_path,
│                            hop_by_hop_header
├── audit.rs         (~100)  RequestAuditContext, operation_name, extract_target_*
├── backend.rs       (~150)  BackendClient, ForwardedResponse
├── rewrite.rs        (~50)  rewrite_response_body
├── proxy.rs         (~150)  proxy_request handler, extract_container_id,
│                            maybe_inject_session_labels
├── policy.rs        (~390)  unchanged (unit tests stay — they're small)
└── session.rs        (~88)  unchanged

tests/
├── helpers/
│   └── mod.rs       (~200)  Calls, test_policy, spawn_mock_backend (ONE
│                            configurable spawner replacing all 7), request_json,
│                            request_text, spawn_psp
├── proxy_tests.rs           9 tests migrated from lib.rs #[cfg(test)]
└── integration_suite.rs     cleaned up to use helpers/
```

### Dependency Flow

```
config.rs ─────────────┐
error.rs ──────────────┤
paths.rs ──────────────┤
                       ▼
audit.rs ◄── paths.rs
backend.rs ◄── error.rs
rewrite.rs ◄── paths.rs
                       │
                       ▼
proxy.rs ◄── audit + backend + rewrite + paths + error
                       │
                       ▼
lib.rs (AppState, router, serve) ◄── all modules
```

### Execution Order (beads dependency graph)

```
PARALLEL (no deps — start immediately):
  psp-3tf  SEC: Fix path traversal in bind mount validation     [P0]
  psp-ttn  SEC: Add request body size limits                    [P1]
  psp-1pl  SEC: Add backend request timeouts                    [P1]
  psp-2fm  SEC: Improve image reference matching                [P1]
  psp-e64  REFACTOR: Extract config.rs                          [P2]
  psp-3ck  REFACTOR: Extract error.rs                           [P2]
  psp-1tr  REFACTOR: Extract paths.rs (+ bug fixes)             [P2]

AFTER psp-1tr:
  psp-3nl  REFACTOR: Extract audit.rs
  psp-15m  REFACTOR: Extract rewrite.rs

AFTER psp-3ck:
  psp-1t8  REFACTOR: Extract backend.rs (+ magic string fix)

AFTER psp-3nl + psp-1t8 + psp-15m:
  psp-x8o  REFACTOR: Extract proxy.rs + slim lib.rs

AFTER psp-x8o:
  psp-2rc  TEST: Create shared helpers + consolidate all tests
```

---

*"Talk is cheap. Show me the code." — and the code showed me a path traversal.*
