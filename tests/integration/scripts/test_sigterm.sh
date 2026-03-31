#!/usr/bin/env bash
set -euo pipefail

# T048: SIGTERM graceful shutdown
# Start traffic through HAProxy (triggering the delay plugin), then send
# SIGTERM to the hub. Verify in-flight request completes and hub exits 0.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
COMPOSE_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
PROJECT="t048-$$"
export HAPROXY_PORT=${HAPROXY_PORT:-$(shuf -i 20000-50000 -n 1)}
# In GitLab DinD, containers run on the 'docker' host, not localhost
DOCKER_HOST_ADDR=$(echo "${DOCKER_HOST:-}" | sed -n 's|tcp://\([^:]*\):.*|\1|p')
DOCKER_HOST_ADDR=${DOCKER_HOST_ADDR:-localhost}

cleanup() {
    docker compose -p "$PROJECT" -f "$COMPOSE_DIR/docker-compose.yml" down -v 2>/dev/null || true
}
trap cleanup EXIT

# Use config with delay plugin (default 5ms sleep per request)
export HUB_CONFIG=config-delay.toml

echo "Starting services with delay plugin..."
docker compose -p "$PROJECT" -f "$COMPOSE_DIR/docker-compose.yml" up -d

echo "Waiting for HAProxy..."
for i in $(seq 1 30); do
    if curl -sf http://$DOCKER_HOST_ADDR:$HAPROXY_PORT/ >/dev/null 2>&1; then break; fi
    if [ "$i" -eq 30 ]; then
        echo "FAIL: HAProxy did not become ready"
        docker compose -p "$PROJECT" -f "$COMPOSE_DIR/docker-compose.yml" logs
        exit 1
    fi
    sleep 1
done

# Send a request in the background
curl -sf http://$DOCKER_HOST_ADDR:$HAPROXY_PORT/ -o /dev/null &
CURL_PID=$!

# Brief pause so the request reaches the hub
sleep 0.1

# Send SIGTERM to the hub
echo "Sending SIGTERM to hub..."
docker compose -p "$PROJECT" -f "$COMPOSE_DIR/docker-compose.yml" kill -s SIGTERM hub

# The background curl should complete (hub drains in-flight work)
if ! wait $CURL_PID; then
    echo "FAIL: in-flight request did not complete after SIGTERM"
    exit 1
fi

# Wait for hub to exit
sleep 2

# Check hub exit code
HUB_CONTAINER=$(docker compose -p "$PROJECT" -f "$COMPOSE_DIR/docker-compose.yml" ps -q hub 2>/dev/null)
if [ -z "$HUB_CONTAINER" ]; then
    echo "FAIL: could not find hub container"
    exit 1
fi

HUB_EXIT=$(docker inspect --format='{{.State.ExitCode}}' "$HUB_CONTAINER" 2>/dev/null || echo "unknown")
if [ "$HUB_EXIT" != "0" ]; then
    echo "FAIL: hub exited with code $HUB_EXIT, expected 0"
    docker compose -p "$PROJECT" -f "$COMPOSE_DIR/docker-compose.yml" logs hub 2>&1 | tail -20
    exit 1
fi

# Verify hub logs show graceful drain
HUB_LOGS=$(docker compose -p "$PROJECT" -f "$COMPOSE_DIR/docker-compose.yml" logs hub 2>&1)
if ! echo "$HUB_LOGS" | grep -q "shutdown signal"; then
    echo "FAIL: hub logs missing shutdown signal acknowledgment"
    exit 1
fi

echo "PASS: T048 - SIGTERM graceful shutdown (hub exit code: $HUB_EXIT)"
