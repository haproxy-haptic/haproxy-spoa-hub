#!/usr/bin/env bash
set -euo pipefail

# T057: End-to-end quickstart validation
# Build hub, start with HAProxy, send traffic via curl, verify
# the hub processes the SPOE message and the noop plugin runs.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
COMPOSE_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
PROJECT="t057-$$"
export HAPROXY_PORT=${HAPROXY_PORT:-$(shuf -i 20000-50000 -n 1)}
# With a remote tcp:// DOCKER_HOST (DinD), published ports live on that host, not localhost
DOCKER_HOST_ADDR=$(echo "${DOCKER_HOST:-}" | sed -n 's|tcp://\([^:]*\):.*|\1|p')
DOCKER_HOST_ADDR=${DOCKER_HOST_ADDR:-localhost}

cleanup() {
    docker compose -p "$PROJECT" -f "$COMPOSE_DIR/docker-compose.yml" down -v 2>/dev/null || true
}
trap cleanup EXIT

echo "Building hub image..."
docker compose -p "$PROJECT" -f "$COMPOSE_DIR/docker-compose.yml" build --quiet hub

echo "Starting services..."
docker compose -p "$PROJECT" -f "$COMPOSE_DIR/docker-compose.yml" up -d

echo "Waiting for HAProxy on port $HAPROXY_PORT..."
for i in $(seq 1 30); do
    if curl -sf --max-time 2 "http://$DOCKER_HOST_ADDR:$HAPROXY_PORT/" >/dev/null 2>&1; then break; fi
    if [ "$i" -eq 30 ]; then
        echo "FAIL: HAProxy did not become ready"
        docker compose -p "$PROJECT" -f "$COMPOSE_DIR/docker-compose.yml" logs
        exit 1
    fi
    sleep 1
done

echo "Sending test request..."
HTTP_CODE=$(curl -sf -o /dev/null -w '%{http_code}' --max-time 5 "http://$DOCKER_HOST_ADDR:$HAPROXY_PORT/" 2>/dev/null || true)
if [ "$HTTP_CODE" != "200" ]; then
    echo "FAIL: expected HTTP 200, got $HTTP_CODE"
    docker compose -p "$PROJECT" -f "$COMPOSE_DIR/docker-compose.yml" logs
    exit 1
fi

# Give the hub a moment to flush logs
sleep 1

# Verify hub logs show the full pipeline: load → handshake → dispatch → complete
HUB_LOGS=$(docker compose -p "$PROJECT" -f "$COMPOSE_DIR/docker-compose.yml" logs hub 2>&1)

# Match in-shell: with `| grep -q`, grep exits on first match, the
# producer takes SIGPIPE, and pipefail reports 141 -- a match read as a miss.
if [[ "$HUB_LOGS" != *"plugin loaded"* ]]; then
    echo "FAIL: hub logs missing 'plugin loaded' event"
    echo "$HUB_LOGS"
    exit 1
fi

# The plugin's own `log::info!` from init must come out of the hub binary.
if [[ "$HUB_LOGS" != *'noop plugin initialised as noop'* ]]; then
    echo "FAIL: hub logs missing the noop plugin's forwarded init record"
    echo "$HUB_LOGS"
    exit 1
fi

if [[ "$HUB_LOGS" != *"SPOP handshake completed"* ]]; then
    echo "FAIL: hub logs missing 'SPOP handshake completed'"
    echo "$HUB_LOGS"
    exit 1
fi

if [[ "$HUB_LOGS" != *"plugin completed"* ]]; then
    echo "FAIL: hub logs missing 'plugin completed' — noop plugin did not run"
    echo "$HUB_LOGS"
    exit 1
fi

if [[ "$HUB_LOGS" != *"message processed"* ]]; then
    echo "FAIL: hub logs missing 'message processed'"
    echo "$HUB_LOGS"
    exit 1
fi

echo "PASS: T057 - End-to-end quickstart validation"
