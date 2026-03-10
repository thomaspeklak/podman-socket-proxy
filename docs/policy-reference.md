# PSP policy file reference

The default policy file is:

- `policy/default-policy.json`

PSP validates the policy file on startup.

## Schema

Current schema version:

```json
{
  "version": "v1",
  "bind_mounts": {
    "allowlist": []
  },
  "images": {
    "allowlist": [],
    "denylist": []
  }
}
```

## Top-level fields

### `version`

Required.

Must currently be:

```json
"v1"
```

### `bind_mounts.allowlist`

List of absolute host-path prefixes allowed for bind mounts.

If a bind source is not under an allowlisted prefix, PSP denies the request.

Example:

```json
{
  "bind_mounts": {
    "allowlist": ["/workspace", "/tmp/psp-share"]
  }
}
```

Behavior notes:

- entries must start with `/`
- `/workspace` allows `/workspace` and `/workspace/...`
- relative paths are invalid
- an empty allowlist means all bind mounts are denied

### `images.allowlist`

If non-empty, only listed image references are allowed.

Example:

```json
{
  "images": {
    "allowlist": ["postgres:16", "redis:7"]
  }
}
```

### `images.denylist`

Any exact image reference listed here is denied.

Example:

```json
{
  "images": {
    "denylist": ["alpine:latest"]
  }
}
```

## Evaluation order

For image policy:

1. denylist is checked first
2. if allowlist is non-empty, the image must be present in the allowlist

For container create policy:

1. validate JSON payload shape
2. deny privileged mode
3. deny host namespace modes
4. deny device mounts
5. deny capability additions
6. deny non-allowlisted bind mounts
7. apply image policy

## Stable rule IDs

| Rule ID | Meaning |
|---|---|
| `PSP-POL-000` | invalid container create payload |
| `PSP-POL-001` | privileged containers denied |
| `PSP-POL-002` | host namespace modes denied |
| `PSP-POL-003` | bind mount source not allowlisted |
| `PSP-POL-004` | device mounts denied |
| `PSP-POL-005` | capability additions denied |
| `PSP-POL-006` | image denied by denylist |
| `PSP-POL-007` | image missing from allowlist |

## Concrete examples

### Minimal deny-by-default policy

```json
{
  "version": "v1",
  "bind_mounts": {
    "allowlist": []
  },
  "images": {
    "allowlist": [],
    "denylist": []
  }
}
```

### Workspace-only bind mounts + approved images

```json
{
  "version": "v1",
  "bind_mounts": {
    "allowlist": ["/workspace"]
  },
  "images": {
    "allowlist": ["postgres:16", "redis:7"],
    "denylist": []
  }
}
```

### Explicitly deny a risky image tag

```json
{
  "version": "v1",
  "bind_mounts": {
    "allowlist": ["/workspace"]
  },
  "images": {
    "allowlist": [],
    "denylist": ["alpine:latest"]
  }
}
```

## Request examples

### Allowed bind mount

```json
{
  "Image": "postgres:16",
  "HostConfig": {
    "Binds": ["/workspace/tmp:/tmp"]
  }
}
```

Assuming `/workspace` is allowlisted, this is allowed.

### Denied bind mount

```json
{
  "Image": "postgres:16",
  "HostConfig": {
    "Binds": ["/home/tom/.ssh:/root/.ssh:ro"]
  }
}
```

Expected denial:

```json
{
  "kind": "policy_denied",
  "rule_id": "PSP-POL-003",
  "message": "bind mount source is not allowlisted: /home/tom/.ssh"
}
```

### Denied privileged create

```json
{
  "Image": "postgres:16",
  "HostConfig": {
    "Privileged": true
  }
}
```

Expected denial:

```json
{
  "kind": "policy_denied",
  "rule_id": "PSP-POL-001",
  "message": "privileged containers are denied by default"
}
```

## Example files in this repo

- `policy/default-policy.json`
- `docs/examples/policies/allow-workspace-postgres.json`
- `docs/examples/policies/debug-images-and-workspace.json`
