# PSP getting started

This guide is the fastest path to running PSP locally, validating that it works, and understanding the most important knobs.

## What you need

- Rust toolchain
- rootless Podman with an accessible API socket
- a writable Unix socket path for PSP
- `curl` for quick manual validation

## Step 1: verify the Podman socket

Typical rootless location:

```bash
echo "$XDG_RUNTIME_DIR/podman/podman.sock"
```

If needed, confirm the socket exists:

```bash
ls -l "$XDG_RUNTIME_DIR/podman/podman.sock"
```

## Step 2: build and test PSP

```bash
cargo build
cargo test
```

## Step 3: start PSP

```bash
export PSP_BACKEND="unix://$XDG_RUNTIME_DIR/podman/podman.sock"
export PSP_LISTEN_SOCKET="/tmp/psp.sock"
export PSP_POLICY_FILE="policy/default-policy.json"
export PSP_ADVERTISED_HOST="127.0.0.1"
export RUST_LOG=info

cargo run --bin psp -- run
```

You can also use the helper commands:

```bash
cargo run --bin psp -- doctor
cargo run --bin psp -- config show
cargo run --bin psp -- smoke-test --image postgres:16
```

### Optional: move repeated settings into config files

PSP loads config in this precedence order:

1. built-in defaults
2. `~/.config/psp/config.json`
3. `<repo-root>/.psp.json`
4. environment variables

Example global config:

```json
{
  "backend": "unix:///run/user/1000/podman/podman.sock",
  "listen_socket": "/tmp/psp.sock",
  "advertised_host": "127.0.0.1",
  "require_session_id": false
}
```

Example project config:

```json
{
  "policy_path": "policy/default-policy.json",
  "advertised_host": "host.containers.internal",
  "require_session_id": true
}
```

PSP walks upward from the current directory to find `.git`. In git worktrees it resolves the shared repository root and loads `.psp.json` from there.

## Step 4: point clients at PSP

For Docker-compatible clients:

```bash
export DOCKER_HOST="unix:///tmp/psp.sock"
```

For manual HTTP checks with `curl`:

```bash
curl --unix-socket /tmp/psp.sock http://d/_ping
curl --unix-socket /tmp/psp.sock http://d/version
```

## Step 5: create a test session

PSP accepts a session header:

```text
x-psp-session-id: demo-session-1
```

If `PSP_REQUIRE_SESSION_ID=true`, mutating requests without a valid session ID are rejected with `400 session_required`.

Example:

```bash
curl --unix-socket /tmp/psp.sock \
  -H 'content-type: application/json' \
  -H 'x-psp-session-id: demo-session-1' \
  -X POST \
  http://d/v1.41/containers/create \
  -d '{"Image":"postgres:16"}'
```

PSP will inject its own management labels before forwarding to Podman.

## Step 6: understand the defaults

### Default policy file

`policy/default-policy.json` is intentionally conservative.

### Default advertised host

PSP rewrites inspect port bindings to:

```text
127.0.0.1
```

Override it if the sandbox should connect through a different hostname.

### Default cleanup behavior

- stale PSP-managed containers are removed on startup
- tracked containers are removed on normal shutdown

To keep resources for debugging:

```bash
export PSP_KEEP_ON_FAILURE=true
```

## First troubleshooting checks

### `501 unsupported_endpoint`

The client requested an API endpoint outside the documented PSP contract.

See:
- `docs/compatibility/testcontainers-profile.md`

### `403 policy_denied`

The request matched a policy rule. Inspect `rule_id`, `hint`, and `request_id` in the response and correlate with the PSP logs.

See:
- `docs/policy-reference.md`
- `docs/operations/audit-logging.md`

### Port mapping looks wrong

Inspect the container through PSP, not directly through Podman, so you see the rewritten host mapping.

See:
- `docs/operations/host-port-resolution.md`

## Next reads

- `docs/operations/runbook.md`
- `docs/examples/http-api-examples.md`
- `docs/ags-integration.md`
