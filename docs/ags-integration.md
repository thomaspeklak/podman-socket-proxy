# AGS integration contract for PSP

AGS integrates PSP as an **external dependency**. PSP owns runtime mediation; AGS owns integration glue.

This document describes the contract AGS should rely on when PSP mode is enabled.

## Core contract

In PSP mode, AGS should:

1. start or connect to a PSP instance
2. set `DOCKER_HOST` to the PSP Unix socket
3. propagate a stable session identifier using `x-psp-session-id`
4. avoid mounting the host Podman socket directly into the sandbox
5. treat PSP as a documented compatibility surface, not as full Docker parity

## Minimal environment example

```bash
export PSP_BACKEND="unix://$XDG_RUNTIME_DIR/podman/podman.sock"
export PSP_LISTEN_SOCKET="/tmp/psp.sock"
export PSP_POLICY_FILE="policy/default-policy.json"
export PSP_ADVERTISED_HOST="127.0.0.1"
export DOCKER_HOST="unix:///tmp/psp.sock"
```

See also:

- `docs/examples/ags/psp-env.sh`

## Session propagation

AGS should generate a stable session ID per sandbox or test run and attach it to PSP-bound requests as:

```text
x-psp-session-id: <session-id>
```

If operators enable `PSP_REQUIRE_SESSION_ID=true`, mutating requests without a valid session ID are rejected.

PSP uses this for:

- resource labeling
- audit log attribution
- shutdown cleanup tracking
- crash-recovery ownership context
- client-visible effective session diagnostics via `x-psp-effective-session-id`

## AGS responsibilities vs PSP responsibilities

### AGS responsibilities
- launch or discover PSP
- wire `DOCKER_HOST`
- propagate session ID
- surface PSP allow/deny outcomes to users
- keep PSP mode explicit and configurable

### PSP responsibilities
- accept only the documented Docker-compatible subset
- enforce policy before forwarding
- label resources
- rewrite inspect host mappings for client usability
- emit structured audit logs
- sweep stale managed containers on startup

## Deny handling contract

When PSP blocks a request, AGS should preserve and surface at least:

- `kind`
- `message`
- `rule_id` when present
- `hint` when present
- `request_id`

This allows AGS to distinguish:

- unsupported endpoint (`501 unsupported_endpoint`)
- policy denial (`403 policy_denied`)
- missing required session (`400 session_required`)
- backend error (`502 backend_error`)
- internal PSP error (`500 internal_error`)

## Example failure surface

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

## Example AGS request flow

1. AGS starts sandbox session `sess-42`
2. AGS ensures PSP is running on `/tmp/psp.sock`
3. AGS sets `DOCKER_HOST=unix:///tmp/psp.sock`
4. AGS configures its Docker-compatible client to send `x-psp-session-id: sess-42`
5. client issues `POST /v1.41/containers/create`
6. PSP injects labels:
   - `io.psp.managed=true`
   - `io.psp.session=sess-42`
7. on shutdown, PSP cleans up tracked containers for the process unless debug retention is enabled

## Compatibility expectations

AGS should assume only the documented PSP compatibility profile.

See:

- `docs/compatibility/testcontainers-profile.md`

## Versioning note

The integration contract is currently versioned by repository documentation and tests together. Changes to headers, session semantics, deny response shape, cleanup behavior, or endpoint support should be treated as contract changes and updated in:

- docs
- integration tests
- AGS integration glue
