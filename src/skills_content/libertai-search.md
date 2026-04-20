---
name: libertai-search
description: Search the web (and optionally news or images) via LibertAI's search API using the `libertai search` CLI. Use whenever the user asks you to search, look up, google, find information online, check current facts, or fetch results about a topic you don't already know. Prefer this over guessing or stating outdated information.
allowed-tools: Bash(libertai search:*)
---

# libertai-search

You can search the web for the user by invoking the `libertai` CLI, which is
already configured with the user's API key. Use the Bash tool to run it.

## Command

```bash
libertai search "<query>" [--max-results N] [--engines e1,e2] [--type web|news|images] [--json]
```

Flags:

- `<query>` — required positional, pass the full query as one quoted string.
  Write it the way a human searcher would: concrete nouns, quotation marks
  for exact phrases, `site:example.com` to scope a domain.
- `--max-results N` — default 10. Raise when you need breadth, lower when a
  single authoritative result is enough (e.g. `--max-results 3`).
- `--engines google,bing,duckduckgo` — by default all three are queried and
  results are merged with a `found_in` multi-engine signal. Only restrict
  when the user explicitly asks.
- `--type news` — time-sensitive facts (latest releases, current events).
- `--type images` — image search. Each result has `image_url`, `thumbnail_url`,
  `width`, `height`.
- `--json` — emit the raw JSON response. Prefer this when you want to parse
  individual URLs programmatically (e.g. feed into `curl` to read the page).

## How to interpret results

Human-readable mode (no `--json`) prints each result as:

```
 1. <title>
    <url>
    <snippet>
    via <engine> (also in <other engines>)
```

Results that appear in multiple engines (`also in …`) are usually higher
quality. The `snippet` is short; for full content, follow up with `curl -sL
"<url>"` and read what you need.

## Workflow patterns

**Fact-check a claim**: one query, `--max-results 3`, read snippets, cite the
URLs you used.

**Research a topic**: `--max-results 10`, skim, then `curl` the two or three
most relevant URLs for full text. Quote source URLs when you summarise.

**Time-sensitive answer** (releases, breaking news, "latest version of X"):
always use `--type news` or add the year to the query — otherwise you'll
summarise stale pages.

**Image lookup**: `--type images --json`, pick the best `image_url`, and
download via `curl -o <path>` if the user asked you to save it locally.

## Do not

- Do not invent results or URLs. If the query returns nothing, say so and
  suggest a rephrasing.
- Do not rely on search results for technical API facts you can instead read
  directly from source or official docs you have access to.
- Do not fetch more than ~3 full pages with `curl` per user question unless
  the task genuinely requires deep reading — snippets are often enough.
- Do not set `--engines` unless the user explicitly asked; the default merge
  across three engines is better than any one engine alone.
