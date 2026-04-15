#!/usr/bin/env bash
set -euo pipefail

PROVIDER_ROOT="/Users/jmatthews/synced/frontend-analyzer-provider"
PROJECT_ROOT="/tmp/semver-pipeline-v2/repos/quipucords-ui"
RULES_ROOT="/tmp/semver-pipeline-v2/rules"
STRATEGIES_JSON="/tmp/semver-pipeline-v2/fix-guidance/fix-strategies.json"

PORT="${PORT:-9001}"
ARTIFACT_DIR="${ARTIFACT_DIR:-/tmp/semver-pipeline-v2/plan-artifacts/quipucords-ui}"
PROVIDER_BIN="$PROVIDER_ROOT/target/release/frontend-analyzer-provider"

mkdir -p "$ARTIFACT_DIR"

cargo build --release --manifest-path "$PROVIDER_ROOT/Cargo.toml"

cat > "$ARTIFACT_DIR/provider_settings.json" <<JSON
[
  {
    "name": "frontend",
    "address": "localhost:${PORT}",
    "initConfig": [
      {
        "analysisMode": "source-only",
        "location": "${PROJECT_ROOT}"
      }
    ]
  },
  {
    "name": "builtin",
    "initConfig": [
      {
        "location": "${PROJECT_ROOT}"
      }
    ]
  }
]
JSON

"$PROVIDER_BIN" serve --port "$PORT" >"$ARTIFACT_DIR/provider.log" 2>&1 &
PROVIDER_PID=$!

cleanup() {
  kill "$PROVIDER_PID" >/dev/null 2>&1 || true
}
trap cleanup EXIT

sleep 2

kantra analyze \
  --input "$PROJECT_ROOT" \
  --output "$ARTIFACT_DIR/kantra-output" \
  --rules "$RULES_ROOT" \
  --override-provider-settings "$ARTIFACT_DIR/provider_settings.json" \
  --enable-default-rulesets=false \
  --skip-static-report \
  --no-dependency-rules \
  --mode source-only \
  --run-local \
  --provider java

(
  cd "$ARTIFACT_DIR"
  "$PROVIDER_BIN" plan \
    "$PROJECT_ROOT" \
    --input "$ARTIFACT_DIR/kantra-output/output.yaml" \
    --strategies "$STRATEGIES_JSON" \
    --verbose
)

echo "Plan written to: $ARTIFACT_DIR/remediation-plan.json"
echo "Provider log:    $ARTIFACT_DIR/provider.log"
echo "Kantra output:   $ARTIFACT_DIR/kantra-output/output.yaml"
