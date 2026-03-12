# PSP HTTP API examples

These examples use `curl` against the PSP Unix socket.

Assumptions:

- PSP listens on `/tmp/psp.sock`
- a Docker-compatible client would normally use `DOCKER_HOST=unix:///tmp/psp.sock`

## Probe daemon health

```bash
curl --unix-socket /tmp/psp.sock http://d/_ping
```

## Query version

```bash
curl --unix-socket /tmp/psp.sock http://d/version
```

## Pull an image

```bash
curl --unix-socket /tmp/psp.sock \
  -X POST \
  'http://d/v1.41/images/create?fromImage=postgres:16'
```

## Create a container

```bash
curl --unix-socket /tmp/psp.sock \
  -H 'content-type: application/json' \
  -H 'x-psp-session-id: example-session-1' \
  -X POST \
  http://d/v1.41/containers/create?name=example-db \
  -d '{
    "Image": "postgres:16",
    "Env": [
      "POSTGRES_PASSWORD=example"
    ]
  }'
```

## Create a container with an allowlisted bind mount

This requires the policy to allow `/workspace`.

```bash
curl --unix-socket /tmp/psp.sock \
  -H 'content-type: application/json' \
  -H 'x-psp-session-id: example-session-2' \
  -X POST \
  http://d/v1.41/containers/create \
  -d '{
    "Image": "postgres:16",
    "HostConfig": {
      "Binds": ["/workspace/tmp:/tmp"]
    }
  }'
```

## Start a container

```bash
curl --unix-socket /tmp/psp.sock \
  -H 'x-psp-session-id: example-session-1' \
  -X POST \
  http://d/v1.41/containers/<container-id>/start
```

## Inspect a container

```bash
curl --unix-socket /tmp/psp.sock \
  -H 'x-psp-session-id: example-session-1' \
  http://d/v1.41/containers/<container-id>/json
```

The response may contain port mappings whose `HostIp` was rewritten by PSP.

## Read logs

```bash
curl --unix-socket /tmp/psp.sock \
  -H 'x-psp-session-id: example-session-1' \
  'http://d/v1.41/containers/<container-id>/logs?stdout=1&stderr=1'
```

## Wait for exit

```bash
curl --unix-socket /tmp/psp.sock \
  -H 'x-psp-session-id: example-session-1' \
  -X POST \
  http://d/v1.41/containers/<container-id>/wait
```

## Remove a container

```bash
curl --unix-socket /tmp/psp.sock \
  -H 'x-psp-session-id: example-session-1' \
  -X DELETE \
  'http://d/v1.41/containers/<container-id>?force=1'
```

## Example blocked request: privileged container

```bash
curl --unix-socket /tmp/psp.sock \
  -H 'content-type: application/json' \
  -H 'x-psp-session-id: blocked-session-1' \
  -X POST \
  http://d/v1.41/containers/create \
  -d '{
    "Image": "postgres:16",
    "HostConfig": {
      "Privileged": true
    }
  }'
```

Expected response:

```json
{
  "message": "privileged containers are denied by default",
  "kind": "policy_denied",
  "rule_id": "PSP-POL-001",
  "hint": "Remove HostConfig.Privileged or change policy intentionally if this is expected.",
  "docs": "docs/policy-reference.md",
  "request_id": "psp-00000001",
  "session_id": "blocked-session-1"
}
```

## Example blocked request: unsupported endpoint

```bash
curl --unix-socket /tmp/psp.sock \
  -H 'content-type: application/json' \
  -X POST \
  http://d/v1.41/networks/create \
  -d '{"Name":"example-net"}'
```

Expected response:

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
