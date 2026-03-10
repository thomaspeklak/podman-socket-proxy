# PSP host/port resolution strategy

Test code running inside a sandbox needs a deterministic way to connect to ports published by Podman-managed containers.

## The problem

Backend inspect responses may advertise wildcard or backend-oriented addresses such as `0.0.0.0`. That is not always the most useful connect target for the sandbox client.

A test client usually wants:

- the real mapped host port chosen by the runtime
- a stable host value it can actually connect to from the sandbox

## PSP strategy

PSP rewrites:

```text
NetworkSettings.Ports[*].HostIp
```

in container inspect responses to a configurable advertised host.

## Default

```text
PSP_ADVERTISED_HOST=127.0.0.1
```

## Override examples

Use a different value when the sandbox should connect through another hostname or address:

```bash
export PSP_ADVERTISED_HOST=127.0.0.1
export PSP_ADVERTISED_HOST=host.containers.internal
export PSP_ADVERTISED_HOST=sandbox-gateway.local
```

PSP preserves the backend-selected `HostPort`. It only normalizes the host component presented to the client.

## Example

### Backend inspect response

```json
{
  "NetworkSettings": {
    "Ports": {
      "5432/tcp": [
        {"HostIp": "0.0.0.0", "HostPort": "15432"}
      ]
    }
  }
}
```

### PSP inspect response with `PSP_ADVERTISED_HOST=127.0.0.1`

```json
{
  "NetworkSettings": {
    "Ports": {
      "5432/tcp": [
        {"HostIp": "127.0.0.1", "HostPort": "15432"}
      ]
    }
  }
}
```

## Why this works well

- random host ports remain supported because Podman still chooses the port
- parallel test execution remains safe because each inspect response carries the actual mapped port
- the client gets a connectable host instead of a wildcard listener address

## Validation in this repo

The test suite includes a reproducible connectivity test that:

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
