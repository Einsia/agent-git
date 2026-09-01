#!/usr/bin/env bash
set -euo pipefail

: "${CI_API_V4_URL:?missing CI_API_V4_URL}"
: "${CI_PROJECT_ID:?missing CI_PROJECT_ID}"
: "${CI_COMMIT_SHA:?missing CI_COMMIT_SHA}"
: "${CI_COMMIT_REF_NAME:?missing CI_COMMIT_REF_NAME}"
: "${CI_PIPELINE_ID:?missing CI_PIPELINE_ID}"
: "${AGIT_NOTIFY_CHANNEL:?missing AGIT_NOTIFY_CHANNEL}"
: "${AGIT_NOTIFY_JOBS:?missing AGIT_NOTIFY_JOBS}"
: "${AGIT_NOTIFIER_TRIGGER_TOKEN:?missing AGIT_NOTIFIER_TRIGGER_TOKEN}"

case "$AGIT_NOTIFY_CHANNEL:$AGIT_NOTIFY_JOBS" in
  dev:dev:linux,dev:macos-arm64 | \
  staging:staging:linux,staging:macos-arm64) ;;
  *) echo "unsupported AgentGit notification target" >&2; exit 2 ;;
esac

curl --fail-with-body --silent --show-error \
  --request POST \
  --form "token=$AGIT_NOTIFIER_TRIGGER_TOKEN" \
  --form 'ref=main' \
  --form "variables[AGIT_SOURCE_PROJECT_ID]=$CI_PROJECT_ID" \
  --form "variables[AGIT_SOURCE_SHA]=$CI_COMMIT_SHA" \
  --form "variables[AGIT_SOURCE_REF]=$CI_COMMIT_REF_NAME" \
  --form "variables[AGIT_SOURCE_CHANNEL]=$AGIT_NOTIFY_CHANNEL" \
  --form "variables[AGIT_SOURCE_PIPELINE_ID]=$CI_PIPELINE_ID" \
  --form "variables[AGIT_ARTIFACT_JOBS]=$AGIT_NOTIFY_JOBS" \
  "$CI_API_V4_URL/projects/88/trigger/pipeline" >/dev/null

printf 'queued trusted %s build notification\n' "$AGIT_NOTIFY_CHANNEL"
