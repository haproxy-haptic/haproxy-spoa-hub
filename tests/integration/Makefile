.PHONY: build test test-quickstart test-sigterm test-sighup test-load clean

build:
	docker compose build

test: test-quickstart test-sigterm test-sighup
	@echo "All integration tests passed."

test-quickstart:
	@echo "=== T057: End-to-end quickstart ==="
	@bash scripts/test_quickstart.sh

test-sigterm:
	@echo "=== T048: SIGTERM graceful shutdown ==="
	@bash scripts/test_sigterm.sh

test-sighup:
	@echo "=== T049: SIGHUP config reload ==="
	@bash scripts/test_sighup.sh

test-load:
	@echo "=== T054b: Load test (SC-004) ==="
	@bash scripts/test_load.sh

clean:
	docker compose down -v --remove-orphans 2>/dev/null || true
	docker image prune -f 2>/dev/null || true
