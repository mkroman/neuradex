#!/usr/bin/env bash
#
# Offline checks for the generated B2 content lifecycle policy.
#
# These assertions run in CI and locally. They do not call AWS: the generator's
# default print mode only needs jq.
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly SCRIPT_DIR
readonly GENERATOR="${SCRIPT_DIR}/content-lifecycle.sh"
WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/neuradex-lifecycle-test.XXXXXX")"
readonly WORK_DIR

cleanup() {
  rm -rf -- "${WORK_DIR}"
}
trap cleanup EXIT

require_command() {
  if ! command -v "$1" >/dev/null 2>&1; then
    printf 'error: required command not found: %s\n' "$1" >&2
    exit 2
  fi
}

require_command jq

assert_policy() {
  local retention="$1"
  local prefix="$2"
  local policy="${WORK_DIR}/policy-${retention}-${prefix:-root}.json"

  CONTENT_RETENTION_DAYS="${retention}" \
    CONTENT_S3_PREFIX="${prefix}" \
    "${GENERATOR}" --output "${policy}" >/dev/null

  jq -e --argjson retention "${retention}" --arg prefix "${prefix}" '
    . as $document
    | $document.Rules as $rules
    | (if $prefix == ""
       then ["packs/", "objects/", "index/", "events/"]
       else ["packs/", "objects/", "index/", "events/"] | map("\($prefix)/\(.)")
       end) as $expected
    | ($rules | length) == 12
    and ($rules | map(.ID) | length) == ($rules | map(.ID) | unique | length)
    and ([$rules[].Status] | all(. == "Enabled"))
    and ([$rules[].Filter.Prefix] | unique | sort) == ($expected | sort)
    and all($expected[] as $p |
      ([$rules[] | select(.Filter.Prefix == $p)] | length) == 3
      and ([$rules[] | select(.Filter.Prefix == $p and .Expiration.Days == $retention)] | length) == 1
      and ([$rules[] | select(.Filter.Prefix == $p and .Expiration.ExpiredObjectDeleteMarker == true)] | length) == 1
      and ([$rules[] | select(.Filter.Prefix == $p and .NoncurrentVersionExpiration.NoncurrentDays == 1)] | length) == 1
    )
  ' "${policy}" >/dev/null

  printf 'ok lifecycle policy: retention=%s prefix=%s\n' "${retention}" "${prefix:-<root>}"
}

assert_policy 365 ""
assert_policy 90 tenant

# Invalid retention values are configuration errors, not policy output.
if CONTENT_RETENTION_DAYS=0 "${GENERATOR}" --print >/dev/null 2>&1; then
  printf 'error: CONTENT_RETENTION_DAYS=0 should fail\n' >&2
  exit 1
fi

printf 'ok lifecycle policy checks\n'
