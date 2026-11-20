# Security Policy

Vox runs entirely on your machine — no cloud calls, no API keys, no telemetry —
so its attack surface is mostly local. That said, it does execute a local LLM's
tool calls (launching `claude` agents and reading Conductor's database), so we
take reports seriously.

## Reporting a vulnerability

**Please do not open a public issue for security problems.**

Instead, use GitHub's private vulnerability reporting:
**Security → Report a vulnerability** on this repository
(<https://github.com/justeozan/vox/security/advisories/new>).

Include:

- what you found and where (file / component),
- a proof of concept or reproduction steps,
- the impact you think it has.

We'll acknowledge within a few days and keep you updated on the fix. Once a fix
is released we're happy to credit you unless you'd prefer to stay anonymous.

## Scope notes

- Vox reads Conductor's SQLite DB **read‑only** and never writes to it.
- Voice‑launched agents run as `claude --print --dangerously-skip-permissions`
  in a worktree — this is intentional for a local dev tool, but it means a
  malicious prompt reaching the LLM can run code in your worktree. Reports about
  hardening this boundary are welcome.
- Supported version: the latest release on `main`.
