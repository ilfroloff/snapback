# snapback vs. Claude Code's session picker

Claude Code's native `/resume` picker is great for picking a session. It is not
a board for running many agents at once. Here's the honest difference.

| | session picker | snapback |
| --- | --- | --- |
| `-p` / SDK / `/loop` sessions | hidden ([docs](https://code.claude.com/docs/en/sessions)) | shown (`--everything`) |
| Running background agents | one at a time | all at once, live |
| Search | session names | full transcripts |
| Act | resume only | reply, stop, start without leaving |
| Scope | one project | every repo + worktree (`-a`) |
| Runtime | built-in | one Rust binary, no deps |

## When the picker is enough

One project, one session at a time, and you remember roughly what you called it.
The native picker is fine — snapback won't change your life.

## When snapback earns its place

- You run several background agents across repos and worktrees in parallel.
- You want to find a session by what you said, not what you named it.
- You need to reply to or stop an agent without leaving your workflow.
- You want to clean up `-p`, SDK and `/loop` runs the picker never shows.

## The one-line version

The picker finds a session. snapback runs your session landscape.
