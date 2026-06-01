---
name: libertai-code-workflow
description: Inspect repositories, plan code edits, make narrow changes, run focused verification, and report changed files. Use for software engineering tasks.
metadata:
  libertai.pillars: code
  libertai.bundle: builtin
---

# LibertAI Code Workflow

Use this skill for software engineering work in a repository.

## Workflow

- Inspect the repository before proposing or applying edits.
- Prefer narrow, reviewable changes that match the existing code style.
- Use todo updates for multi-step code tasks.
- In plan mode, read and reason only; describe the intended edit path instead of mutating files.
- Before finishing, report changed files and the verification you ran or could not run.
- Treat generated diffs, tool previews, and test output as the source of truth over assumptions.
