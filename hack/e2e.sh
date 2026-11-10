#!/usr/bin/env bash
# Hermetic in-cluster end-to-end suite for the neuradex Helm deployment.
#
# Deploys deterministic fixture and MinIO services, installs the chart with S3
# content capture enabled, drives the API through a port-forward, and asserts
# the HTTP bodies, the audit events, and the stored content objects. CI runs
# this after loading the freshly built image into the kind cluster.
#
# Usage:
#   hack/e2e.sh
#
# Environment:
#   E2E_NAMESPACE         namespace to create and delete (default: neuradex-e2e)
#   E2E_IMAGE             optional <repository>:<tag>; overrides the two below
#   E2E_IMAGE_REPOSITORY  image repository (default: ghcr.io/mkroman/neuradex)
#   E2E_IMAGE_TAG         image tag (default: ci)
#   E2E_PORT              local port for the service port-forward (default: 18080)
#   E2E_KEEP=1            keep the namespace after a successful run
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
namespace="${E2E_NAMESPACE:-neuradex-e2e}"
image_repository="${E2E_IMAGE_REPOSITORY:-ghcr.io/mkroman/neuradex}"
image_tag="${E2E_IMAGE_TAG:-ci}"
port="${E2E_PORT:-18080}"

if [[ -n "${E2E_IMAGE:-}" ]]; then
    if [[ "${E2E_IMAGE}" == *"@"* ]]; then
        echo "error: E2E_IMAGE must be <repository>:<tag>, not a digest reference" >&2
        exit 2
    fi
    image_tag="${E2E_IMAGE##*:}"
    image_repository="${E2E_IMAGE%:*}"
    if [[ "${image_tag}" == "${E2E_IMAGE}" || -z "${image_tag}" || "${image_tag}" == */* ]]; then
        echo "error: E2E_IMAGE must be <repository>:<tag>" >&2
        exit 2
    fi
fi

for tool in kubectl helm curl jq; do
    if ! command -v "${tool}" >/dev/null 2>&1; then
        echo "error: required tool not found: ${tool}" >&2
        exit 2
    fi
done

if ! kubectl cluster-info >/dev/null 2>&1; then
    echo "error: no reachable Kubernetes cluster (kubectl cluster-info failed)" >&2
    exit 2
fi

tmp_dir="$(mktemp -d)"
response_body="${tmp_dir}/response.body"
audit_events="${tmp_dir}/audit.jsonl"
content_keys="${tmp_dir}/content-keys.txt"
port_forward_log="${tmp_dir}/port-forward.log"
port_forward_pid=""

cleanup() {
    local status=$?

    if [[ -n "${port_forward_pid}" ]]; then
        kill "${port_forward_pid}" 2>/dev/null || true
        wait "${port_forward_pid}" 2>/dev/null || true
    fi

    rm -rf "${tmp_dir}"

    if [[ "${status}" -eq 0 ]]; then
        if [[ "${E2E_KEEP:-0}" == "1" ]]; then
            echo "keeping namespace ${namespace} (E2E_KEEP=1)"
        else
            echo "deleting namespace ${namespace} (set E2E_KEEP=1 to keep it)"
            kubectl delete namespace "${namespace}" --ignore-not-found --timeout=120s >/dev/null 2>&1 || true
        fi
    fi
}
trap cleanup EXIT

fail() {
    local message="$1"

    # Diagnostics must all run, and must never mask the original failure.
    set +e
    {
        echo "e2e failed: ${message}"
        echo
        echo "--- namespace pods ---"
        kubectl get pods --namespace "${namespace}" -o wide
        echo
        echo "--- namespace events ---"
        kubectl get events --namespace "${namespace}" --sort-by=.lastTimestamp
        echo
        echo "--- neuradex logs (last 200 lines) ---"
        kubectl logs --namespace "${namespace}" deploy/neuradex --tail=200
        echo
        echo "--- minio objects ---"
        kubectl exec --namespace "${namespace}" deploy/minio -- mc ls --recursive e2e/neuradex
    } >&2

    exit 1
}

# Sends a request to the port-forwarded service and leaves the body in
# ${response_body}. Prints the HTTP status code.
request() {
    local method="$1"
    local path="$2"
    local data="${3-}"
    local args=(
        --silent
        --show-error
        --max-time 60
        --request "${method}"
        --output "${response_body}"
        --write-out '%{http_code}'
    )

    if [[ -n "${data}" ]]; then
        args+=(--header 'content-type: application/json' --data "${data}")
    fi

    curl "${args[@]}" "${base_url}${path}"
}

# expect_status <description> <expected-status> <method> <path> [json-body]
expect_status() {
    local description="$1"
    local expected="$2"
    shift 2
    local status

    if ! status="$(request "$@")"; then
        fail "${description}: the request failed"
    fi

    if [[ "${status}" != "${expected}" ]]; then
        fail "${description}: expected HTTP ${expected}, got ${status}; body: $(cat "${response_body}")"
    fi
}

# expect_jq <description> <filter>
expect_jq() {
    local description="$1"
    local filter="$2"

    if ! jq --exit-status "${filter}" "${response_body}" >/dev/null; then
        fail "${description}: jq assertion failed: ${filter}; body: $(cat "${response_body}")"
    fi
}

# Audit event filters.

fetch_event='select(.operation.type == "fetch" and .http.route == "/v1/fetch" and .http.status == 200 and ((.content_references // []) | length) > 0)'
peek_event='select(.operation.type == "peek" and .http.status == 200 and ((.content_references // []) | length) > 0)'
search_event='select(.operation.type == "search" and .http.status == 200 and ((.operation.query_sha256 // "") | length) == 64 and ((.operation | has("query")) | not) and ((.content_references // []) | length) >= 2)'
openwebui_event='select(.operation.type == "openwebui_search" and .http.status == 200 and .operation.count == 1 and ((.content_references // []) | length) >= 2)'
not_found_event='select(.http.status == 404 and .http.path == "/v1/missing" and ((.http | has("route")) | not))'
openapi_event='select(.operation.type == "infrastructure" and .operation.kind == "openapi")'
docs_event='select(.operation.type == "infrastructure" and .operation.kind == "docs")'

collect_audit_events() {
    kubectl logs --namespace "${namespace}" deploy/neuradex --since=20m 2>/dev/null \
        | jq --raw-input --compact-output 'fromjson? | select(.event=="neuradex.request")' \
        >"${audit_events}"
}

audit_event_exists() {
    local filter="$1"

    jq --exit-status "${filter}" "${audit_events}" >/dev/null 2>&1
}

# audit_expect <description> <filter>
audit_expect() {
    local description="$1"
    local filter="$2"

    if ! audit_event_exists "${filter}"; then
        fail "${description}: no matching audit event (filter: ${filter})"
    fi
}

echo "e2e: namespace=${namespace} image=${image_repository}:${image_tag}"

kubectl create namespace "${namespace}" --dry-run=client -o yaml | kubectl apply -f - \
    || fail "could not create the namespace"

printf '%s' 'not text' >"${tmp_dir}/binary.txt"

kubectl create configmap e2e-fixtures \
    --namespace "${namespace}" \
    --from-file=page.html="${repo_root}/hack/e2e/fixtures/page.html" \
    --from-file=search.html="${repo_root}/hack/e2e/fixtures/search.html" \
    --from-file=search_stream_sse.bin="${repo_root}/crates/test-support/fixtures/kagi/search_stream_sse.bin" \
    --from-file=binary.txt="${tmp_dir}/binary.txt" \
    --dry-run=client -o yaml \
    | kubectl apply -f - \
    || fail "could not create the e2e-fixtures ConfigMap"

kubectl create configmap e2e-nginx \
    --namespace "${namespace}" \
    --from-file=default.conf="${repo_root}/hack/e2e/nginx.conf" \
    --dry-run=client -o yaml \
    | kubectl apply -f - \
    || fail "could not create the e2e-nginx ConfigMap"

kubectl apply --namespace "${namespace}" --filename "${repo_root}/hack/e2e/manifests.yaml" \
    || fail "could not apply hack/e2e/manifests.yaml"

kubectl rollout status deployment/fixtures --namespace "${namespace}" --timeout=120s \
    || fail "the fixtures deployment did not become ready"

kubectl rollout status deployment/minio --namespace "${namespace}" --timeout=120s \
    || fail "the MinIO deployment did not become ready"

bucket_ready=false
for ((attempt = 1; attempt <= 30; attempt++)); do
    if kubectl exec --namespace "${namespace}" deploy/minio -- mc mb --ignore-existing e2e/neuradex >/dev/null 2>&1; then
        bucket_ready=true
        break
    fi
    sleep 2
done
if [[ "${bucket_ready}" != true ]]; then
    fail "could not create the MinIO bucket e2e/neuradex"
fi

helm upgrade --install neuradex "${repo_root}/chart/neuradex" \
    --namespace "${namespace}" \
    --values "${repo_root}/hack/e2e/values.yaml" \
    --set "image.repository=${image_repository}" \
    --set "image.tag=${image_tag}" \
    --set image.pullPolicy=Never \
    --wait --timeout 5m \
    || fail "helm upgrade --install failed"

base_url="http://127.0.0.1:${port}"
kubectl port-forward --namespace "${namespace}" svc/neuradex "${port}:80" \
    >"${port_forward_log}" 2>&1 &
port_forward_pid=$!

port_forward_ready=false
for ((attempt = 1; attempt <= 60; attempt++)); do
    if ! kill -0 "${port_forward_pid}" 2>/dev/null; then
        fail "the port-forward exited before the service became reachable"
    fi
    status="$(curl --silent --output /dev/null --max-time 2 --write-out '%{http_code}' "${base_url}/healthz" || true)"
    if [[ "${status}" == "204" ]]; then
        port_forward_ready=true
        break
    fi
    sleep 1
done
if [[ "${port_forward_ready}" != true ]]; then
    fail "timed out waiting for the port-forward to reach /healthz"
fi

# Health.

expect_status 'GET /healthz' 204 GET /healthz
if [[ -s "${response_body}" ]]; then
    fail "GET /healthz must have an empty body"
fi

# Infrastructure endpoints.

expect_status 'GET /openapi.json' 200 GET /openapi.json
expect_jq 'openapi paths' '(.paths | has("/v1/fetch") and has("/v1/peek") and has("/v1/search") and has("/v1/openwebui_search") and has("/healthz"))'

expect_status 'GET /docs' 200 GET /docs
if ! grep -Eqi '<html|<!doctype html' "${response_body}"; then
    fail "GET /docs did not return an HTML document"
fi

# Peek.

expect_status 'GET /v1/peek' 200 GET '/v1/peek?url=http%3A%2F%2Ffixtures%2Fpage'
expect_jq 'peek title' '.metadata.title == "Neuradex E2E Fixture"'
expect_jq 'peek description' '.metadata.description == "A deterministic page served inside the kind cluster."'
expect_jq 'peek has no body' '(has("body") | not)'
expect_jq 'peek status' '.metrics.status == 200'
expect_jq 'peek bytes read' '.metrics.bytes_read > 0'
expect_jq 'peek truncation' '.metrics.truncated == false'

# Fetch.

expect_status 'GET /v1/fetch' 200 GET '/v1/fetch?url=http%3A%2F%2Ffixtures%2Fpage&include=headers,redirects'
expect_jq 'fetch title' '.metadata.title == "Neuradex E2E Fixture"'
expect_jq 'fetch description' '.metadata.description == "A deterministic page served inside the kind cluster."'
expect_jq 'fetch body marker' '.body | contains("E2E_BODY_MARKER")'
expect_jq 'fetch truncation' '.truncated == false'
expect_jq 'fetch status' '.metrics.status == 200'
expect_jq 'fetch redirect count' '.metrics.redirects_followed == 0'
expect_jq 'fetch content type' '.response_headers["content-type"] | startswith("text/html")'
expect_jq 'fetch user agent' '(.request_headers["user-agent"] | length) > 0'

expect_status 'GET /v1/fetch redirect' 200 GET '/v1/fetch?url=http%3A%2F%2Ffixtures%2Fredirect&include=redirects'
expect_jq 'redirect count' '.metrics.redirects_followed == 1'
expect_jq 'redirect final url' '.metrics.final_url == "http://fixtures/page"'

expect_status 'GET /v1/fetch binary' 415 GET '/v1/fetch?url=http%3A%2F%2Ffixtures%2Fbinary'
expect_jq 'binary error type' '.error.type == "unsupported_media_type"'

# Error envelope.

expect_status 'GET /v1/missing' 404 GET '/v1/missing'
expect_jq 'not-found error type' '.error.type == "not_found"'

expect_status 'POST /healthz' 405 POST /healthz
expect_jq 'method-not-allowed error type' '.error.type == "method_not_allowed"'

expect_status 'GET /v1/fetch invalid url' 400 GET '/v1/fetch?url=not-a-url'
expect_jq 'invalid-param error type' '.error.type == "invalid_param"'

# Search.

expect_status 'POST /v1/search' 200 POST /v1/search '{"query":"hello world","limit":1}'
expect_jq 'search query echo' '.query == "hello world"'
expect_jq 'search result count' '(.results | length) == 1'
expect_jq 'search first title' '.results[0].title | contains("Hello, world")'
expect_jq 'search first url' '.results[0].url | contains("wikipedia.org")'
expect_jq 'search metrics' '.metrics.result_count == 1'

expect_status 'POST /v1/openwebui_search' 200 POST /v1/openwebui_search '{"query":"hello world","count":1}'
expect_jq 'openwebui result count' 'length == 1'
expect_jq 'openwebui first link' '.[0].link | contains("wikipedia.org")'
expect_jq 'openwebui first title' '.[0].title | contains("Hello, world")'

expect_status 'POST /v1/search empty query' 400 POST /v1/search '{"query":""}'
expect_jq 'empty query error type' '.error.type == "invalid_param"'

# Audit events. kubectl log delivery can lag the response, so retry briefly.

audit_ready=false
for ((attempt = 1; attempt <= 15; attempt++)); do
    collect_audit_events || true
    if audit_event_exists "${fetch_event}" \
        && audit_event_exists "${peek_event}" \
        && audit_event_exists "${search_event}" \
        && audit_event_exists "${openwebui_event}" \
        && audit_event_exists "${not_found_event}" \
        && audit_event_exists "${openapi_event}" \
        && audit_event_exists "${docs_event}"; then
        audit_ready=true
        break
    fi
    sleep 2
done
if [[ "${audit_ready}" != true ]]; then
    fail "timed out waiting for the expected audit events"
fi

audit_expect 'fetch event' "${fetch_event}"
audit_expect 'peek event' "${peek_event}"
audit_expect 'search event' "${search_event}"
audit_expect 'openwebui search event' "${openwebui_event}"
audit_expect 'not-found event' "${not_found_event}"
audit_expect 'openapi infrastructure event' "${openapi_event}"
audit_expect 'docs infrastructure event' "${docs_event}"

if jq --exit-status 'select(.http.path == "/healthz")' "${audit_events}" >/dev/null 2>&1; then
    fail "/healthz must not emit an audit event"
fi

# Stored content. The service seals and uploads packs every five seconds, so
# each referenced key appears shortly after its request returned.

jq --raw-output '(.content_references // [])[] | .key' "${audit_events}" | sort -u >"${content_keys}"

if [[ ! -s "${content_keys}" ]]; then
    fail "the audit events carried no content references"
fi

printf 'e2e: verifying %s stored content objects\n' "$(wc -l <"${content_keys}")"
while IFS= read -r key; do
    object_found=false
    for ((attempt = 1; attempt <= 30; attempt++)); do
        if kubectl exec --namespace "${namespace}" deploy/minio -- mc stat "e2e/neuradex/${key}" >/dev/null 2>&1; then
            object_found=true
            break
        fi
        sleep 1
    done
    if [[ "${object_found}" != true ]]; then
        fail "content object e2e/neuradex/${key} never appeared in MinIO"
    fi
done <"${content_keys}"

if ! object_listing="$(kubectl exec --namespace "${namespace}" deploy/minio -- mc ls --recursive e2e/neuradex)"; then
    fail "could not list the MinIO bucket"
fi

for prefix in \
    'packs/fetch-response/' \
    'packs/peek-response/' \
    'packs/search-request/' \
    'packs/search-response/' \
    'packs/openwebui-search-request/' \
    'packs/openwebui-search-response/'; do
    if ! grep -Fq "${prefix}" <<<"${object_listing}"; then
        fail "no MinIO object under ${prefix}"
    fi
done

echo "e2e passed: neuradex ${image_repository}:${image_tag} in namespace ${namespace}"
