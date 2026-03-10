# PSP host/port resolution strategy

- Related issue: `psp-287.5`

## Problem

Test code running inside the sandbox needs a deterministic way to connect to ports published by Podman-managed containers.

With Podman, inspect responses and published-port metadata may advertise wildcard or backend-oriented addresses such as `0.0.0.0`. That is not always the most useful value for the sandbox client when it needs an actual connect target.

## PSP v1 strategy

PSP rewrites `NetworkSettings.Ports[*].HostIp` in container inspect responses to a configurable advertised host value.

### Default

- `PSP_ADVERTISED_HOST=127.0.0.1`

### Override

Set `PSP_ADVERTISED_HOST` when the sandbox reaches published ports through a different host name or address.

Examples:

```bash
PSP_ADVERTISED_HOST=127.0.0.1
PSP_ADVERTISED_HOST=host.containers.internal
PSP_ADVERTISED_HOST=<sandbox-reachable-hostname>
```

PSP preserves the backend-selected `HostPort` value and only normalizes the connect host exposed to the client.

## Why this works

- random host ports remain supported because Podman still chooses the host port
- parallel test execution remains safe because each inspect response carries the actual mapped port
- sandbox clients get a stable host value instead of a wildcard listener address

## Validation

The broker test suite includes a reproducible connectivity test that:

1. injects an inspect response with wildcard host bindings
2. verifies PSP rewrites those bindings to `127.0.0.1`
3. connects to multiple published ports in parallel using the rewritten values

## Troubleshooting

### Inspect shows the right port but connections fail

- verify `PSP_ADVERTISED_HOST` is reachable from the sandbox
- confirm the service is actually listening and healthy
- confirm host firewall/network policy permits the connection

### Inspect still shows `0.0.0.0`

- ensure the client is querying inspect through PSP rather than directly against Podman
- confirm the endpoint is `/containers/{id}/json`
- confirm the response contains `NetworkSettings.Ports`

### Parallel tests collide on ports

- prefer random host ports instead of fixed host port assignments
- inspect the rewritten `HostPort` values returned per container
- avoid assuming a single static port across concurrent runs
