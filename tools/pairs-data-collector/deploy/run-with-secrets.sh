#!/usr/bin/env bash
set -euo pipefail

readonly token_file="${OP_SERVICE_ACCOUNT_TOKEN_FILE:-$HOME/.config/op/service-token}"
readonly env_file="${OP_ENV_FILE:-$HOME/.agent.env}"

if [[ $# -eq 0 ]]; then
    echo "usage: run-with-secrets.sh COMMAND [ARG ...]" >&2
    exit 2
fi
if [[ ! -r "$token_file" ]]; then
    echo "1Password service-account token is not readable: $token_file" >&2
    exit 1
fi
if [[ ! -r "$env_file" ]]; then
    echo "1Password environment file is not readable: $env_file" >&2
    exit 1
fi

OP_SERVICE_ACCOUNT_TOKEN="$(<"$token_file")"
export OP_SERVICE_ACCOUNT_TOKEN
exec op run --env-file "$env_file" --no-masking -- "$@"
