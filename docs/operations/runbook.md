# PSP operator runbook

- Related issue: `psp-287.9`

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

## Run PSP locally

```bash
export PSP_BACKEND="unix://$XDG_RUNTIME_DIR/podman/podman.sock"
export PSP_LISTEN_SOCKET="/tmp/psp.sock"
export PSP_POLICY_FILE="policy/default-policy.json"
export PSP_ADVERTISED_HOST="127.0.0.1"

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

## Validate locally

Run the full PSP suite:

```bash
cargo test
```

Run the dedicated integration suite only:

```bash
cargo test --test integration_suite
```

## Key configuration knobs

| Variable | Default | Purpose |
|---|---|---|
| `PSP_BACKEND` | `unix://$XDG_RUNTIME_DIR/podman/podman.sock` | Podman backend endpoint |
| `PSP_LISTEN_SOCKET` | `/tmp/psp.sock` | client-facing Unix socket |
| `PSP_POLICY_FILE` | `policy/default-policy.json` | schema-validated policy file |
| `PSP_ADVERTISED_HOST` | `127.0.0.1` | host value returned in inspect port bindings |
| `PSP_KEEP_ON_FAILURE` | `false` | skip shutdown cleanup for debugging |

## What PSP enforces

By default PSP denies:

- privileged containers
- host namespace joins
- bind mounts outside the allowlist
- device mounts
- capability additions
- image references blocked by policy

Every policy denial returns a stable `rule_id` and a clear message.

## Troubleshooting

### Client gets `501 unsupported_endpoint`

The request is outside the current Docker-compatible contract.

See:

- `docs/compatibility/testcontainers-profile.md`

### Client gets `403 policy_denied`

Inspect the returned `rule_id` and the PSP logs.

See:

- `docs/operations/audit-logging.md`
- `docs/adr/0001-psp-architecture.md`

### Containers remain after debugging

If `PSP_KEEP_ON_FAILURE=true`, PSP intentionally skips shutdown cleanup. The next startup sweep will remove stale managed containers.

See:

- `docs/operations/session-lifecycle.md`

### Published port is unreachable

Verify `PSP_ADVERTISED_HOST` is reachable from the sandbox and inspect the rewritten port mapping returned by `/containers/{id}/json`.

See:

- `docs/operations/host-port-resolution.md`
