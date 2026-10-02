## Agent skills

### Issue tracker

GitHub Issues is the work-item system for this repository. See `docs/agents/issue-tracker.md`.

### Triage labels

Use the configured needs-triage, needs-info, ready-for-agent, ready-for-human, and wontfix labels. See `docs/agents/triage-labels.md`.

### Domain docs

Use the single root `CONTEXT.md`; architecture decisions live in `docs/adr/`. See `docs/agents/domain.md`.

## Non-code artifacts

Issues, PR descriptions, specs, plans, reviews, and every other non-code artifact give readers the
context and judgment the diff cannot, not a narrated diff or filler, and are published in full on
GitHub. The full rules:

@docs/non-code-rules.md

## PR rules

- Merge a PR only when I explicitly ask; squash-merge unless I say otherwise.
- When reviewing a PR, post everything (findings, spec and standards checks, assessment, observations, verification, summary) as one comment on the PR.
- After a PR is merged, clean up local branches and worktrees, fast-forward main, then update and close related issues.

## Git conventions

Never include AI attribution in commit messages, PR titles, or PR descriptions, in any form: no
`Co-Authored-By: Claude`, `Generated with ...` footers, sign-offs naming an AI agent or vendor
(Claude, Anthropic, GPT, OpenAI, …), or `Claude-Session:` trailers and session URLs — even when a
tool inserts them automatically. When squash-merging, write a clean commit message that describes
only the change itself.