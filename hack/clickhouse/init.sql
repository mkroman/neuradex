-- ClickHouse DDL for the local neuradex analytics stack.
--
-- The columns mirror the serialized `neuradex-audit` AuditEvent (schema_version 1.0.0) after
-- Vector flattens the nested `service`, `http`, and `operation` objects. The event itself is
-- nested; see docs/reference/event-fields.md. Derived columns are marked below.
--
-- No bodies, query text, auth subject, or read-audit fields are present: the audit crate does
-- not emit them. The content_references column is the JSON array from the event; the
-- content_* summary columns are derived by Vector for capacity queries, not fields of the
-- AuditEvent.
--
-- The official image runs this file only when /var/lib/clickhouse is empty. After changing the
-- DDL, either reset the clickhouse_data volume or apply the change manually.

CREATE DATABASE IF NOT EXISTS neuradex;

CREATE TABLE IF NOT EXISTS neuradex.events
(
    -- Envelope and correlation.
    event                       LowCardinality(String),
    schema_version              LowCardinality(String),
    event_id                    String,
    timestamp                   DateTime64(3, 'UTC'),
    observed_at                 Nullable(DateTime64(3, 'UTC')),
    trace_id                    String,
    span_id                     String,
    request_id                  String,

    -- service { name, version, environment? }
    service_name                LowCardinality(String),
    service_version             String,
    service_environment         LowCardinality(String),

    -- http { method, route?, path, status?, request_body_bytes?, response_body_bytes?,
    --        duration_ms }
    http_method                 LowCardinality(String),
    http_route                  LowCardinality(String),
    http_path                   String,
    http_status                 UInt16,
    http_request_body_bytes     UInt64,
    http_response_body_bytes    UInt64,
    http_duration_ms            UInt64,

    outcome                     LowCardinality(String),
    error_type                  LowCardinality(String),

    -- operation { type, ...union fields }
    operation_type              LowCardinality(String),

    -- search / openwebui_search
    query_length                UInt64,
    query_terms                 UInt64,
    query_sha256                String,
    query_limit                 UInt64,
    query_count                 UInt64,
    auth_mode                   LowCardinality(String),
    queue_ms                    UInt64,
    upstream_ms                 UInt64,
    upstream_result_count       UInt64,
    returned_result_count       UInt64,
    zero_results                Bool,
    result_digest               String,
    top_hosts                   String,

    -- fetch / peek
    target_scheme               LowCardinality(String),
    target_host                 String,
    target_port                 UInt16,
    target_path                 String,
    target_url_sha256           String,
    redirects                   UInt32,
    final_host                  String,
    upstream_status             UInt16,
    content_type                LowCardinality(String),
    content_length              UInt64,
    bytes_read                  UInt64,
    ttfb_ms                     UInt64,
    total_ms                    UInt64,
    truncated                   Bool,
    stopped_at_head             Bool,

    -- infrastructure
    infrastructure_kind         LowCardinality(String),

    -- content_references [ { storage, key, blake3, sha256, size, content_type? } ]
    content_references          String,

    -- Derived from content_references by Vector, not part of AuditEvent. Useful for capacity
    -- queries without parsing the JSON array on every scan.
    content_reference_count     UInt32,
    content_bytes               UInt64,
    content_storage             LowCardinality(String),
    content_key                 String,
    content_blake3              String,
    content_sha256              String,

    -- HeaderCapture objects, encoded as JSON text.
    request_headers             String,
    response_headers            String
)
ENGINE = MergeTree
PARTITION BY toDate(timestamp)
ORDER BY (timestamp, request_id)
TTL timestamp + INTERVAL 365 DAY
SETTINGS
    index_granularity = 8192,
    -- Drop whole expired parts; avoids rewriting parts on every TTL merge.
    ttl_only_drop_parts = 1;

-- Daily request/error/latency rollup. AggregatingMergeTree keeps the state mergeable while
-- allowing plain inserts from the materialized view.
CREATE TABLE IF NOT EXISTS neuradex.events_daily
(
    day                 Date,
    operation_type      LowCardinality(String),
    http_route          LowCardinality(String),
    http_method         LowCardinality(String),
    status_class        LowCardinality(String),
    outcome             LowCardinality(String),
    requests            SimpleAggregateFunction(sum, UInt64),
    errors              SimpleAggregateFunction(sum, UInt64),
    duration_ms_sum     SimpleAggregateFunction(sum, UInt64),
    duration_ms_max     SimpleAggregateFunction(max, UInt64)
)
ENGINE = AggregatingMergeTree
PARTITION BY toYYYYMM(day)
ORDER BY (day, operation_type, http_route, http_method, status_class, outcome)
TTL day + INTERVAL 365 DAY
SETTINGS ttl_only_drop_parts = 1;

CREATE MATERIALIZED VIEW IF NOT EXISTS neuradex.events_daily_mv
TO neuradex.events_daily
AS
SELECT
    toDate(timestamp)                                      AS day,
    operation_type,
    http_route,
    http_method,
    concat(toString(intDiv(http_status, 100)), 'xx')       AS status_class,
    outcome,
    count()                                                AS requests,
    countIf(http_status >= 500)                            AS errors,
    sum(http_duration_ms)                                  AS duration_ms_sum,
    max(http_duration_ms)                                  AS duration_ms_max
FROM neuradex.events
GROUP BY day, operation_type, http_route, http_method, status_class, outcome;

-- Daily content captured per upstream host and content type. `distinct_content` is an
-- AggregateFunction(uniq, String); read it with uniqMerge. The content_* columns are derived
-- from content_references, so this view only covers events that actually carry a reference.
CREATE TABLE IF NOT EXISTS neuradex.content_daily
(
    day                 Date,
    target_host         String,
    content_type        LowCardinality(String),
    fetches             SimpleAggregateFunction(sum, UInt64),
    bytes_captured      SimpleAggregateFunction(sum, UInt64),
    truncated           SimpleAggregateFunction(sum, UInt64),
    distinct_content    AggregateFunction(uniq, String)
)
ENGINE = AggregatingMergeTree
PARTITION BY toYYYYMM(day)
ORDER BY (day, target_host, content_type)
TTL day + INTERVAL 365 DAY
SETTINGS ttl_only_drop_parts = 1;

CREATE MATERIALIZED VIEW IF NOT EXISTS neuradex.content_daily_mv
TO neuradex.content_daily
AS
SELECT
    toDate(timestamp)                   AS day,
    target_host,
    content_type,
    count()                             AS fetches,
    sum(content_bytes)                  AS bytes_captured,
    countIf(truncated)                  AS truncated,
    uniqState(content_blake3)           AS distinct_content
FROM neuradex.events
WHERE operation_type = 'fetch'
  AND content_reference_count > 0
  AND content_blake3 != ''
GROUP BY day, target_host, content_type;
