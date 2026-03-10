# ADR 0001: PSP architecture and trust boundaries

- Status: Accepted
- Date: 2026-03-10
- Related issue: `psp-287.1`

## Context

Sandboxed agents need to run integration tests, including Testcontainers-style workflows, while preserving meaningful isolation from host resources.

Directly mounting the host Podman socket into sandbox containers is functionally effective but collapses an important trust boundary: the sandbox can ask Podman to create arbitrary containers, attach host mounts, request elevated runtime flags, and persist resources beyond the sandbox lifetime under the host user account.

PSP exists to preserve compatibility with Docker-oriented tooling without granting raw runtime control to an untrusted sandbox workload.

## Decision

Build **PSP (Podman Socket Proxy)** as a standalone process that:

1. Exposes a constrained Docker-compatible API endpoint to sandbox clients.
2. Enforces deny-by-default policy before forwarding any operation.
3. Talks to the host's rootless Podman API on behalf of clients.
4. Emits structured audit logs for allow and deny decisions.
5. Applies session labels and deterministic cleanup semantics to all managed resources.
6. Returns stable deny rule IDs and human-readable reasons for blocked requests.

## Why brokered access instead of raw socket mounting

### Raw Podman socket mount

Mounting the host Podman socket into the sandbox is rejected as the default architecture because it gives the sandbox indirect control over the host user's container runtime. Even in rootless mode, that allows requests such as:

- bind mounting sensitive host paths available to the host user
- starting privileged or near-privileged containers if the backend permits it
- joining host namespaces or publishing unrestricted ports
- creating long-lived resources outside the sandbox lifecycle
- probing the runtime for information unrelated to the test session

This is too much authority to hand to an untrusted workload origin.

### PSP broker

A brokered proxy keeps Docker/Testcontainers compatibility while restoring an explicit policy boundary:

- every request is inspected before it reaches Podman
- unsupported or dangerous options are denied by default
- resource ownership is attributed to a PSP session
- cleanup is centralized and auditable
- implementation scope is explicit rather than inheriting the full Podman API surface

## Scope (v1)

- Container lifecycle operations required by Testcontainers core flows.
- Basic image operations required for pull and inspect.
- Network and port behavior needed for service connectivity from sandbox tests.
- Policy enforcement for high-risk options such as privileged mode, host namespaces, devices, capabilities, and unrestricted bind mounts.
- Session metadata propagation, labeling, and cleanup of PSP-managed resources.
- Structured audit logging for allow and deny outcomes.

## Non-goals (v1)

- Full Docker API parity.
- Multi-tenant authentication and authorization platform.
- Cloud control plane or managed service model.
- General-purpose container brokerage for arbitrary host automation.
- Replacing AGS or taking ownership of AGS internal planning/execution logic.

## Trust boundaries

### 1. Sandbox client

The sandbox client is the **untrusted request origin**. It may be buggy, compromised, or adversarial. All request content from this boundary must be treated as untrusted input.

### 2. PSP process

PSP is the **policy decision and mediation boundary**. It terminates the Docker-compatible API presented to the sandbox, validates requests, applies policy, labels resources, forwards allowed requests, and records audit events.

### 3. Host rootless Podman API

Podman is the **execution backend**. It is trusted to perform requested operations correctly, but it is not trusted to enforce PSP's higher-level sandbox policy goals. PSP must therefore prevent unsafe requests before forwarding them.

### 4. Host filesystem, network exposure, and user resources

These are the **assets to protect**. Rootless Podman reduces blast radius versus rootful operation, but host-user-readable files, network access, local sockets, and long-lived runtime resources still require explicit protection.

## Security assumptions

- Podman runs in **rootless** mode as the host user.
- Sandbox clients can reach PSP but do not get direct access to the Podman socket in PSP mode.
- PSP can add labels/metadata to all resources it creates or manages.
- Deny-by-default is acceptable for unsupported or ambiguous request shapes.
- Logs must avoid plaintext secret disclosure.

## Abuse case matrix

