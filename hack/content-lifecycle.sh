#!/usr/bin/env bash
#
# Generate or apply the neuradex content lifecycle policy on a B2 bucket.
#
# Backblaze B2 maps S3 lifecycle actions to its version model:
#
#   Expiration.Days                              -> hide the current version
#   Expiration.ExpiredObjectDeleteMarker = true   -> remove orphan delete markers
#   NoncurrentVersionExpiration.NoncurrentDays   -> delete the hidden version
#
# Hiding alone does not free storage. Each prefix therefore gets three rules:
# hide the current version after CONTENT_RETENTION_DAYS, remove orphan delete
# markers (the pair B2 requires), and delete the hidden version one day later.
# B2 runs lifecycle rules once per day, so the effective deletion window is
# roughly CONTENT_RETENTION_DAYS + 1 days after upload, not an exact SLA.
#
# The S3 API is the single lifecycle writer for the bucket. Do not create,
# edit, or delete these rules in the B2 web console or through the B2 Native
# API (b2_update_bucket): B2 regenerates rule IDs when either is used, and the
# two views of the configuration diverge. Applying this policy replaces the
# bucket's entire lifecycle configuration.
#
# The bucket must have versioning enabled. --apply and --check verify that
# before doing anything else.
#
# AbortIncompleteMultipartUpload is intentionally omitted: the service uploads
# packs and objects with single PUTs, so there are no multipart uploads to
# abort. Add it to the generated policy if that ever changes.
#
# Usage:
#   hack/content-lifecycle.sh [--print] [--output FILE]
#   hack/content-lifecycle.sh --check
#   hack/content-lifecycle.sh --apply
#
# --print is the default and needs only jq; it validates the retention days but
# not the S3 connection settings. --check and --apply also need the AWS CLI and
# the CONTENT_S3_* settings below. Secrets are never printed.
#
# Exit status: 0 success or in sync, 1 drift or operation failure, 2 usage or
# configuration error.
set -euo pipefail

readonly DEFAULT_RETENTION_DAYS=365
# B2 applies rules once per day and requires a positive NoncurrentDays value.
# One day is the shortest delay that keeps the pair simple and reviewable.
readonly NONCURRENT_DELETE_DELAY_DAYS=1
readonly PREFIX_NAMES=("packs" "objects" "index" "events")

MODE=""
OUTPUT=""
WORK_DIR=""

usage() {
  cat <<'EOF'
Generate or apply the neuradex B2/S3 content lifecycle policy.

Usage:
  hack/content-lifecycle.sh [--print] [--output FILE]
  hack/content-lifecycle.sh --check
  hack/content-lifecycle.sh --apply

Modes:
  --print         Print the generated policy as JSON (default). No AWS calls.
  --output FILE   Write the policy to FILE instead of stdout (print mode).
  --check         Read the bucket lifecycle configuration and compare it with
                  the generated policy. Exits 1 on drift.
  --apply         Replace the bucket lifecycle configuration. Requires this
                  explicit opt-in; the default mode never writes to the bucket.

Environment (the service uses the same names):
  CONTENT_RETENTION_DAYS        Days before the current version is hidden
                                (default: 365; positive integer).
  CONTENT_S3_BUCKET             Bucket name (--check/--apply).
  CONTENT_S3_ENDPOINT           S3 endpoint, for example
                                https://s3.us-west-004.backblazeb2.com.
  CONTENT_S3_REGION             Signing region, for example us-west-004.
  CONTENT_S3_ACCESS_KEY_ID      Access key ID (never printed).
  CONTENT_S3_SECRET_ACCESS_KEY  Secret access key (never printed).
  CONTENT_S3_PREFIX             Optional key prefix, matching the service.
  CONTENT_S3_FORCE_PATH_STYLE   true (default) or false.

The policy covers packs/, objects/, index/, and events/. Each prefix gets a
hide rule, the required ExpiredObjectDeleteMarker pair, and a noncurrent
version deletion one day after hiding. The S3 API is the only supported
lifecycle writer for the bucket; do not edit the rules in the B2 console or
through b2_update_bucket.

Requirements: jq for --print; jq and AWS CLI v2 for --check and --apply. The
bucket must have versioning enabled.

Exit status: 0 success or in sync, 1 drift or operation failure, 2 usage or
configuration error.
EOF
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 2
}

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

