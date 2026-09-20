# neuradex opencode tools

An [opencode](https://opencode.ai) v2 plugin that registers this service's
`/v1/search` endpoint as a web search provider named `neuradex` and selects it
as the default provider, so opencode's built-in `websearch` tool searches
through Neuradex.

## Setup

Symlink the plugin into the global plugins directory opencode scans:

    mkdir -p ~/.config/opencode/plugins
    ln -s "$PWD/support/opencode/plugins/websearch.ts" ~/.config/opencode/plugins/websearch.ts

The plugin imports `@opencode/plugin`, which is released in lockstep with
opencode — pin both to the same release and bump them together. Module
imports resolve from the plugin file's real path, so the dependency must be
installed here in the repository (one-time per clone; `node_modules` is
gitignored):

    npm install

Alternatively, point opencode at this directory directly:

    OPENCODE_CONFIG_DIR="$PWD/support/opencode" opencode

Resolution is identical either way — in both modes the import resolves from
this directory. Copying the file into the global plugins directory instead of
symlinking also works (that is the fully documented variant), but then updates
to the plugin no longer follow the repository.

## Reloading

Opencode reloads plugins when files under the watched config directory change.
A fresh `npm install` here counts as an unwatched dependency change — run
`opencode service restart` after installing or bumping `@opencode/plugin`.

## Behavior

- The built-in `websearch` tool is always visible unless websearch is disabled
  (the `"websearch": false` config key or the "Disable" choice in the provider
  prompt). The v1-era `OPENCODE_ENABLE_EXA`/`OPENCODE_ENABLE_PARALLEL` gates
  no longer exist.
- Searches use the `websearch` permission action; unless it is allowed in
  `opencode.json(c)`, the first search asks, and the answer is remembered.
- Because the plugin sets the default provider, no provider-selection prompt
  appears. A `"websearch": {"provider": ...}` setting in `opencode.json(c)`
  takes precedence over the plugin's default.
- The query is the only model-facing argument; the provider caps results at
  10. Results render through the built-in tool as `## [title](url)` markdown
  with the snippet as content, and "No search results found." when empty.
- Provider errors surface as the built-in tool's generic failure message
  ("Unable to search the web for …"); the underlying error is visible in
  diagnostics and server logs.

## Configuration

- `NEURADEX_BASE_URL` — service base URL; defaults to `http://127.0.0.1:8080`
  (matches the service's `LISTEN_ADDR` default).

## API failures

- Errors thrown by the provider carry the service's
  `{"error": {type, message}}` body text in the message
  (`websearch: search failed (N): …`); connection failures read
  `websearch: request failed: …`. The built-in tool wraps both in its generic
  message, so watch the logs when debugging.
- The service must be running for searches to work (see the repository's
  `AGENTS.md` for running it locally).
