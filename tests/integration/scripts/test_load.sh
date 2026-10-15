#!/usr/bin/env bash
set -euo pipefail

# T054b: Load test — verify concurrent SPOP connections with stable p99 latency (SC-004)
#
# Uses 'hey' (Go HTTP load tester) to drive concurrent requests through HAProxy,
# each of which creates a SPOP connection to the hub. Measures p99 latency and
# verifies it stays below a threshold.
#
# Environment variables:
#   HAPROXY_PORT       — random by default
#   LOAD_CONCURRENCY   — concurrent connections (default: 200)
#   LOAD_REQUESTS      — total requests to send (default: 10000)
#   P99_THRESHOLD_MS   — max acceptable p99 latency in ms (default: 50)

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
COMPOSE_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
PROJECT="t054b-$$"
export HAPROXY_PORT=${HAPROXY_PORT:-$(shuf -i 20000-50000 -n 1)}
# With a remote tcp:// DOCKER_HOST (DinD), published ports live on that host, not localhost
DOCKER_HOST_ADDR=$(echo "${DOCKER_HOST:-}" | sed -n 's|tcp://\([^:]*\):.*|\1|p')
DOCKER_HOST_ADDR=${DOCKER_HOST_ADDR:-localhost}

CONCURRENCY=${LOAD_CONCURRENCY:-200}
REQUESTS=${LOAD_REQUESTS:-10000}
P99_THRESHOLD=${P99_THRESHOLD_MS:-50}

cleanup() {
    docker compose -p "$PROJECT" -f "$COMPOSE_DIR/docker-compose.yml" down -v 2>/dev/null || true
}
trap cleanup EXIT

# Check for 'hey' load tester
if ! command -v hey &>/dev/null; then
    echo "SKIP: T054b - 'hey' not installed (go install github.com/rakyll/hey@latest)"
    exit 0
fi

echo "Starting services..."
docker compose -p "$PROJECT" -f "$COMPOSE_DIR/docker-compose.yml" up -d

echo "Waiting for HAProxy on port $HAPROXY_PORT..."
for i in $(seq 1 30); do
    if curl -sf "http://$DOCKER_HOST_ADDR:$HAPROXY_PORT/" >/dev/null 2>&1; then break; fi
    if [ "$i" -eq 30 ]; then
        echo "FAIL: HAProxy did not become ready"
        docker compose -p "$PROJECT" -f "$COMPOSE_DIR/docker-compose.yml" logs
        exit 1
    fi
    sleep 1
done

# Warmup: a few requests to ensure plugin is loaded and JIT is warm
echo "Warming up (50 requests)..."
hey -n 50 -c 10 -q 0 "http://$DOCKER_HOST_ADDR:$HAPROXY_PORT/" >/dev/null 2>&1

# Run the load test
echo "Running load test: $REQUESTS requests, $CONCURRENCY concurrent..."
RESULT=$(hey -n "$REQUESTS" -c "$CONCURRENCY" -q 0 "http://$DOCKER_HOST_ADDR:$HAPROXY_PORT/" 2>&1)

echo "$RESULT"
echo "---"

# Extract p99 latency from hey output (in seconds, convert to ms)
# hey outputs a latency distribution table like:
#   99% in 0.0123 secs
P99_SECS=$(echo "$RESULT" | grep -E '^\s*99%' | awk '{print $3}')
if [ -z "$P99_SECS" ]; then
    echo "FAIL: could not parse p99 latency from hey output"
    exit 1
fi

# Convert seconds to milliseconds (using awk for float arithmetic)
P99_MS=$(awk "BEGIN { printf \"%.2f\", $P99_SECS * 1000 }")

# Extract error count
ERRORS=$(echo "$RESULT" | grep -c '^\[' || true)
STATUS_ERRORS=$(echo "$RESULT" | grep "Status code distribution" -A 50 | grep -v "200" | grep -oP '\d+' | head -1 || echo "0")

# Check for non-200 responses
NON_200=$(echo "$RESULT" | grep -A 20 "Status code distribution" | grep -v "200" | grep -oP '\[\d+\]\s+\d+' || true)
if [ -n "$NON_200" ]; then
    echo "WARNING: Non-200 responses detected: $NON_200"
fi

# Verify p99 latency is below threshold
PASS=$(awk "BEGIN { print ($P99_MS <= $P99_THRESHOLD) ? 1 : 0 }")
if [ "$PASS" -eq 1 ]; then
    echo "PASS: T054b - Load test (p99: ${P99_MS}ms <= ${P99_THRESHOLD}ms threshold, ${CONCURRENCY} concurrent, ${REQUESTS} requests)"
else
    echo "FAIL: T054b - p99 latency ${P99_MS}ms exceeds ${P99_THRESHOLD}ms threshold"
    echo "Hub logs (last 20 lines):"
    docker compose -p "$PROJECT" -f "$COMPOSE_DIR/docker-compose.yml" logs hub 2>&1 | tail -20
    exit 1
fi
