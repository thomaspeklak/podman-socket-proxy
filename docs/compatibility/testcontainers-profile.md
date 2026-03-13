# PSP Testcontainers compatibility profile

Status: v1 contract.

This document defines the Docker-compatible API surface PSP currently supports for Rust `testcontainers`-style flows.

PSP does **not** aim for full Docker Engine parity. The surface is intentionally narrow and centered on the request sequence needed to create, start, inspect, observe, wait for, and remove test containers through a policy-gated broker.

## Target client flow

The supported baseline flow is:

1. probe daemon health/capabilities
2. pull an image if missing
3. create a container
4. start the container
5. inspect container state and published ports
6. read logs while waiting for readiness
7. wait for exit where needed
8. remove the container

This covers the common lifecycle used by Rust `testcontainers` modules that follow the standard startup pattern.

## Path handling

PSP accepts both:

- unversioned paths, such as `/containers/create`
- Docker-version-prefixed paths, such as `/v1.41/containers/create`

PSP matches requests against the normalized path but forwards the original request path to the backend.

## Supported endpoints

| Endpoint | Method | Supported | Notes |
|---|---|---|---|
| `/_ping` | `GET` | yes | liveness probe |
| `/version` | `GET` | yes | version/capability probe |
| `/info` | `GET` | yes | runtime metadata probe |
| `/images/create` | `POST` | yes | image pull entrypoint |
| `/containers/json` | `GET` | yes | container list; PSP filters response to `io.psp.managed=true` containers only |
| `/containers/create` | `POST` | yes | create container; PSP may inject labels |
| `/containers/{id}/start` | `POST` | yes | start container |
| `/containers/{id}/json` | `GET` | yes | inspect container; PSP rewrites `HostIp` fields |
| `/images/{name}/json` | `GET` | yes | image inspect; name may contain slashes (e.g. `org/image:tag`) |
| `/containers/{id}/logs` | `GET` | yes | container logs |
| `/containers/{id}/wait` | `POST` | yes | wait for exit |
| `/containers/{id}` | `DELETE` | yes | remove container |
| `/containers/{id}/exec` | `POST` | yes | create exec instance for health checks |
| `/exec/{id}/start` | `POST` | yes | start exec instance |
| `/exec/{id}/json` | `GET` | yes | inspect exec result (exit code) |

## Important PSP-specific behavior

### Policy enforcement

Supported does not mean unconditionally allowed. PSP still applies policy to supported endpoints.

Examples:

- `POST /containers/create` is supported, but privileged containers are denied
- `POST /images/create` is supported, but image policy can block specific images
- `GET /containers/{id}/json` is supported, but access to pre-existing non-PSP-managed containers is denied unless explicitly allowlisted

### Session labeling

For container create requests, PSP may inject labels into the forwarded request:

- `io.psp.managed=true`
- `io.psp.session=<session-id>`

When configured with `PSP_REQUIRE_SESSION_ID=true`, PSP rejects mutating requests that do not provide a valid session ID.

### Port host rewriting

On container inspect responses, PSP rewrites `NetworkSettings.Ports[*].HostIp` to `PSP_ADVERTISED_HOST` so the client receives a sandbox-usable connect host.

## Unsupported or deferred endpoints

These are common future candidates, but are not part of the current v1 contract:

| Endpoint | Method | Status | Why |
|---|---|---|---|
| `/networks/create` | `POST` | unsupported | network policy and ownership model not implemented |
| `/networks/{id}` | `DELETE` | unsupported | deferred until network lifecycle support exists |
| `/containers/{id}/stop` | `POST` | unsupported | not required for MVP lifecycle |
| `/events` | `GET` | unsupported | streaming support not required for MVP |
| `/images/{name}/json` | `GET` | supported | moved to supported endpoints above |
| broad Docker surface | many | unsupported | PSP uses an explicit allowlist |

## Response/error contract

### Unsupported endpoint

Status:

```text
501 Not Implemented
```

Body shape:

```json
{
  "message": "unsupported endpoint: POST /v1.41/networks/create",
  "kind": "unsupported_endpoint",
  "method": "POST",
  "path": "/v1.41/networks/create",
  "hint": "Use only the documented Testcontainers-compatible PSP API subset.",
  "docs": "docs/compatibility/testcontainers-profile.md",
  "request_id": "psp-00000002",
  "session_id": "anonymous"
}
```

### Policy denial

Status:

```text
403 Forbidden
```

Body shape:

```json
{
  "message": "privileged containers are denied by default",
  "kind": "policy_denied",
  "rule_id": "PSP-POL-001",
  "hint": "Remove HostConfig.Privileged or change policy intentionally if this is expected.",
  "docs": "docs/policy-reference.md",
  "request_id": "psp-00000001",
  "session_id": "sess-42"
}
```

### Missing required session

Status:

```text
400 Bad Request
```

Body shape includes:

```json
{
  "kind": "session_required"
}
```

### Backend failure

Status:

```text
502 Bad Gateway
```

Body shape includes:

```json
{
  "kind": "backend_error"
}
```

### Internal PSP failure

Status:

```text
500 Internal Server Error
```

Body shape includes:

```json
{
  "kind": "internal_error"
}
```

## Known differences from Docker Engine

- PSP exposes only a curated subset of the Docker API
- PSP sits in front of rootless Podman, so behavior may differ from Docker in edge cases
- policy enforcement intentionally blocks some request shapes even if Podman would accept them
- PSP adds session labeling, cleanup semantics, and audit logs that are not Docker Engine features

## Verified by tests

The repository currently verifies this contract with:

- unit/contract coverage in `src/lib.rs`
- integration coverage in `tests/integration_suite.rs`

The integration suite covers:

- happy-path lifecycle flow
- blocked privileged request path
- parallel inspect requests
- cleanup behavior
