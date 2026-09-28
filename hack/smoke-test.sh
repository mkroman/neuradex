#!/bin/sh
# Run the deployment smoke checks against a running neuradex service.
#
# Asserts the HTTP status codes of the endpoints a healthy deployment must
# serve. The deployment-test workflow pipes this script into the shell of a
# curl pod inside the kind cluster:
#
#   kubectl exec -i curl -- sh -s http://neuradex < hack/smoke-test.sh
#
# Usage: smoke-test.sh <base-url>
set -eu

base_url="${1:?usage: smoke-test.sh <base-url>}"

check() {
  path="${1}"
  expected="${2}"
  code="$(curl -sS -o /dev/null --write-out '%{http_code}' "${base_url}${path}")"
  echo "${path}: ${code}"
  if [ "${code}" != "${expected}" ]; then
    echo "smoke check failed: ${path}: expected ${expected}, got ${code}" >&2
    exit 1
  fi
}

check /healthz 204
check '/v1/fetch?url=https://example.com' 200
check /openapi.json 200
check /swagger-ui/ 200

echo "all smoke checks passed"
