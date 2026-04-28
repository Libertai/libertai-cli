---
name: libertai-agent-ops
description: Manage everyday operational workflows, task decomposition, status reporting, follow-ups, and project coordination. Use for assistant-agent work outside a code repository.
metadata:
  libertai.pillars: agent
  libertai.bundle: builtin
---

# LibertAI Agent Ops

Use this skill for everyday management, coordination, planning, and operational workflows.

## Workflow

- Turn ambiguous requests into a short working plan, then execute the parts that available tools can safely complete.
- Track decisions, open questions, deadlines, owners, and follow-up actions explicitly.
- Prefer concise status reports over long narrative unless the user asks for detail.
- When a task has external side effects, state what action is about to happen and wait for explicit authorization if policy or tooling requires it.

## High-Risk Domains

For financial, legal, medical, or trading topics, provide analysis and risk framing only unless the user explicitly authorizes an action and available tools support that action safely.

For trading-related work, verify time-sensitive market data with current sources, state assumptions, and avoid presenting predictions as certainty.
