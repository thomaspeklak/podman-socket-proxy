# podman-socket-proxy

`podman-socket-proxy` (binary: `psp`) is a policy-gated, Docker-compatible proxy that sits between sandboxed clients and a rootless Podman API.

## Why this exists
Raw Podman socket mounts are functional for Testcontainers but weaken sandbox isolation.
PSP adds an enforcement layer so integration tests can run with explicit, auditable policy controls.

## Project status
Planning is tracked in Beads epic `psp-287`.
Execution starts with `psp-287.1` (ADR + threat model).

## Scope
- Docker-compatible API surface for Testcontainers-critical flows
- Policy engine (deny-by-default)
- Session lifecycle + cleanup
- Audit/deny diagnostics

## Non-goals
- Replacing AGS
- Owning AGS internals
- Cloud multi-tenant control plane

## Relationship to AGS
AGS should integrate PSP as an external dependency and keep only integration glue in the AGS repo.