| ID | Abuse case | Example request/behavior | Impact if allowed | Mitigation in PSP | Residual risk |
|---|---|---|---|---|---|
| AC-01 | Arbitrary host bind mount | Mount `~/.ssh`, git checkout metadata, or local service sockets into a test container | Host secret exfiltration, lateral access, credential theft | Deny bind mounts by default; later allowlist explicit safe paths only; log stable rule ID | Over-broad allowlists remain dangerous |
| AC-02 | Privileged container creation | `HostConfig.Privileged=true` or equivalent Podman flags | Expanded kernel/device access relative to rootless limits; policy bypass attempts | Hard deny privileged mode with stable rule ID | Backend semantics can drift; compatibility tests must watch this |
| AC-03 | Host namespace escape | `NetworkMode=host`, host PID/IPC/UTS namespace joins | Host/service interference, information leakage, traffic capture | Hard deny host namespace joins and namespace-sharing options | Some safe-looking aliases may need normalization to catch |
| AC-04 | Device and capability escalation | Add `/dev/*`, `CapAdd=ALL`, unsafe individual caps | Wider attack surface against kernel or host services | Deny all devices by default; deny capability adds by default in v1 | Future allowlists must be narrowly scoped |
| AC-05 | Unrestricted port publishing | Publish broad host ports or wildcard listeners | Unexpected host exposure, port collisions, service reachability outside intended scope | Restrict port publishing policy; prefer ephemeral, session-attributed mappings | Podman port behavior differences may need explicit handling |
| AC-06 | Resource exhaustion / denial of service | Pull huge images, create many containers/networks, omit cleanup | Disk, CPU, memory, and namespace exhaustion on host user account | Session quotas later; v1 uses session labels, startup sweep, and TTL GC | Burst exhaustion remains possible before cleanup |
| AC-07 | Secret leakage via logs | Auth headers, env vars, pull creds, tokens included in request body | Secrets persist in logs and diagnostics | Redact known sensitive fields; log structured metadata instead of raw payloads | Unknown secret-shaped fields may still slip through |
| AC-08 | Use of unsupported API surface | Invoke arbitrary Docker endpoints that PSP does not model safely | Proxy accidentally forwards unsafe operations or behaves inconsistently | Explicit endpoint allowlist; deny unsupported endpoints with structured error | Clients may need more compatibility work |
| AC-09 | Session confusion or takeover | Reuse another session's container IDs or infer shared resources | Cross-session interference, incorrect cleanup, information leaks | Label all PSP-managed resources with session identity; scope lookups and cleanup by label | Mislabeling bugs could break isolation |
| AC-10 | Persistence beyond sandbox lifetime | Sandbox exits without cleanup; resources keep running | Long-lived containers/networks continue consuming resources or exposing services | Deterministic cleanup on session end, startup sweep of stale labeled resources, TTL-based GC | Cleanup can lag if PSP crashes at the wrong time |
| AC-11 | Backend information disclosure | Inspect unrelated images/containers or enumerate runtime state | Leakage of host runtime metadata | Restrict inspect/list operations to PSP-managed resources where possible | Some backend responses may contain incidental metadata |
| AC-12 | Policy bypass through field encoding quirks | Dangerous options hidden in alternate JSON shapes/defaults | Unsafe request slips through validation | Normalize request payloads before evaluation; reject ambiguous input | New API variants may require ongoing hardening |

## Policy baseline derived from this ADR

The following baseline rules are mandatory for v1 implementation:

1. **Endpoint allowlist**: only explicitly supported Docker-compatible endpoints are accepted.
2. **Deny-by-default options**: privileged mode, host namespaces, devices, and capability adds are denied unless a future rule explicitly allows them.
3. **Bind mount restrictions**: host bind mounts are denied by default.
4. **Session attribution**: PSP-managed resources must carry stable session labels.
5. **Auditable denials**: every deny response must include a stable rule ID and clear reason.
6. **Secret-safe logging**: request/response logging must redact sensitive material.
7. **Cleanup ownership**: PSP is responsible for cleaning up stale labeled resources created under its control.

## Alternatives considered

### A) Raw Podman socket mount into sandbox

- Pros: simplest integration path; maximum compatibility with Docker-oriented clients.
- Cons: removes meaningful mediation and gives the sandbox broad runtime authority as the host user.
- Decision: rejected as the default architecture.

### B) Podman-in-Podman

- Pros: stronger separation from the host socket and potentially cleaner containment boundaries.
- Cons: significantly more operational complexity and fragility in rootless/containerized environments; image/network performance concerns.
- Decision: deferred, not selected for v1.

### C) External test runner outside sandbox

- Pros: strongest separation from the sandbox itself.
- Cons: weakens the self-contained sandbox workflow and changes the developer/operator model substantially.
- Decision: acceptable fallback path, not the primary v1 architecture.

## Consequences

### Positive

- Better isolation than raw socket mounting.
- Explicit, reviewable policy boundary.
- Centralized cleanup and auditability.
- Clear ownership split: AGS integrates PSP, PSP owns runtime mediation.

### Trade-offs

- Additional component to build, configure, and operate.
- Ongoing compatibility work against Docker and Testcontainers expectations.
- Some legitimate workloads will be blocked until policy and compatibility support are intentionally expanded.

## Residual risks

The following risks remain even after adopting PSP:

- Rootless Podman still operates with the host user's authority; PSP reduces but does not eliminate host-user risk.
- Misconfigured allowlists can reintroduce host access paths.
- Incomplete request normalization could leave policy bypass gaps.
- Cleanup is best-effort in the presence of process crashes or host instability.
- Compatibility pressure may encourage unsafe exceptions unless rule changes remain explicit and reviewed.

These risks are accepted for v1 and must remain visible in implementation and operations docs.

## Integration contract with AGS (high level)

- AGS treats PSP as an external dependency.
- AGS injects the PSP endpoint through `DOCKER_HOST` in explicit PSP mode.
- AGS does **not** directly mount the host Podman socket when PSP mode is enabled.
- Session identity must be propagated from AGS to PSP in a stable, explicit form.
- PSP internals remain outside AGS repo scope.

## Implementation implications for follow-on issues

- `psp-287.2` must implement an endpoint allowlist, request normalization, session labels, and deny/error mapping.
- `psp-287.3` must define the policy schema, stable rule IDs, and default deny rules matching this ADR.
- `psp-287.4` must identify the minimum Docker/Testcontainers endpoint set required for supported scenarios.
- Tests must cover both allowed flows and blocked high-risk requests.

## Open questions

1. What is the minimum endpoint set required by Rust `testcontainers` for the target supported modules?
2. How should PSP represent session identity on the wire and in labels?
3. What is the default host/port resolution strategy from an in-sandbox test process to a PSP-managed container?
4. Which request fields require canonicalization before policy evaluation to avoid bypasses?
5. What quota controls are necessary after the MVP to reduce resource exhaustion risk?

## Next steps

- Complete compatibility matrix (`psp-287.4`).
- Define policy schema and baseline rules (`psp-287.3`).
- Implement broker MVP with contract tests (`psp-287.2`).
- Add operator-facing policy and audit log documentation once implementation starts.
