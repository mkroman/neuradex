#!/bin/sh
# Install cargo-chef or cargo-auditable from its checksummed GitHub release.
#
# The release's `sha256.sum` manifest is verified against the digest pinned
# below, and the downloaded archive against the manifest before it is
# installed. Renovate keeps `version` and `manifest_sha256` up to date via the
# `github-release-attachments` datasource.
#
# Usage: install-cargo-tool.sh <cargo-chef|cargo-auditable> <dest-dir>
set -eu

tool="${1:?usage: install-cargo-tool.sh <cargo-chef|cargo-auditable> <dest-dir>}"
dest="${2:?usage: install-cargo-tool.sh <cargo-chef|cargo-auditable> <dest-dir>}"

case "${tool}" in
  cargo-chef)
    package_name="LukeMathWalker/cargo-chef"
    version="v0.1.78"
    manifest_sha256="01d5e93f6c5446e8e4d5eb91d1b49331b9c4cbb743b88f10158ae944c01c8d6f"
    ;;
  cargo-auditable)
    package_name="rust-secure-code/cargo-auditable"
    version="v0.7.6"
    manifest_sha256="4e19c1e94e59e2add3fc2076fd3a17a8c471b15ac414cb9bfe3ae024f851531f"
    ;;
  *)
    echo "unknown tool: ${tool}" >&2
    exit 1
    ;;
esac

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) target="x86_64-unknown-linux-gnu" ;;
  Linux-aarch64) target="aarch64-unknown-linux-gnu" ;;
  Darwin-arm64) target="aarch64-apple-darwin" ;;
  *)
    echo "unsupported platform: $(uname -s)-$(uname -m)" >&2
    exit 1
    ;;
esac

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  else
    shasum -a 256 "$1" | cut -d' ' -f1
  fi
}

asset="${tool}-${target}.tar.xz"
base_url="https://github.com/${package_name}/releases/download/${version}"

tmp="$(mktemp -d)"
trap 'rm -rf "${tmp}"' EXIT

curl -fsSLo "${tmp}/sha256.sum" "${base_url}/sha256.sum"
actual="$(sha256 "${tmp}/sha256.sum")"
if [ "${actual}" != "${manifest_sha256}" ]; then
  echo "sha256.sum digest mismatch for ${package_name} ${version}: ${actual}" >&2
  exit 1
fi

curl -fsSLo "${tmp}/${asset}" "${base_url}/${asset}"
expected="$(awk -v asset="${asset}" '$2 == "*" asset || $2 == asset { print $1; exit }' "${tmp}/sha256.sum")"
if [ -z "${expected}" ]; then
  echo "no checksum for ${asset} in ${package_name} ${version}" >&2
  exit 1
fi
actual="$(sha256 "${tmp}/${asset}")"
if [ "${actual}" != "${expected}" ]; then
  echo "checksum mismatch for ${asset}: ${actual}" >&2
  exit 1
fi

tar -xJf "${tmp}/${asset}" -C "${tmp}"
binary="$(find "${tmp}" -type f -name "${tool}" | head -n 1)"
install -m 0755 "${binary}" "${dest}/${tool}"
