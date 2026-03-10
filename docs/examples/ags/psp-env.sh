#!/usr/bin/env bash
set -euo pipefail

export PSP_BACKEND="unix://${XDG_RUNTIME_DIR}/podman/podman.sock"
export PSP_LISTEN_SOCKET="/tmp/psp.sock"
export PSP_POLICY_FILE="policy/default-policy.json"
export PSP_ADVERTISED_HOST="127.0.0.1"
export DOCKER_HOST="unix:///tmp/psp.sock"

# AGS should also propagate a stable PSP session header on requests:
#   x-psp-session-id: <session-id>
