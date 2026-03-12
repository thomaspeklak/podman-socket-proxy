# PSP session lifecycle and cleanup

PSP tracks ownership of managed resources so repeated runs do not silently accumulate stale containers.

## Session identity

PSP accepts an optional request header:

```text
x-psp-session-id
```

If absent, PSP uses:

```text
anonymous
```

## Resource labels injected by PSP

For container create requests, PSP injects labels into the forwarded request:

- `io.psp.managed=true`
- `io.psp.session=<session-id>`

These labels are applied at the broker boundary, not trusted from the client.

## Why labels matter

Labels allow PSP to:

- recognize resources it created
- attribute containers to a session
- perform stale-resource cleanup
- avoid relying on client-side cleanup discipline alone

## Cleanup behavior

### Startup sweep

On startup, PSP queries the backend for containers labeled `io.psp.managed=true` and force-removes them before accepting new requests.

This is the crash-recovery path for resources left behind by abnormal exits.

### Shutdown cleanup

During normal shutdown, PSP force-removes tracked containers created during the current process lifetime.

### Debug retention

Set:

```bash
PSP_KEEP_ON_FAILURE=true
```

to skip shutdown cleanup and intentionally keep managed containers for debugging.

On the next normal PSP start, the startup sweep will still remove stale managed containers.

## Response headers

PSP injects two headers on every response (allowed and denied alike):

| Header | Value |
|--------|-------|
| `x-psp-request-id` | Stable per-request identifier (e.g. `psp-00000001`). Use for log correlation. |
| `x-psp-effective-session-id` | The session ID PSP actually used — either the value from `x-psp-session-id` or `anonymous`. |

`x-psp-effective-session-id` is useful when the client needs to confirm which session PSP attributed the request to, especially when no session header was sent.

## Example request flow

1. client sends `x-psp-session-id: sess-123`
2. client calls `POST /v1.41/containers/create`
3. PSP injects:
   - `io.psp.managed=true`
   - `io.psp.session=sess-123`
4. PSP tracks the returned container ID in memory
5. on shutdown, PSP removes tracked containers unless keep-on-failure is enabled

## Operational notes

- repeated PSP runs should not accumulate stale managed containers
- session labeling is useful for attribution even when the client is well-behaved
- cleanup is best-effort; startup sweep is the recovery mechanism for abnormal exits

## What is currently covered

The repository includes test coverage for:

- label injection on create
- startup stale-container sweep
- shutdown cleanup of tracked resources