cleanup() {
  if [[ -n "${WORK_DIR}" ]]; then
    rm -rf -- "${WORK_DIR}"
  fi
}

trap cleanup EXIT

trim() {
  local value="$1"
  value="${value#"${value%%[![:space:]]*}"}"
  value="${value%"${value##*[![:space:]]}"}"
  printf '%s' "${value}"
}

normalize_prefix() {
  local value
  value="$(trim "$1")"
  while [[ "${value}" == /* ]]; do
    value="${value#/}"
  done
  while [[ "${value}" == */ ]]; do
    value="${value%/}"
  done
  printf '%s' "${value}"
}

require_command() {
  local name="$1"
  if ! command -v "${name}" >/dev/null 2>&1; then
    case "${name}" in
      aws)
        die "required command not found: aws; install the AWS CLI v2 (https://docs.aws.amazon.com/cli/latest/userguide/getting-started-install.html) for --check/--apply"
        ;;
      jq)
        die "required command not found: jq; install jq to generate the lifecycle policy"
        ;;
      *)
        die "required command not found: ${name}"
        ;;
    esac
  fi
}

validate_positive_integer() {
  local name="$1"
  local value="$2"
  local default="$3"
  if [[ ! "${value}" =~ ^[1-9][0-9]*$ ]]; then
    die "${name} must be a positive integer (got '${value}'; default: ${default})"
  fi
}

require_setting() {
  local name="$1"
  local value
  value="$(trim "${!name-}")"
  if [[ -z "${value}" ]]; then
    die "${name} must be set and non-empty for --${MODE}"
  fi
  printf '%s' "${value}"
}

parse_mode() {
  local requested="$1"
  if [[ -n "${MODE}" && "${MODE}" != "${requested}" ]]; then
    die "choose exactly one of --print, --check, or --apply"
  fi
  MODE="${requested}"
}

parse_args() {
  while (($# > 0)); do
    case "$1" in
      --print)
        parse_mode print
        ;;
      --check)
        parse_mode check
        ;;
      --apply)
        parse_mode apply
        ;;
      -o | --output)
        if (($# < 2)); then
          die "--output requires a file path"
        fi
        OUTPUT="$2"
        shift
        ;;
      -h | --help)
        usage
        exit 0
        ;;
      *)
        die "unknown argument: $1 (try --help)"
        ;;
    esac
    shift
  done

  if [[ -z "${MODE}" ]]; then
    MODE=print
  fi

  if [[ -n "${OUTPUT}" && "${MODE}" != "print" ]]; then
    die "--output is only valid with --print"
  fi
  if [[ "${OUTPUT}" == "-" ]]; then
    OUTPUT=""
  fi
}

generate_policy() {
  jq -n \
    --argjson retention_days "${RETENTION_DAYS}" \
    --argjson noncurrent_days "${NONCURRENT_DELETE_DELAY_DAYS}" \
    --arg content_prefix "${CONTENT_PREFIX}" \
    --args '
      {
        Rules: [
          $ARGS.positional[] as $name
          | (if $content_prefix == "" then "\($name)/" else "\($content_prefix)/\($name)/" end) as $prefix
          | {
              ID: "neuradex-\($name)-current",
              Filter: {Prefix: $prefix},
              Status: "Enabled",
              Expiration: {Days: $retention_days}
            },
            {
              ID: "neuradex-\($name)-delete-markers",
              Filter: {Prefix: $prefix},
              Status: "Enabled",
              Expiration: {ExpiredObjectDeleteMarker: true}
            },
            {
              ID: "neuradex-\($name)-noncurrent",
              Filter: {Prefix: $prefix},
              Status: "Enabled",
              NoncurrentVersionExpiration: {NoncurrentDays: $noncurrent_days}
            }
        ]
      }
    ' \
    "${PREFIX_NAMES[@]}"
}

