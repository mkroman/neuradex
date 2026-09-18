import { tool } from "@opencode-ai/plugin"

const DEFAULT_LIMIT = 3
const DEFAULT_BASE_URL = "http://127.0.0.1:8080"

type SearchResponse = {
  results: { title: string; url: string; description: string }[]
  metrics: { total_ms: number; result_count: number }
}

type ErrorBody = { error?: { message?: string } }

export default tool({
  description:
    "Search the web for live, up-to-date information. Returns a ranked list of results with title, URL, and description. Use this proactively whenever you need recent or real-time details (news, releases, prices, docs, current events), whenever you don't know something or are unsure, or whenever your training data may be out of date — searching beats guessing.",
  args: {
    query: tool.schema.string().min(1).describe("The web search query"),
    limit: tool.schema.number().int().min(1).optional().describe("Maximum number of results to return. Defaults to 3"),
  },
  async execute(args, context) {
    const base = (process.env.NEURADEX_BASE_URL ?? DEFAULT_BASE_URL).replace(/\/+$/, "")
    const url = new URL("/v1/search", base)
    url.searchParams.set("q", args.query)
    url.searchParams.set("limit", String(args.limit ?? DEFAULT_LIMIT))

    let response: Response
    try {
      response = await fetch(url, { signal: context.abort, headers: { accept: "application/json" } })
    } catch (error) {
      throw new Error(`websearch: request failed: ${error instanceof Error ? error.message : String(error)}`)
    }

    if (!response.ok) {
      const body = (await response.json().catch(() => null)) as ErrorBody | null
      const message = body?.error?.message ?? response.statusText
      throw new Error(`websearch: search failed (${response.status}): ${message}`)
    }

    const body = (await response.json()) as SearchResponse

    if (body.results.length === 0) {
      return { title: `websearch: no results for "${args.query}"`, output: "No results." }
    }

    const output = body.results
      .map((result, index) => {
        const head = `${index + 1}. ${result.title} — ${result.url}`
        return result.description ? `${head}\n   ${result.description}` : head
      })
      .join("\n")

    return {
      title: `websearch: ${body.results.length} results for "${args.query}"`,
      output,
      metadata: { result_count: body.results.length, total_ms: body.metrics.total_ms },
    }
  },
})
