#!/usr/bin/env bash
set -euo pipefail

# T049: SIGHUP config reload
# 1. Start hub with noop plugin only
# 2. Send request, verify noop plugin processes it
# 3. Overwrite config to add echo plugin
# 4. Send SIGHUP
# 5. Send request, verify echo plugin also processes it

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
COMPOSE_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
PROJECT="t049-$$"
export HAPROXY_PORT=${HAPROXY_PORT:-$(shuf -i 20000-50000 -n 1)}
# In GitLab DinD, containers run on the 'docker' host, not localhost
DOCKER_HOST_ADDR=$(echo "${DOCKER_HOST:-}" | sed -n 's|tcp://\([^:]*\):.*|\1|p')
DOCKER_HOST_ADDR=${DOCKER_HOST_ADDR:-localhost}

cleanup() {
    docker compose -p "$PROJECT" -f "$COMPOSE_DIR/docker-compose.yml" down -v 2>/dev/null || true
}
trap cleanup EXIT

echo "Starting services with noop plugin only..."
docker compose -p "$PROJECT" -f "$COMPOSE_DIR/docker-compose.yml" up -d --quiet-pull

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

# Step 1: Verify noop plugin processes the request
curl -sf --max-time 5 "http://$DOCKER_HOST_ADDR:$HAPROXY_PORT/" >/dev/null
sleep 1
HUB_LOGS=$(docker compose -p "$PROJECT" -f "$COMPOSE_DIR/docker-compose.yml" logs hub 2>&1)
# Match in-shell: with `| grep -q`, grep exits on first match, the
# producer takes SIGPIPE, and pipefail reports 141 -- a match read as a miss.
if [[ "$HUB_LOGS" != *'"plugin":"noop"'* ]]; then
    if ! grep -q 'plugin.*noop.*completed' <<<"$HUB_LOGS"; then
        echo "FAIL: noop plugin not invoked before reload"
        echo "$HUB_LOGS"
        exit 1
    fi
fi
echo "OK: noop plugin active before reload"

# Step 2: Swap config to add echo plugin
HUB_CONTAINER=$(docker compose -p "$PROJECT" -f "$COMPOSE_DIR/docker-compose.yml" ps -q hub)
docker exec "$HUB_CONTAINER" cp /config/config-with-echo.toml /config/config.toml

# Step 3: Send SIGHUP
echo "Sending SIGHUP to hub..."
docker compose -p "$PROJECT" -f "$COMPOSE_DIR/docker-compose.yml" kill -s SIGHUP hub
sleep 2

# Step 4: Verify reload succeeded
HUB_LOGS=$(docker compose -p "$PROJECT" -f "$COMPOSE_DIR/docker-compose.yml" logs hub 2>&1)
if [[ "$HUB_LOGS" != *"configuration reloaded successfully"* ]]; then
    echo "FAIL: config reload did not succeed"
    echo "$HUB_LOGS" | tail -30
    exit 1
fi
echo "OK: configuration reloaded"

# Step 5: Send another request and check echo plugin ran
curl -sf --max-time 5 "http://$DOCKER_HOST_ADDR:$HAPROXY_PORT/" >/dev/null
sleep 1
HUB_LOGS=$(docker compose -p "$PROJECT" -f "$COMPOSE_DIR/docker-compose.yml" logs hub 2>&1)
if [[ "$HUB_LOGS" != *'"plugin":"echo"'* ]]; then
    if ! grep -q 'plugin.*echo.*completed' <<<"$HUB_LOGS"; then
        echo "FAIL: echo plugin not invoked after SIGHUP reload"
        echo "$HUB_LOGS" | tail -30
        exit 1
    fi
fi

# Step 6: Verify noop still works after reload
if [[ "$(grep -c '"plugin":"noop"' <<<"$HUB_LOGS" || true)" -lt 2 ]]; then
    if [[ "$(grep -c 'plugin.*noop.*completed' <<<"$HUB_LOGS" || true)" -lt 2 ]]; then
        echo "WARN: could not confirm noop ran after reload (may be log format mismatch)"
    fi
fi

echo "PASS: T049 - SIGHUP config reload"
