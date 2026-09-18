# neuradex opencode tools

A `websearch` tool for [opencode](https://opencode.ai) that queries this service's
`/v1/search` endpoint. The tool id is `websearch`, so it overrides opencode's
built-in `websearch` tool everywhere.

## Setup

Symlink the tool into a directory opencode scans for custom tools (`tools/*.ts`):

    mkdir -p ~/.config/opencode/tools
    ln -s "$PWD/support/opencode/tools/websearch.ts" ~/.config/opencode/tools/websearch.ts

Alternatively, point opencode at this directory directly:

    OPENCODE_CONFIG_DIR="$PWD/support/opencode" opencode

## Enabling the override

The built-in `websearch` tool is only shown for certain providers, and opencode
applies that same filter to any tool with the `websearch` id — including this
override. Export one of the following so the tool is visible:

    export OPENCODE_ENABLE_EXA=1
    # or
    export OPENCODE_ENABLE_PARALLEL=1

Using the `opencode` or `opencode-go` provider also enables it.

## Configuration

- `NEURADEX_BASE_URL` — service base URL; defaults to `http://127.0.0.1:8080`
  (matches the service's `LISTEN_ADDR` default).

## Behavior

- Arguments: `query` (required), `limit` (optional, defaults to 3).
- Output: brief plain-text numbered list; `metadata` carries `result_count`
  and `total_ms`.
- API failures are surfaced as tool errors using the service's
  `{"error": {type, message}}` body.