canonical_rules() {
  jq -S '
    (.Rules // [])
    | map(del(.ID))
    # B2 may serialize an unset boolean as false and omit empty fields. Drop
    # both so an unchanged configuration compares equal.
    | walk(
        if type == "object" then
          with_entries(select(.value != null and .value != false))
        else .
        end
      )
    | sort_by([
        (.Filter.Prefix // .Prefix // ""),
        (.Expiration.Days // 0),
        (.Expiration.ExpiredObjectDeleteMarker // false),
        (.NoncurrentVersionExpiration.NoncurrentDays // 0),
        (.AbortIncompleteMultipartUpload.DaysAfterInitiation // 0)
      ])
  '
}

load_settings() {
  RETENTION_DAYS="$(trim "${CONTENT_RETENTION_DAYS-${DEFAULT_RETENTION_DAYS}}")"
  validate_positive_integer CONTENT_RETENTION_DAYS "${RETENTION_DAYS}" "${DEFAULT_RETENTION_DAYS}"
  CONTENT_PREFIX="$(normalize_prefix "${CONTENT_S3_PREFIX-}")"

  if [[ "${MODE}" == "print" ]]; then
    require_command jq
    return
  fi

  require_command jq
  require_command aws

  BUCKET="$(require_setting CONTENT_S3_BUCKET)"
  ENDPOINT="$(require_setting CONTENT_S3_ENDPOINT)"
  REGION="$(require_setting CONTENT_S3_REGION)"
  ACCESS_KEY_ID="$(require_setting CONTENT_S3_ACCESS_KEY_ID)"
  SECRET_ACCESS_KEY="$(require_setting CONTENT_S3_SECRET_ACCESS_KEY)"

  if [[ ! "${ENDPOINT}" =~ ^https?:// ]]; then
    die "CONTENT_S3_ENDPOINT must start with http:// or https:// (got '${ENDPOINT}')"
  fi

  local force_path_style_raw
  force_path_style_raw="$(trim "${CONTENT_S3_FORCE_PATH_STYLE-}")"
  case "${force_path_style_raw,,}" in
    "" | 1 | true | yes | on)
      FORCE_PATH_STYLE=true
      ;;
    0 | false | no | off)
      FORCE_PATH_STYLE=false
      ;;
    *)
      die "CONTENT_S3_FORCE_PATH_STYLE must be a boolean (true/false); got '${force_path_style_raw}'"
      ;;
  esac
}

prepare_aws_environment() {
  WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/neuradex-lifecycle.XXXXXX")"

  export AWS_ACCESS_KEY_ID="${ACCESS_KEY_ID}"
  export AWS_SECRET_ACCESS_KEY="${SECRET_ACCESS_KEY}"
  export AWS_EC2_METADATA_DISABLED=true
  unset AWS_SESSION_TOKEN

  if [[ "${FORCE_PATH_STYLE}" == "true" ]]; then
    # The AWS CLI has no documented environment variable for addressing style.
    # A private config file is the supported way to force path-style requests.
    unset AWS_PROFILE AWS_DEFAULT_PROFILE
    export AWS_CONFIG_FILE="${WORK_DIR}/aws-config"
    {
      printf '[default]\n'
      printf 's3 =\n'
      printf '    addressing_style = path\n'
    } >"${AWS_CONFIG_FILE}"
  fi
}

aws_s3api() {
  aws --endpoint-url "${ENDPOINT}" --region "${REGION}" s3api "$@"
}

check_bucket_versioning() {
  local output status
  if ! output="$(aws_s3api get-bucket-versioning --bucket "${BUCKET}" --output json 2>"${WORK_DIR}/versioning.err")"; then
    printf 'error: could not read the versioning status of bucket %s:\n' "${BUCKET}" >&2
    cat "${WORK_DIR}/versioning.err" >&2
    exit 1
  fi

  status="$(jq -r '.Status // "disabled"' <<<"${output}")"
  status="${status:-disabled}"
  if [[ "${status}" != "Enabled" ]]; then
    fail "bucket ${BUCKET} does not have versioning enabled (status: ${status}); B2 lifecycle rules require a versioning-enabled bucket"
  fi
}

apply_policy() {
  local policy_file="${WORK_DIR}/policy.json"
  local rule_count

  if ! generate_policy >"${policy_file}"; then
    fail "could not generate the lifecycle policy"
  fi
  rule_count="$(jq '.Rules | length' "${policy_file}")"

  if ! aws_s3api put-bucket-lifecycle-configuration \
    --bucket "${BUCKET}" \
    --lifecycle-configuration "file://${policy_file}" >/dev/null; then
    fail "could not apply the lifecycle configuration to bucket ${BUCKET}"
  fi

  printf 'applied %s-rule lifecycle policy to bucket %s (retention %s days, delete after %s day)\n' \
    "${rule_count}" \
    "${BUCKET}" \
    "${RETENTION_DAYS}" \
    "${NONCURRENT_DELETE_DELAY_DAYS}"
  printf 'run %s --check to verify the bucket matches\n' "$0"
}

check_policy() {
  local policy_file="${WORK_DIR}/policy.json"
  local remote_file="${WORK_DIR}/remote.json"
  local error_file="${WORK_DIR}/get-lifecycle.err"
  local remote_rules desired_rules rule_count

  if ! generate_policy >"${policy_file}"; then
    fail "could not generate the lifecycle policy"
  fi
  rule_count="$(jq '.Rules | length' "${policy_file}")"

  if ! aws_s3api get-bucket-lifecycle-configuration \
    --bucket "${BUCKET}" \
    --output json >"${remote_file}" 2>"${error_file}"; then
    if grep -q -- 'NoSuchLifecycleConfiguration' "${error_file}"; then
      fail "bucket ${BUCKET} has no lifecycle configuration; review --print output and run --apply"
    fi
    printf 'error: could not read the lifecycle configuration of bucket %s:\n' "${BUCKET}" >&2
    cat "${error_file}" >&2
    exit 1
  fi

  remote_rules="$(canonical_rules <"${remote_file}")"
  desired_rules="$(canonical_rules <"${policy_file}")"

  if [[ "${remote_rules}" == "${desired_rules}" ]]; then
    printf 'bucket %s lifecycle configuration matches the generated policy (%s rules)\n' \
      "${BUCKET}" \
      "${rule_count}"
    return
  fi

  printf 'lifecycle configuration drift on bucket %s\n' "${BUCKET}" >&2
  printf 'normalized remote (left) and desired (right) rules; rule IDs are ignored because B2 regenerates them:\n' >&2
  diff -u \
    --label remote \
    --label desired \
    <(printf '%s\n' "${remote_rules}") \
    <(printf '%s\n' "${desired_rules}") >&2 || true
  exit 1
}

main() {
  parse_args "$@"
  load_settings

  case "${MODE}" in
    print)
      if [[ -n "${OUTPUT}" ]]; then
        if ! generate_policy >"${OUTPUT}"; then
          fail "could not write the lifecycle policy to ${OUTPUT}"
        fi
        printf 'wrote lifecycle policy to %s\n' "${OUTPUT}" >&2
      else
        generate_policy
      fi
      ;;
    check)
      prepare_aws_environment
      check_bucket_versioning
      check_policy
      ;;
    apply)
      prepare_aws_environment
      check_bucket_versioning
      apply_policy
      ;;
    *)
      die "unknown mode: ${MODE}"
      ;;
  esac
}

main "$@"
