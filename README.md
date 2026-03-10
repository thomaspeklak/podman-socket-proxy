# podman-socket-proxy

`podman-socket-proxy` (binary: `psp`) is a policy-gated, Docker-compatible proxy that sits between sandboxed clients and a rootless Podman API.

PSP is designed for the case where you want Docker/Testcontainers-style workflows without giving an untrusted sandbox direct access to the host Podman socket.

## Why PSP exists

Mounting the host Podman socket directly into a sandbox works, but it gives the sandbox indirect control over the host user's container runtime. Even in rootless mode, that can allow dangerous operations such as:

- requesting privileged or high-authority container settings
- bind mounting host paths that contain sensitive data
- creating long-lived resources outside sandbox lifetime
- probing the backend runtime beyond the intended test session

PSP restores an explicit trust boundary:

- the sandbox talks to PSP, not directly to Podman
- PSP accepts only a documented Docker-compatible subset
- PSP applies deny-by-default policy before forwarding requests
- PSP labels resources for ownership and cleanup
- PSP emits auditable allow/deny diagnostics

## What is implemented

### Broker/runtime behavior
- Unix socket listener for `DOCKER_HOST=unix://...` style clients
- forwarding to Podman over either Unix socket or HTTP base URL
- Docker-version-prefixed endpoint matching, such as `/v1.41/...`
- explicit endpoint allowlist for core Testcontainers-style lifecycle flows
- structured client errors for unsupported endpoints and backend failures

### Policy behavior
- schema-validated JSON policy file
- deny-by-default enforcement for privileged mode, host namespaces, devices, capability adds, and non-allowlisted bind mounts
- image allowlist / denylist controls
- stable deny rule IDs with human-readable messages

### Session and cleanup behavior
- session identity from `x-psp-session-id`
- automatic label injection on container create
- startup sweep of stale PSP-managed containers
- shutdown cleanup of tracked containers
- optional debug retention with `PSP_KEEP_ON_FAILURE=true`

### Client compatibility behavior
- support for create/start/inspect/logs/wait/remove lifecycle endpoints
- inspect response rewriting of `HostIp` to a sandbox-reachable host via `PSP_ADVERTISED_HOST`
- deterministic integration suite covering happy path, blocked path, parallel inspect, and cleanup behavior

## Quickstart

### 1. Build and test

```bash
cargo build
cargo test
```

### 2. Start Podman in rootless mode

Typical socket location:

```bash
$XDG_RUNTIME_DIR/podman/podman.sock
```

### 3. Run PSP

```bash
export PSP_BACKEND="unix://$XDG_RUNTIME_DIR/podman/podman.sock"
export PSP_LISTEN_SOCKET="/tmp/psp.sock"
export PSP_POLICY_FILE="policy/default-policy.json"
export PSP_ADVERTISED_HOST="127.0.0.1"

cargo run --bin psp
```

### 4. Point Docker-compatible clients at PSP

```bash
export DOCKER_HOST="unix:///tmp/psp.sock"
```

### 5. Optional: keep containers for debugging

```bash
export PSP_KEEP_ON_FAILURE=true
```

## Configuration reference

| Variable | Default | Meaning |
|---|---|---|
| `PSP_BACKEND` | `unix://$XDG_RUNTIME_DIR/podman/podman.sock` | Podman backend endpoint |
| `PSP_LISTEN_SOCKET` | `/tmp/psp.sock` | Unix socket exposed to clients |
| `PSP_POLICY_FILE` | `policy/default-policy.json` | JSON policy file loaded on startup |
| `PSP_ADVERTISED_HOST` | `127.0.0.1` | host value injected into inspect port mappings |
| `PSP_KEEP_ON_FAILURE` | `false` | skip shutdown cleanup for debugging |
| `RUST_LOG` | unset | standard `tracing` filter, e.g. `info` or `debug` |

## Supported Docker-compatible endpoints

PSP intentionally supports only a narrow v1 surface:

- `GET /_ping`
- `GET /version`
- `GET /info`
- `POST /images/create`
- `POST /containers/create`
- `POST /containers/{id}/start`
- `GET /containers/{id}/json`
- `GET /containers/{id}/logs`
- `POST /containers/{id}/wait`
- `DELETE /containers/{id}`

Version-prefixed paths such as `/v1.41/containers/create` are also supported.

## Common examples

### Example: run PSP with a custom policy

```bash
cat > /tmp/psp-policy.json <<'JSON'
{
  "version": "v1",
  "bind_mounts": {
    "allowlist": ["/workspace"]
  },
  "images": {
    "allowlist": ["postgres:16", "redis:7"],
    "denylist": ["alpine:latest"]
  }
}
JSON

export PSP_POLICY_FILE=/tmp/psp-policy.json
cargo run --bin psp
```

### Example: probe the daemon

```bash
curl --unix-socket /tmp/psp.sock http://d/_ping
curl --unix-socket /tmp/psp.sock http://d/version
curl --unix-socket /tmp/psp.sock http://d/info
```

### Example: blocked privileged container request

```bash
curl --unix-socket /tmp/psp.sock \
  -H 'content-type: application/json' \
  -H 'x-psp-session-id: sess-demo' \
  -X POST \
  http://d/v1.41/containers/create \
  -d '{
    "Image": "postgres:16",
    "HostConfig": {"Privileged": true}
  }'
```

Expected response shape:

```json
{
  "message": "privileged containers are denied by default",
  "kind": "policy_denied",
  "rule_id": "PSP-POL-001"
}
```

## Documentation map

### Start here
- Getting started: `docs/getting-started.md`
- Operator runbook: `docs/operations/runbook.md`
- Policy reference: `docs/policy-reference.md`

### Architecture and contracts
- Architecture / trust model ADR: `docs/adr/0001-psp-architecture.md`
- Testcontainers compatibility contract: `docs/compatibility/testcontainers-profile.md`
- AGS integration contract: `docs/ags-integration.md`

### Operations
- Host/port resolution: `docs/operations/host-port-resolution.md`
- Session lifecycle and cleanup: `docs/operations/session-lifecycle.md`
- Audit logging and deny diagnostics: `docs/operations/audit-logging.md`

### Examples
- HTTP API examples: `docs/examples/http-api-examples.md`
- Example policies: `docs/examples/policies/`
- Example AGS environment wiring: `docs/examples/ags/psp-env.sh`

## Testing

Run everything:

```bash
cargo test
```

Run the integration suite only:

```bash
cargo test --test integration_suite
```

## Non-goals

- replacing AGS
- taking ownership of AGS internals
- full Docker Engine parity
- cloud multi-tenant control plane

## Relationship to AGS

AGS should treat PSP as an external dependency. In PSP mode, AGS should point `DOCKER_HOST` at PSP and must not mount the host Podman socket directly into the sandbox.
