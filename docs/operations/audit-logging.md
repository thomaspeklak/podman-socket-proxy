# PSP audit logging and deny diagnostics

PSP emits structured logs for request allow/deny decisions so operators can answer:

- which session made the request?
- which broker operation was attempted?
- which image or container was involved?
- why was a request denied?
- which stable policy rule blocked it?

## Logging model

PSP uses `tracing`-based structured fields rather than dumping raw payloads.

Start PSP with a log level such as:

```bash
RUST_LOG=info cargo run --bin psp
```

## Allow log fields

Allow logs include fields such as:

- `decision=allow`
- `session`
- `operation`
- `path`
- `target_image`
- `target_container`
- `status`

Example shape:

```text
decision=allow session=sess-42 operation=containers.create path=/v1.41/containers/create target_image=postgres:16 status=201 psp forwarded request
```

## Deny log fields

Deny logs include fields such as:

- `decision=deny`
- `kind=policy_denied` or `kind=unsupported_endpoint`
- `rule_id` for policy denials
- `session`
- `operation`
- `path`
- `target_image`
- `target_container`
- `reason` for policy denials

Example shape:

```text
decision=deny kind=policy_denied rule_id=PSP-POL-001 session=sess-42 operation=containers.create path=/v1.41/containers/create target_image=postgres:16 reason="privileged containers are denied by default" psp denied request
```

## Operation names currently emitted

- `daemon.ping`
- `daemon.version`
- `daemon.info`
- `images.create`
- `containers.create`
- `containers.start`
- `containers.inspect`
- `containers.logs`
- `containers.wait`
- `containers.delete`

## Secret-safe logging

PSP intentionally avoids logging raw JSON request bodies, headers, auth data, env vars, or pull secrets.

Instead, it extracts a small safe summary:

- image reference from `Image` or `fromImage`
- container identifier from the request path
- session identifier from `x-psp-session-id`

That means logs remain useful without persisting obvious plaintext secrets such as:

- environment variables
- credentials in image pull payloads
- opaque auth headers
- bind mount contents

## Client-facing deny diagnostics

Policy denials return structured responses with:

- `kind=policy_denied`
- `rule_id=<stable-id>`
- human-readable `message`

Example:

```json
{
  "message": "privileged containers are denied by default",
  "kind": "policy_denied",
  "rule_id": "PSP-POL-001"
}
```

Unsupported endpoints return:

```json
{
  "message": "unsupported endpoint: POST /v1.41/networks/create",
  "kind": "unsupported_endpoint",
  "method": "POST",
  "path": "/v1.41/networks/create"
}
```

## Recommended operator workflow

When a user reports a blocked container flow:

1. capture the client-visible error body
2. look at PSP logs for the same session ID
3. identify `kind`, `operation`, and `rule_id`
4. determine whether the request is unsupported or denied by policy
5. update policy or compatibility support intentionally, not ad hoc
