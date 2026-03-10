# PSP operator runbook

This runbook is intended for local operators and integrators who need to build, run, validate, and troubleshoot PSP.

## Prerequisites

- Rust toolchain installed
- rootless Podman API socket available
- a writable Unix socket path for PSP

Typical rootless Podman socket location:

```bash
$XDG_RUNTIME_DIR/podman/podman.sock
```

## Build

```bash
cargo build
```

## Validate before running

Run the full test suite:

```bash
cargo test
```

Run only the integration suite:

```bash
cargo test --test integration_suite
```

## Run PSP locally

```bash
export PSP_BACKEND="unix://$XDG_RUNTIME_DIR/podman/podman.sock"
export PSP_LISTEN_SOCKET="/tmp/psp.sock"
export PSP_POLICY_FILE="policy/default-policy.json"
export PSP_ADVERTISED_HOST="127.0.0.1"
export RUST_LOG=info

cargo run --bin psp
```

Optional debug override:

```bash
export PSP_KEEP_ON_FAILURE=true
```

## Point Docker-compatible clients at PSP

```bash
export DOCKER_HOST="unix:///tmp/psp.sock"
```

## Recommended startup checklist

1. confirm the Podman socket exists
2. confirm the PSP policy file exists and is valid JSON
3. choose an appropriate `PSP_ADVERTISED_HOST`
4. start PSP with `RUST_LOG=info`
5. probe `/_ping` through the PSP socket

Example probe:

```bash
curl --unix-socket /tmp/psp.sock http://d/_ping
```

Expected output:

```text
OK
```

## Key configuration knobs

| Variable | Default | Purpose |
|---|---|---|
| `PSP_BACKEND` | `unix://$XDG_RUNTIME_DIR/podman/podman.sock` | Podman backend endpoint |
| `PSP_LISTEN_SOCKET` | `/tmp/psp.sock` | client-facing Unix socket |
| `PSP_POLICY_FILE` | `policy/default-policy.json` | schema-validated policy file |
| `PSP_ADVERTISED_HOST` | `127.0.0.1` | host value returned in inspect port bindings |
| `PSP_KEEP_ON_FAILURE` | `false` | skip shutdown cleanup for debugging |
| `RUST_LOG` | unset | standard `tracing` filter |

## Supported endpoint surface

PSP v1 currently supports:

- daemon probes: `/_ping`, `/version`, `/info`
- image pull: `/images/create`
- container lifecycle: create, start, inspect, logs, wait, delete

Versioned forms like `/v1.41/containers/create` are also supported.

## Example local session

### Start PSP

```bash
cargo run --bin psp
```

### Pull an image through PSP

```bash
curl --unix-socket /tmp/psp.sock \
  -X POST \
  'http://d/v1.41/images/create?fromImage=postgres:16'
```

### Create a container through PSP

```bash
curl --unix-socket /tmp/psp.sock \
  -H 'content-type: application/json' \
  -H 'x-psp-session-id: demo-run-1' \
  -X POST \
  http://d/v1.41/containers/create?name=demo-db \
  -d '{
    "Image": "postgres:16",
    "Env": [
      "POSTGRES_PASSWORD=example"
    ]
  }'
```

### Start the container

```bash
curl --unix-socket /tmp/psp.sock \
  -H 'x-psp-session-id: demo-run-1' \
  -X POST \
  http://d/v1.41/containers/<container-id>/start
```

### Inspect rewritten port mapping

```bash
curl --unix-socket /tmp/psp.sock \
  -H 'x-psp-session-id: demo-run-1' \
  http://d/v1.41/containers/<container-id>/json
```

### Read logs

```bash
curl --unix-socket /tmp/psp.sock \
  -H 'x-psp-session-id: demo-run-1' \
  'http://d/v1.41/containers/<container-id>/logs?stdout=1&stderr=1'
```

### Remove the container

```bash
curl --unix-socket /tmp/psp.sock \
  -H 'x-psp-session-id: demo-run-1' \
  -X DELETE \
  'http://d/v1.41/containers/<container-id>?force=1'
```

## What PSP enforces

By default PSP denies:

- privileged containers
- host namespace joins
- bind mounts outside the allowlist
- device mounts
- capability additions
- image references blocked by policy

Every policy denial returns a stable `rule_id` and a clear message.

## Denial example

Request:

```bash
curl --unix-socket /tmp/psp.sock \
  -H 'content-type: application/json' \
  -X POST \
  http://d/v1.41/containers/create \
  -d '{
    "Image": "postgres:16",
    "HostConfig": {"Privileged": true}
  }'
```

Response:

```json
{
  "message": "privileged containers are denied by default",
  "kind": "policy_denied",
  "rule_id": "PSP-POL-001"
}
```

## Logs

PSP emits structured allow/deny logs. Start with:

```bash
RUST_LOG=info cargo run --bin psp
```

See:

- `docs/operations/audit-logging.md`

## Troubleshooting

### Client gets `501 unsupported_endpoint`

The request is outside the current Docker-compatible contract.

See:
- `docs/compatibility/testcontainers-profile.md`

### Client gets `403 policy_denied`

Inspect the returned `rule_id` and the PSP logs.

See:
- `docs/policy-reference.md`
- `docs/operations/audit-logging.md`
- `docs/adr/0001-psp-architecture.md`

### PSP fails on startup loading policy

Check:

- the file path in `PSP_POLICY_FILE`
- valid JSON syntax
- `version` equals `v1`
- bind mount allowlist entries are absolute paths

### Containers remain after debugging

If `PSP_KEEP_ON_FAILURE=true`, PSP intentionally skips shutdown cleanup. The next startup sweep will remove stale managed containers.

See:
- `docs/operations/session-lifecycle.md`

### Published port is unreachable

Verify `PSP_ADVERTISED_HOST` is reachable from the sandbox and inspect the rewritten port mapping returned by `/containers/{id}/json`.

See:
- `docs/operations/host-port-resolution.md`
