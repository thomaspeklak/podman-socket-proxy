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
  },
  "containers": {
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

### `containers.allowlist`

List of exact container identifiers or names that PSP may access when the target container was not created and managed by PSP itself.

Typical entries are added through discovery mode:

```json
{
  "containers": {
    "allowlist": ["shared-db", "1a2b3c4d5e6f"]
  }
}
```

Behavior notes:

- PSP-managed containers are always accessible
- pre-existing backend containers are denied by default unless explicitly allowlisted
- entries may be a full container ID, a stable short ID you choose to store, or a container name

### `containers.denylist`

List of exact container identifiers or names that PSP must block even if they would otherwise be accessible.

Example:

```json
{
  "containers": {
    "denylist": ["prod-db"]
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

For access to existing containers:

1. allow PSP-managed containers
2. check explicit denylist matches
3. require an explicit allowlist match for non-managed containers

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
| `PSP-POL-008` | container denied by denylist |
| `PSP-POL-009` | container missing from allowlist |

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
  },
  "containers": {
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
  },
  "containers": {
    "allowlist": [],
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
  },
  "containers": {
    "allowlist": [],
    "denylist": []
  }
}
```

### Allow access to a shared local database container

```json
{
  "version": "v1",
  "bind_mounts": {
    "allowlist": ["/workspace"]
  },
  "images": {
    "allowlist": ["postgres:16"],
    "denylist": []
  },
  "containers": {
    "allowlist": ["shared-db"],
    "denylist": ["prod-db"]
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

### Denied access to a discovered but unallowlisted container

Expected denial:

```json
{
  "kind": "policy_denied",
  "rule_id": "PSP-POL-009",
  "message": "container is not present in the allowlist: shared-db"
}
```

## Fast policy workflows

```bash
psp policy check policy/default-policy.json
psp policy explain --request-file /tmp/create.json
psp policy init /tmp/psp-policy.json --profile workspace-postgres
psp discover containers
psp discover allow
psp discover allow --project
psp discover allow shared-db
psp images search postgres
psp images allow postgres:16
```

## Example files in this repo

- `policy/default-policy.json`
- `docs/examples/policies/allow-workspace-postgres.json`
- `docs/examples/policies/debug-images-and-workspace.json`
