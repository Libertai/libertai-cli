---
name: libertai-chat-research
description: Search the web, fetch pages, verify current facts, and summarize sources. Use for web research, finding information, fact checks, current events, and URL reading.
allowed-tools: search fetch
metadata:
  libertai.pillars: chat
  libertai.bundle: builtin
---

# LibertAI Chat Research

Use this skill for folderless chat research and questions that need current or sourced information.

## Workflow

- Use `search` for current facts, news, discovery, or when the user asks to find something.
- Use `fetch` to read specific URLs or the strongest search results before summarizing them.
- Keep source provenance clear when web tools influence the response.
- Separate what sources say from your own inference.
- Do not use local project filesystem or shell tools unless the user explicitly makes the chat about local files.

## Image-Oriented Research

When the task is image search or visual discovery, return image URLs and metadata from available search results. Do not claim to inspect image bytes unless a vision-capable tool supplied that content.
