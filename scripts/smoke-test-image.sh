#!/bin/sh
# Smoke-test a hub image on the host's native architecture: --help runs, the
# hub loads the noop plugin and binds a TCP listener.
#
# Usage: scripts/smoke-test-image.sh <image-ref>

set -eu

IMAGE="${1:?usage: $0 <image-ref>}"

docker run --rm "${IMAGE}" --help

docker build -t smoke-hub - <<DOCKERFILE
FROM ${IMAGE}
RUN printf '%s\n' \
  'plugin_dir = "/etc/haproxy-spoa-hub/plugins"' \
  'default_timeout_ms = 500' \
  'log_level = "debug"' \
  '[[listeners]]' \
  'type = "tcp"' \
  'address = "0.0.0.0:12345"' \
  '[[plugins]]' \
  'name = "noop"' \
  'library = "libnoop_plugin.so"' \
  'messages = ["check-request"]' \
  > /etc/haproxy-spoa-hub/config.toml
DOCKERFILE

docker rm -f smoke-hub >/dev/null 2>&1 || true
docker run -d --name smoke-hub smoke-hub
trap 'docker rm -f smoke-hub >/dev/null 2>&1 || true' EXIT

i=1
while :; do
    if [ "$(docker inspect -f '{{.State.Running}}' smoke-hub 2>/dev/null)" != "true" ]; then
        echo "FAIL: container exited unexpectedly"
        docker logs smoke-hub
        exit 1
    fi
    if docker logs smoke-hub 2>&1 | grep -q "listening"; then
        echo "Hub is listening"
        break
    fi
    if [ "$i" -eq 15 ]; then
        echo "FAIL: hub did not start listening within 15s"
        docker logs smoke-hub
        exit 1
    fi
    i=$((i + 1))
    sleep 1
done

docker logs smoke-hub 2>&1 | grep "plugin loaded"
echo "PASS - smoke test"
