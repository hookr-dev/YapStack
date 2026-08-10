#!/bin/sh
# SPDX-License-Identifier: AGPL-3.0-only
# Renders yapstack-relay.toml from the container's environment (wired from the one .env),
# then exec's the relay. This is the .env -> config bridge: the server reads a TOML file
# (YAPSTACK_RELAY_CONFIG), and this script writes it from env vars at start.
#
# NOTE: there is deliberately NO [limits] section and NO admin key. Absent [limits] =>
# entitlements = AllowAll and the admin API is not even mounted. Self-host is always the
# maximum tier and the relay makes ZERO outbound calls.
set -eu

: "${DATABASE_URL:?DATABASE_URL must be set}"
: "${JWT_SECRET:?JWT_SECRET must be set}"
: "${SERVER_PEPPER:?SERVER_PEPPER must be set}"

BIND_ADDR="${YAPSTACK_BIND_ADDR:-0.0.0.0:8080}"
CONFIG_PATH="${YAPSTACK_RELAY_CONFIG:-/app/yapstack-relay.toml}"

# TOML string escaping: backslash and double-quote. Secrets are written as basic strings.
toml_escape() {
    printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g'
}

{
    printf 'bind_addr = "%s"\n' "$(toml_escape "$BIND_ADDR")"
    printf 'database_url = "%s"\n' "$(toml_escape "$DATABASE_URL")"
    printf 'jwt_secret = "%s"\n' "$(toml_escape "$JWT_SECRET")"
    printf 'server_pepper = "%s"\n' "$(toml_escape "$SERVER_PEPPER")"

    # Trusted reverse-proxy IPs (comma-separated). X-Forwarded-For is honored ONLY when
    # the immediate wire peer is one of these; unset => fail-closed (XFF ignored, the
    # connecting peer is the rate-limit key). Set to the IP the relay sees Caddy connect
    # FROM so per-client rate limits work behind the proxy (docs/self-hosting.md).
    if [ -n "${YAPSTACK_TRUSTED_PROXIES:-}" ]; then
        printf 'trusted_proxies = ['
        _tp_sep=''
        # POSIX word-split on commas.
        _tp_ifs="$IFS"; IFS=','
        for _tp in $YAPSTACK_TRUSTED_PROXIES; do
            _tp="$(printf '%s' "$_tp" | sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//')"
            [ -z "$_tp" ] && continue
            printf '%s"%s"' "$_tp_sep" "$(toml_escape "$_tp")"
            _tp_sep=', '
        done
        IFS="$_tp_ifs"
        printf ']\n'
    fi

    printf '\n[sync]\n'
    printf 'protocol_version = 1\n'
    printf 'min_client_version = "1.0.0"\n'
    printf 'engine_version = "0.16.3"\n'
    # billing_url intentionally omitted: self-host advertises no control plane.

    # Object storage is present in this stack (audio + R2 snapshot blobs). The relay only
    # PRESIGNS URLs (local HMAC); it never uploads/downloads bytes.
    if [ -n "${MINIO_ENDPOINT:-}" ]; then
        printf '\n[storage]\n'
        printf 'endpoint = "%s"\n' "$(toml_escape "${MINIO_ENDPOINT}")"
        printf 'region = "%s"\n' "$(toml_escape "${MINIO_REGION:-us-east-1}")"
        printf 'bucket = "%s"\n' "$(toml_escape "${MINIO_BUCKET:-yapstack}")"
        printf 'access_key_id = "%s"\n' "$(toml_escape "${MINIO_ROOT_USER:?MINIO_ROOT_USER must be set}")"
        printf 'secret_access_key = "%s"\n' "$(toml_escape "${MINIO_ROOT_PASSWORD:?MINIO_ROOT_PASSWORD must be set}")"
        if [ -n "${MINIO_PUBLIC_ENDPOINT:-}" ]; then
            printf 'public_endpoint = "%s"\n' "$(toml_escape "${MINIO_PUBLIC_ENDPOINT}")"
        fi
    fi
} > "$CONFIG_PATH"

export YAPSTACK_RELAY_CONFIG="$CONFIG_PATH"
echo "yapstack: rendered relay config at $CONFIG_PATH (self-host, AllowAll, zero outbound)"
exec yapstack-server
