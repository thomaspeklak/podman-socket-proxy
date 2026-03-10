# podman-socket-proxy

`podman-socket-proxy` (binary: `psp`) is a policy-gated, Docker-compatible proxy that sits between sandboxed clients and a rootless Podman API.

## Why this exists
Raw Podman socket mounts are functional for Testcontainers but weaken sandbox isolation.
PSP adds an enforcement layer so integration tests can run with explicit, auditable policy controls.

## Project status
Planning is tracked in Beads epic `psp-287`.
Core architecture ADR: `docs/adr/0001-psp-architecture.md`
Compatibility profile: `docs/compatibility/testcontainers-profile.md`

## Scope
- Docker-compatible API surface for Testcontainers-critical flows
- Policy engine (deny-by-default)
- Session lifecycle + cleanup
- Audit/deny diagnostics

## Current implementation
- Unix socket listener for `DOCKER_HOST=unix://...` style clients
- Rootless Podman backend forwarding via Unix socket or HTTP base URL
- Versioned Docker-path allowlist for core lifecycle operations
- Schema-validated policy file at `policy/default-policy.json`
- Stable deny rule IDs for blocked privileged, namespace, bind mount, device, capability, and image-policy requests
- Inspect-response host rewriting for sandbox-reachable published ports (`PSP_ADVERTISED_HOST`)

## Non-goals
- Replacing AGS
- Owning AGS internals
- Cloud multi-tenant control plane

## Relationship to AGS
AGS should integrate PSP as an external dependency and keep only integration glue in the AGS repo.
