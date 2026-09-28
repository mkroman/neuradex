//! Neuradex web search provider for opencode v2.
//!
//! Registers this service's `/v1/search` endpoint as an opencode websearch
//! provider named `neuradex` and selects it as the default provider, so the
//! built-in `websearch` tool searches through Neuradex. See README.md in this
//! directory for setup.

import { Plugin } from "@opencode/plugin"

const DEFAULT_LIMIT = 10
const DEFAULT_BASE_URL = "http://127.0.0.1:8080"

type SearchResponse = {
  results: { title: string; url: string; description: string }[]
  metrics: { total_ms: number; result_count: number }
}

type ErrorBody = { error?: { message?: string } }

export default Plugin.define({
  id: "neuradex.websearch",
  async setup(ctx) {
    await ctx.websearch.transform((editor) => {
      editor.add({
        id: "neuradex",
        name: "Neuradex",
        execute: async ({ query }, { signal }) => {
          const base = (process.env.NEURADEX_BASE_URL ?? DEFAULT_BASE_URL).replace(/\/+$/, "")
          const url = new URL("/v1/search", base)
          url.searchParams.set("q", query)
          url.searchParams.set("limit", String(DEFAULT_LIMIT))

          let response: Response
          try {
            response = await fetch(url, { signal, headers: { accept: "application/json" } })
          } catch (error) {
            throw new Error(`websearch: request failed: ${error instanceof Error ? error.message : String(error)}`)
          }

          if (!response.ok) {
            const body = (await response.json().catch(() => null)) as ErrorBody | null
            const message = body?.error?.message ?? response.statusText
            throw new Error(`websearch: search failed (${response.status}): ${message}`)
          }

          const body = (await response.json()) as SearchResponse

          return body.results.map((result) => ({
            url: result.url,
            title: result.title,
            content: result.description || undefined,
            time: {},
          }))
        },
      })

      editor.default.set("neuradex")
    })
  },
})
