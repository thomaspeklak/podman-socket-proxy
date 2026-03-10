# AGS integration contract for PSP

- Related issue: `psp-287.9`

## Contract summary

AGS integrates PSP as an **external dependency**. PSP owns runtime mediation; AGS owns integration glue.

## Required AGS behavior in PSP mode

1. Start or connect to a PSP instance.
2. Set `DOCKER_HOST` to the PSP Unix socket.
3. Propagate a stable session identifier using `x-psp-session-id`.
4. Do **not** mount the host Podman socket directly into the sandbox.

## Minimal environment example

```bash
export PSP_BACKEND="unix://$XDG_RUNTIME_DIR/podman/podman.sock"
export PSP_LISTEN_SOCKET="/tmp/psp.sock"
export DOCKER_HOST="unix:///tmp/psp.sock"
```

## Session propagation

AGS should generate a stable session ID per sandbox/test run and send it on PSP-bound requests as:

```text
x-psp-session-id: <session-id>
```

PSP uses this for:

- resource labeling
- audit log attribution
- cleanup ownership

## Compatibility expectations

AGS should assume only the documented PSP compatibility profile, not general Docker API parity.

See:

- `docs/compatibility/testcontainers-profile.md`

## Deny handling

When PSP blocks a request, AGS should surface:

- `kind`
- `message`
- `rule_id` when present

This lets users distinguish unsupported features from explicit policy denials.

## Versioning note

For now, the integration contract is documented in-repo alongside the PSP compatibility profile and ADR. Changes to request headers, cleanup semantics, or supported endpoint behavior should be treated as contract changes and reflected in docs and tests together.
