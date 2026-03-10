# PSP session lifecycle and cleanup

- Related issue: `psp-287.6`

## Session identity

PSP accepts an optional request header:

- `x-psp-session-id`

If absent, PSP uses `anonymous`.

## Resource labeling

For container create requests, PSP injects labels:

- `io.psp.managed=true`
- `io.psp.session=<session-id>`

These labels provide ownership and cleanup scoping for PSP-managed resources.

## Cleanup behavior

### Startup sweep

On startup, PSP queries Podman for stale containers labeled `io.psp.managed=true` and force-removes them before accepting new requests.

This is the crash-recovery path for containers left behind by abnormal exits.

### Shutdown cleanup

On normal shutdown, PSP force-removes containers tracked in the current process session state.

### Debug override

Set:

```bash
PSP_KEEP_ON_FAILURE=true
```

to skip shutdown cleanup and leave managed containers behind for debugging.

## Operational notes

- repeated PSP runs should not accumulate stale managed containers
- if shutdown cleanup is disabled, startup sweep will remove those stale containers on the next PSP start
- session labeling is applied at the broker boundary, not delegated to the client
