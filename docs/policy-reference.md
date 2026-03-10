# PSP policy file reference

The default policy file is:

- `policy/default-policy.json`

## Schema version

```json
{
  "version": "v1"
}
```

PSP validates the policy schema version on startup.

## Bind mount policy

```json
{
  "bind_mounts": {
    "allowlist": ["/workspace"]
  }
}
```

Only bind sources under listed absolute paths are allowed.

## Image policy

```json
{
  "images": {
    "allowlist": ["postgres:16", "redis:7"],
    "denylist": ["alpine:latest"]
  }
}
```

- if `allowlist` is non-empty, images outside it are denied
- any image in `denylist` is denied

## Stable rule IDs

- `PSP-POL-001` privileged containers denied
- `PSP-POL-002` host namespace modes denied
- `PSP-POL-003` bind mount source not allowlisted
- `PSP-POL-004` device mounts denied
- `PSP-POL-005` capability additions denied
- `PSP-POL-006` image denied by denylist
- `PSP-POL-007` image missing from allowlist
