#!/usr/bin/env bash
#
# Install pinned CI tool releases from their upstream GitHub releases.
#
# Tool metadata lives in tools.txt as readable [tool] blocks. Renovate
# maintains every block's version and SHA-256 with the
# github-release-attachments datasource, so adding a tool only requires a new
# block.
#
# Every download is verified with SHA-256 before extraction. Raw assets are
# installed directly; archives use the binary path from the manifest. No
# third-party GitHub Actions, Go toolchain, or container images are involved.
#
# Usage:
#   ./hack/ci/install-tools.sh [BIN_DIR] [TOOL...]
#
# BIN_DIR defaults to ~/.local/bin. With no TOOL arguments, every tool in the
# manifest is installed. The script currently supports Linux x86_64.
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly SCRIPT_DIR
readonly TOOLS_FILE="${SCRIPT_DIR}/tools.txt"

BIN_DIR="${1:-${HOME}/.local/bin}"
shift || true
REQUESTED=("$@")

TEMP_DIR="$(mktemp -d)"

SECTION=""
REPO=""
VERSION=""
SHA256=""
TAG_TEMPLATE=""
ASSET_TEMPLATE=""
BINARY_TEMPLATE=""
ARCHIVE_FORMAT=""

declare -A REQUESTED_TOOLS=()
declare -A INSTALLED_TOOLS=()

cleanup() {
  rm -rf "${TEMP_DIR}"
}

trap cleanup EXIT

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

reset_section() {
  SECTION=""
  REPO=""
  VERSION=""
  SHA256=""
  TAG_TEMPLATE=""
  ASSET_TEMPLATE=""
  BINARY_TEMPLATE=""
  ARCHIVE_FORMAT=""
}

download() {
  local url="$1"
  local output="$2"

  curl --fail --silent --show-error --location --retry 3 \
    --output "${output}" "${url}"
}

verify_sha256() {
  local expected="$1"
  local archive="$2"

  printf '%s  %s\n' "${expected}" "${archive}" |
    sha256sum --check --status ||
    die "SHA-256 verification failed for ${archive}"
}

expand_template() {
  local template="$1"
  local version="$2"
  local version_nov="$3"

  template="${template//\$\{version_nov\}/${version_nov}}"
  printf '%s' "${template//\$\{version\}/${version}}"
}

install_tool() {
  local tool="$1"
  local repo="$2"
  local version="$3"
  local expected_sha="$4"
  local tag_template="$5"
  local asset_template="$6"
  local binary_template="$7"
  local archive_format="$8"
  local version_nov="${version#v}"
  local tag asset binary url archive

  tag="$(expand_template "${tag_template}" "${version}" "${version_nov}")"
  asset="$(expand_template "${asset_template}" "${version}" "${version_nov}")"
  binary="$(expand_template "${binary_template}" "${version}" "${version_nov}")"
  url="https://github.com/${repo}/releases/download/${tag}/${asset}"
  archive="${TEMP_DIR}/${tool}-download"

  download "${url}" "${archive}"
  verify_sha256 "${expected_sha}" "${archive}"

  case "${archive_format}" in
    raw)
      install -m 0755 "${archive}" "${BIN_DIR}/${tool}"
      ;;
    tar.gz)
      tar -xzf "${archive}" -C "${TEMP_DIR}"
      install -m 0755 "${TEMP_DIR}/${binary}" "${BIN_DIR}/${tool}"
      ;;
    tar.xz)
      tar -xJf "${archive}" -C "${TEMP_DIR}"
      install -m 0755 "${TEMP_DIR}/${binary}" "${BIN_DIR}/${tool}"
      ;;
    *)
      die "unsupported archive format for ${tool}: ${archive_format}"
      ;;
  esac
  printf 'Installed %s into %s\n' "${tool}" "${BIN_DIR}"
}

flush_section() {
  [[ -n "${SECTION}" ]] || return 0

  [[ -n "${REPO}" && -n "${VERSION}" && -n "${SHA256}" &&
    -n "${TAG_TEMPLATE}" && -n "${ASSET_TEMPLATE}" &&
    -n "${BINARY_TEMPLATE}" && -n "${ARCHIVE_FORMAT}" ]] ||
    die "incomplete tools.txt block for ${SECTION}"

  [[ "${SHA256}" =~ ^[a-f0-9]{64}$ ]] ||
    die "invalid sha256 for ${SECTION}"

  if ((${#REQUESTED_TOOLS[@]} == 0)) ||
    [[ -n "${REQUESTED_TOOLS[${SECTION}]:-}" ]]; then
    install_tool \
      "${SECTION}" \
      "${REPO}" \
      "${VERSION}" \
      "${SHA256}" \
      "${TAG_TEMPLATE}" \
      "${ASSET_TEMPLATE}" \
      "${BINARY_TEMPLATE}" \
      "${ARCHIVE_FORMAT}"
    INSTALLED_TOOLS["${SECTION}"]=1
  fi

  reset_section
}

main() {
  local tool key value line

  [[ "$(uname -s)" == "Linux" && "$(uname -m)" == "x86_64" ]] ||
    die "install-tools.sh currently supports Linux x86_64 only"

  [[ -r "${TOOLS_FILE}" ]] || die "missing tool manifest: ${TOOLS_FILE}"

  for tool in "${REQUESTED[@]}"; do
    REQUESTED_TOOLS["${tool}"]=1
  done

  mkdir -p "${BIN_DIR}"

  while IFS= read -r line; do
    [[ -n "${line}" && "${line}" != \#* ]] || continue

    if [[ "${line}" == \[*\] ]]; then
      flush_section
      SECTION="${line:1:${#line}-2}"
      continue
    fi

    [[ "${line}" == *=* ]] || die "invalid tools.txt line: ${line}"

    key="${line%%=*}"
    value="${line#*=}"

    [[ -n "${SECTION}" ]] || die "tools.txt key outside a block: ${key}"

    case "${key}" in
      repo)
        REPO="${value}"
        ;;
      version)
        VERSION="${value}"
        ;;
      sha256)
        SHA256="${value}"
        ;;
      tag)
        TAG_TEMPLATE="${value}"
        ;;
      asset)
        ASSET_TEMPLATE="${value}"
        ;;
      binary)
        BINARY_TEMPLATE="${value}"
        ;;
      archive)
        ARCHIVE_FORMAT="${value}"
        ;;
      *)
        die "unknown tools.txt key: ${key}"
        ;;
    esac
  done <"${TOOLS_FILE}"

  flush_section

  for tool in "${!REQUESTED_TOOLS[@]}"; do
    [[ -n "${INSTALLED_TOOLS[${tool}]:-}" ]] || die "unknown tool: ${tool}"
  done
}

main "$@"
