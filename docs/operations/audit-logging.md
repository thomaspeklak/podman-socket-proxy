# PSP audit logging and deny diagnostics

- Related issue: `psp-287.7`

## Goals

PSP emits structured logs for request allow/deny decisions so operators can answer:

- which session made the request?
- which broker operation was attempted?
- which image or container was involved?
- why was a request denied?
- which stable policy rule blocked it?

## Log shape

PSP uses structured `tracing` fields rather than raw request payload dumps.

### Allow log fields

- `decision=allow`
- `session`
- `operation`
- `path`
- `target_image`
- `target_container`
- `status`

### Deny log fields

- `decision=deny`
- `kind=policy_denied` or `kind=unsupported_endpoint`
- `rule_id` for policy denials
- `session`
- `operation`
- `path`
- `target_image`
- `target_container`
- `reason` for policy denials

## Secret-safe logging

PSP intentionally avoids logging raw JSON request bodies, headers, auth data, env vars, or pull secrets.

Instead, it extracts a small safe summary:

- image reference from `Image` or `fromImage`
- container identifier from the request path
- session identifier from `x-psp-session-id`

This keeps logs parseable without storing plaintext secrets.

## Deny diagnostics

Policy denials also return structured client responses with:

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

## Current operation names

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
