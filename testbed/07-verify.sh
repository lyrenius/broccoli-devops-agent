#!/usr/bin/env bash
# End-to-end check of the control plane against the testbed.
#
# Each case injects a fault whose ground truth is known, captures a Snapshot, and asserts
# what the agent should have concluded. Two of the cases assert a *negative* -- that the
# fault does not appear -- because the v0.1 probe registry genuinely cannot see them, and
# quietly passing those would hide the coverage-gap design rather than test it.
#
# Runs the deterministic Team by default. Export BROCCOLI_MODEL_API_KEY and pass
# --team harness to exercise the model-backed path and the ActionRun proposals with it.

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

AGENT=(cargo run --quiet -- --config config/agent.testbed.toml)
TEAM="readonly"
[ "${1:-}" = "--team" ] && TEAM="${2:-readonly}"

cd "$REPO_DIR"

PASS=0
FAIL=0

# Assert that a snapshot line for `resource` reports `expected` health.
expect_state() {
    local snap="$1" resource="$2" expected="$3"
    local actual
    # Stop at the coverage-gaps section: a resource named there would otherwise match a
    # second time and the health word would be compared against the gap text.
    actual="$(echo "$snap" | awk -v r="$resource" '/^coverage gaps:/ {exit} $1 == r {print $3}')"
    if [ "$actual" = "$expected" ]; then
        printf '    \033[1;32mPASS\033[0m %-20s %s\n' "$resource" "$expected"
        PASS=$((PASS + 1))
    else
        printf '    \033[1;31mFAIL\033[0m %-20s expected %s, got %s\n' "$resource" "$expected" "${actual:-<missing>}"
        FAIL=$((FAIL + 1))
    fi
}

snapshot() { "${AGENT[@]}" snapshot 2>/dev/null; }

banner() { printf '\n\033[1;36m== %s\033[0m\n' "$*"; }

banner "case 0: baseline, everything deployed and healthy"
"$TESTBED_DIR/scenarios/restore-all.sh" >/dev/null 2>&1
snap="$(snapshot)"
expect_state "$snap" postgres-main Healthy
expect_state "$snap" redis-mq Healthy
expect_state "$snap" seaweedfs-storage Healthy
expect_state "$snap" broccoli-server Healthy
expect_state "$snap" web-frontend Healthy
expect_state "$snap" worker-1 Unknown

banner "case 1: redis stopped -- a dependency the probes can see"
"$TESTBED_DIR/scenarios/stop-service.sh" redis-mq >/dev/null
snap="$(snapshot)"
expect_state "$snap" redis-mq Down
expect_state "$snap" postgres-main Healthy
# /healthz pings the DB and the queue and answers 503 when either is down, so the server
# reports its own dependency failure. The frontend probe fetches the SPA entry point,
# which is served from disk and keeps answering 200 -- the split is real, not an artefact.
expect_state "$snap" broccoli-server Down
expect_state "$snap" web-frontend Healthy
echo "  diagnosis:"
"${AGENT[@]}" report --team "$TEAM" \
    --title "Submissions are not being judged" \
    --description "The judge queue stopped draining a few minutes ago" 2>&1 | sed -n 's/^summary: /    /p'
"$TESTBED_DIR/scenarios/restore-all.sh" >/dev/null 2>&1

banner "case 2: worker stopped -- a fault no probe can see"
"$TESTBED_DIR/scenarios/stop-service.sh" worker-1 >/dev/null 2>&1
snap="$(snapshot)"
# The whole point: the deployment is broken and the Snapshot still looks fine.
expect_state "$snap" broccoli-server Healthy
expect_state "$snap" redis-mq Healthy
expect_state "$snap" worker-1 Unknown
echo "  diagnosis (should lean on the coverage gap, not claim health):"
"${AGENT[@]}" report --team "$TEAM" \
    --title "Submissions stay queued forever" \
    --description "Contestants submit and the verdict never arrives" 2>&1 | sed -n -e 's/^summary: /    /p' -e 's/^open: */    open: /p'
"$TESTBED_DIR/scenarios/restore-all.sh" >/dev/null 2>&1

banner "case 3: app-1 partitioned from infra-1 -- healthy parts, broken whole"
"$TESTBED_DIR/scenarios/partition.sh" "$APP_VM" "$INFRA_VM" >/dev/null 2>&1
sleep 20
snap="$(snapshot)"
expect_state "$snap" postgres-main Healthy
expect_state "$snap" redis-mq Healthy
expect_state "$snap" broccoli-server Down
# Nothing on app-1 crashed: it still serves the frontend, it just cannot reach infra-1.
expect_state "$snap" web-frontend Healthy
echo "  diagnosis:"
"${AGENT[@]}" report --team "$TEAM" \
    --title "The site is down for contestants" \
    --description "The web console returns errors, but the database looks fine" 2>&1 | sed -n 's/^summary: /    /p'
"$TESTBED_DIR/scenarios/restore-all.sh" >/dev/null 2>&1

banner "results"
printf '  %d passed, %d failed\n\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
