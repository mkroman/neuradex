-- ClickHouse DDL for the local neuradex analytics stack.
--
-- The official image runs this file only when /var/lib/clickhouse is empty. After changing the
-- DDL, either reset the clickhouse_data volume or apply the change manually. All statements are
-- IF NOT EXISTS so a fresh start is idempotent.
--
-- No response bodies or search result text are stored here. `query_text` is optional and
-- restricted; dashboards must aggregate with `query_hash` instead.

CREATE DATABASE IF NOT EXISTS neuradex;

CREATE TABLE IF NOT EXISTS neuradex.events
(
    -- Envelope and correlation.
    schema_version          UInt16,
    event                   LowCardinality(String),
    event_id                String,
    timestamp               DateTime64(3, 'UTC'),
    request_id              String,
    trace_id                String,
    span_id                 String,

    -- Request metadata.
    route                   LowCardinality(String),
    method                  LowCardinality(String),
    status                  UInt16,
    duration_ms             UInt32,
    outcome                 LowCardinality(String),
    error_type              LowCardinality(String),
    service_version         String,

    -- Authenticated principal.
    auth_subject            String,
    auth_method             LowCardinality(String),
    user_id                 String,

    -- Search metadata. query_text is policy-gated; query_hash is always safe to aggregate.
    query_text              String,
    query_hash              String,
    result_count            UInt32,

    -- Target metadata. target_url is sanitized (no userinfo, fragment, or query values).
    target_url              String,
    upstream_host           String,
    redirects_followed      UInt16,

    -- Content references. Bodies live in B2 packfiles, never here.
    content_captured        Bool,
    content_kind            LowCardinality(String),
    content_hash_blake3     String,
    content_hash_sha256     String,
    content_bytes           UInt64,
    content_type            LowCardinality(String),
    content_truncated       Bool,
    content_pack_id         String,
    content_frame_offset    UInt64,
    content_frame_length    UInt32,
    content_capture_error   LowCardinality(String),

    -- Header JSON strings. Values are redacted/HMACed by the audit crate.
    request_headers         String,
    response_headers        String,
    request_headers_truncated  Bool,
    response_headers_truncated Bool,

    -- Read audit. Empty for normal request events.
    read_action             LowCardinality(String),
    read_target             String,
    read_reason             String,
    read_bytes              UInt64
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
    event               LowCardinality(String),
    route               LowCardinality(String),
    method              LowCardinality(String),
    status_class        LowCardinality(String),
    outcome             LowCardinality(String),
    requests            SimpleAggregateFunction(sum, UInt64),
    errors              SimpleAggregateFunction(sum, UInt64),
    duration_ms_sum     SimpleAggregateFunction(sum, UInt64),
    duration_ms_max     SimpleAggregateFunction(max, UInt64)
)
ENGINE = AggregatingMergeTree
PARTITION BY toYYYYMM(day)
ORDER BY (day, event, route, method, status_class, outcome)
TTL day + INTERVAL 365 DAY
SETTINGS ttl_only_drop_parts = 1;

CREATE MATERIALIZED VIEW IF NOT EXISTS neuradex.events_daily_mv
TO neuradex.events_daily
AS
SELECT
    toDate(timestamp)                                      AS day,
    event,
    route,
    method,
    concat(toString(intDiv(status, 100)), 'xx')            AS status_class,
    outcome,
    count()                                                AS requests,
    countIf(status >= 500)                                 AS errors,
    sum(duration_ms)                                       AS duration_ms_sum,
    max(toUInt64(duration_ms))                             AS duration_ms_max
FROM neuradex.events
GROUP BY day, event, route, method, status_class, outcome;

-- Daily content captured per upstream host and content type. `distinct_content` is an
-- AggregateFunction(uniq, String); read it with uniqMerge.
CREATE TABLE IF NOT EXISTS neuradex.content_daily
(
    day                 Date,
    upstream_host       String,
    content_type        LowCardinality(String),
    fetches             SimpleAggregateFunction(sum, UInt64),
    bytes_captured      SimpleAggregateFunction(sum, UInt64),
    truncated           SimpleAggregateFunction(sum, UInt64),
    distinct_content    AggregateFunction(uniq, String)
)
ENGINE = AggregatingMergeTree
PARTITION BY toYYYYMM(day)
ORDER BY (day, upstream_host, content_type)
TTL day + INTERVAL 365 DAY
SETTINGS ttl_only_drop_parts = 1;

CREATE MATERIALIZED VIEW IF NOT EXISTS neuradex.content_daily_mv
TO neuradex.content_daily
AS
SELECT
    toDate(timestamp)                   AS day,
    upstream_host,
    content_type,
    count()                             AS fetches,
    sum(content_bytes)                  AS bytes_captured,
    countIf(content_truncated)          AS truncated,
    uniqState(content_hash_blake3)      AS distinct_content
FROM neuradex.events
WHERE event = 'fetch'
  AND content_captured
  AND content_hash_blake3 != ''
GROUP BY day, upstream_host, content_type;
