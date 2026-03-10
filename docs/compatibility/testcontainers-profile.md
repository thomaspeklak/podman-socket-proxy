# PSP Testcontainers compatibility profile

- Status: v1 draft contract
- Related issue: `psp-287.4`

## Purpose

This document defines the initial Docker-compatible API contract that PSP supports for Rust `testcontainers`-style lifecycle flows.

PSP does **not** aim for full Docker Engine parity. The supported surface is intentionally narrow and centered on the request sequence needed to create, start, inspect, observe, wait for, and remove test containers through a policy-gated broker.

## Supported client flow

The target v1 flow is:

1. Probe daemon health/capabilities.
2. Pull an image if missing.
3. Create a container.
4. Start the container.
5. Inspect container state and port metadata.
6. Read logs while waiting for readiness.
7. Wait for exit where needed.
8. Remove the container.

This covers the baseline lifecycle used by Rust `testcontainers` modules that follow the common container startup pattern.

## Endpoint contract

Versioned Docker API paths such as `/v1.41/...` are accepted. PSP normalizes the version prefix for contract matching and forwards the original request path to the backend.

### Required endpoints

| Endpoint | Method | PSP status | Notes |
|---|---|---|---|
| `/_ping` | `GET` | supported | daemon liveness probe |
| `/version` | `GET` | supported | daemon capability/version probe |
| `/info` | `GET` | supported | runtime metadata probe |
| `/images/create` | `POST` | supported | image pull entrypoint |
| `/containers/create` | `POST` | supported | container creation |
| `/containers/{id}/start` | `POST` | supported | container start |
| `/containers/{id}/json` | `GET` | supported | inspect state and networking |
| `/containers/{id}/logs` | `GET` | supported | readiness / diagnostics logs |
| `/containers/{id}/wait` | `POST` | supported | wait for container exit |
| `/containers/{id}` | `DELETE` | supported | remove container |

### Optional future endpoints

These are common candidates for later compatibility expansion but are not part of the v1 contract yet.

| Endpoint | Method | PSP status | Rationale |
|---|---|---|---|
| `/networks/create` | `POST` | unsupported | network policy and cleanup semantics need design |
| `/networks/{id}` | `DELETE` | unsupported | blocked until session/network ownership model lands |
| `/containers/{id}/stop` | `POST` | unsupported | not required for minimal happy path yet |
| `/events` | `GET` | unsupported | streaming API not needed for MVP |
| `/images/{name}/json` | `GET` | unsupported in current broker | may be needed once image policy and caching behavior are refined |

### Unsupported by design in v1

| Category | Examples | Reason |
|---|---|---|
| Broad Docker daemon surface | build, exec, commit, swarm, plugins, volumes | outside PSP MVP scope |
| Unsafe high-authority operations | privileged containers, host namespace joins, device requests | denied by policy baseline |
| Arbitrary pass-through | unknown Docker endpoints | explicit allowlist is required |

## Behavior contract

### Endpoint matching

- PSP accepts both unversioned and Docker-version-prefixed paths.
- Unsupported endpoints fail fast in PSP and are **not** forwarded to Podman.
- Unsupported endpoint responses use structured JSON with a machine-readable `kind`.

Example unsupported response shape:

```json
{
  "message": "unsupported endpoint: POST /v1.41/networks/create",
  "kind": "unsupported_endpoint",
  "method": "POST",
  "path": "/v1.41/networks/create"
}
```

### Forwarding

- Allowed requests preserve HTTP method, path, query string, headers except `Host`, and body payload.
- PSP forwards the original versioned path when present so backend compatibility remains intact.
- Backend response status, headers (minus hop-by-hop headers), and body are returned to the client.

### Error handling

- Unsupported endpoints return `501 Not Implemented`.
- Backend communication failures return `502 Bad Gateway`.
- Internal PSP failures return `500 Internal Server Error`.

## Known differences from Docker Engine

- PSP exposes only a curated subset of the Docker API.
- PSP sits in front of rootless Podman, so runtime behavior may differ from Docker Engine in edge cases.
- Policy enforcement will intentionally block some Docker request shapes even if Podman would otherwise accept them.
- Session labeling, cleanup, audit logging, and deny rule IDs are PSP-specific behavior layered on top of the Docker-compatible surface.

## Contract verification

The broker currently includes contract tests that prove:

- create/start/inspect/logs/wait/remove lifecycle calls pass through PSP successfully
- version-prefixed paths are accepted
- unsupported endpoints fail with structured `501` responses and are not forwarded upstream

These tests live in:

- `src/lib.rs` (`proxies_container_lifecycle_endpoints`)
- `src/lib.rs` (`rejects_unsupported_endpoints_with_structured_error`)

Follow-on work in `psp-287.8` should expand this into a dedicated integration and compatibility suite.
