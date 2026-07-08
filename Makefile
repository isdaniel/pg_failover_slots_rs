# Makefile — pg_failover_slots_rs (pgrx extension)
#
# Override the target PostgreSQL major version, e.g.:  make test PG=pg17
# pgrx requires exactly ONE pg feature active, so we never use --all-features.
PG ?= pg18
PG_CONFIG ?= /usr/bin/pg_config

.PHONY: check build format format-check clippy audit test doc-check \
        package integration-test before-git-push \
        deps-check deps-bump deps-bump-dry

deps-check:
	@echo "=== Checking for outdated dependencies ==="
	@cargo update --dry-run 2>&1 | grep -i "updating\|unchanged\|locking" || echo "All dependencies up to date"

deps-bump:
	@echo "=== Bumping dependencies to latest compatible versions ==="
	cargo update
	@echo "=== Verifying build ==="
	cargo check --no-default-features --features $(PG)
	@echo "=== Running tests ==="
	cargo pgrx test $(PG)
	@echo "Done."

deps-bump-dry:
	@echo "=== Dry-run: what would be updated ==="
	@cargo update --dry-run 2>&1

check:
	cargo check --no-default-features --features $(PG)

build:
	cargo build --no-default-features --features $(PG)

format:
	cargo fmt

format-check:
	cargo fmt --check

clippy:
	cargo clippy --no-default-features --features $(PG) -- -D warnings

audit:
	cargo audit

test:
	cargo pgrx test $(PG)

doc-check:
	cargo doc --no-deps --no-default-features --features $(PG)

package:
	cargo pgrx package --no-default-features --features $(PG) --pg-config $(PG_CONFIG)

# Heavy — builds Docker images and runs the HA failover + physical-before-logical
# scenarios (needs Docker). Kept out of before-git-push because it is slow.
integration-test:
	cd tests/integration && ./run_physical_before_logical_test.sh
	cd tests/integration && ./run_ha_failover_test.sh

before-git-push: format-check check clippy test doc-check
