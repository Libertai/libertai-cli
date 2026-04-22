---
name: libertai-search
description: Search the web and read web pages via LibertAI's search API using the `libertai search` and `libertai fetch` CLIs. Use whenever the user asks you to search, look up, google, find information online, check current facts, or read the contents of a web page. Prefer this over guessing or stating outdated information.
allowed-tools: Bash(libertai search:*), Bash(libertai fetch:*)
---

# libertai-search

You can search the web and read individual pages for the user by invoking the
`libertai` CLI, which is already configured with the user's API key. Use the
Bash tool to run it.

## Commands

```bash
libertai search "<query>" [--max-results N] [--engines e1,e2] [--type web|news|images] [--json]
libertai fetch   "<url>"  [--json]
```

Use `search` to discover URLs, then `fetch` to read the full text of the ones
that matter.

### `libertai search` flags

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
  individual URLs programmatically.

### `libertai fetch` flags

- `<url>` — required positional, a full `http(s)://` URL.
- `--json` — emit the raw JSON response (`url`, `title`, `content`,
  `word_count`) instead of a human-readable block.

`fetch` returns the cleaned article text of the page — navigation, ads, and
sidebars are stripped server-side. Prefer it over `curl` when you want to
*read* a page, since `curl` hands back raw HTML you then have to parse.

## How to interpret results

Human-readable search mode (no `--json`) prints each result as:

```
 1. <title>
    <url>
    <snippet>
    via <engine> (also in <other engines>)
```

Results that appear in multiple engines (`also in …`) are usually higher
quality. The `snippet` is short; for full content, pass the URL to
`libertai fetch` and read what you need.

`libertai fetch` human-readable output is:

```
<title>
<url>
<word_count> words

<article body…>
```

## Workflow patterns

**Fact-check a claim**: one search query, `--max-results 3`, read snippets,
cite the URLs you used.

**Research a topic**: `libertai search --max-results 10`, skim, then
`libertai fetch "<url>"` on the two or three most relevant URLs for full
text. Quote source URLs when you summarise.

**Read a specific page the user mentioned**: skip search entirely and go
straight to `libertai fetch "<url>"`.

**Time-sensitive answer** (releases, breaking news, "latest version of X"):
always use `--type news` or add the year to the query — otherwise you'll
summarise stale pages.

**Image lookup**: `--type images --json`, pick the best `image_url`, and
download via `curl -o <path>` if the user asked you to save it locally. (Do
not use `libertai fetch` for images — it is a text extractor.)

## Do not

- Do not invent results or URLs. If the query returns nothing, say so and
  suggest a rephrasing.
- Do not rely on search results for technical API facts you can instead read
  directly from source or official docs you have access to.
- Do not fetch more than ~3 full pages per user question unless the task
  genuinely requires deep reading — snippets are often enough.
- Do not set `--engines` unless the user explicitly asked; the default merge
  across three engines is better than any one engine alone.
- Do not use `libertai fetch` on binary URLs (PDFs, images, archives) — it
  expects HTML-ish pages and returns extracted text.
