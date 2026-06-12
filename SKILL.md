---
name: lens-annotations
description: Read, reply to, and continuously watch for human review comments left in the working tree via `lens annotation`. Default behavior is a watch loop that blocks until a new unresolved annotation appears, addresses it in code, and replies on the thread. Also use ad-hoc whenever the user mentions an "annotation", "review note", "comment", or "lens", and before declaring work complete in a lens repo.
---

# Lens annotations

`lens` is a terminal code-review tool. Humans leave review comments on the working tree (file-level or line-range, on either side of the diff) and persist them in a per-repo SQLite store at `.git/lens.db`. The `lens annotation` subcommand exposes that store to agents over JSON on stdout.

Treat unresolved annotations the same way you would treat unresolved PR review comments: read them, address the underlying request in code, and reply on the thread so the human knows what you did.

## When to use this skill

Use it without being asked when any of these are true:

- The user asks you to "watch annotations", "watch for review comments", or anything similar — enter the loop described below and stay in it.
- You just received a task in a repo that uses lens — list annotations before writing any code so you don't undo recent feedback.
- You are about to declare a piece of work complete — list annotations again to catch any new ones left while you were working.
- The user mentions an "annotation", a "comment", a "review note", or "lens" — go straight to `lens annotation list`.
- You did a `git pull`, switched branches, or rebased — annotations are stored per-repo, so new ones may have appeared.

Do not bring it up in repos that do not use lens. Check once with `lens annotation list` — if the command fails because lens is not installed, or returns `[]` and the repo shows no other sign of using lens, drop it for the session.

## Loop mode: watch for annotation activity

Run as a continuous loop: block on `lens annotation watch`, address whatever it returns, then block again. The watch command only returns when at least one thread needs your attention (unresolved AND latest comment is not from `agent`), so the loop body never wakes up with nothing to do. The loop exits only when the user explicitly tells you to stop ("stop watching", "done", "stop the loop", etc.).

```bash
lens annotation watch
```

Run it via the Bash tool with `run_in_background: true` and `timeout: 600000`. It uses fs-notify on `.git/lens.db` (no polling), and on exit prints the actionable threads as JSON on stdout in the same shape as `lens annotation list`. The harness notifies you when it returns. Do not poll, sleep, or do other work in the meantime. On a 10-minute timeout, just restart it.

If actionable threads already exist when you launch `watch`, it returns immediately with them — so the first iteration handles any backlog from before you started the loop.

### Each iteration

1. **Block.** `lens annotation watch` — returns when at least one thread needs attention.
2. **Address each returned thread** following the per-annotation workflow below. Handle items one at a time and watch for conflicts.
3. **Loop.** Return to step 1.
4. **Exit the loop only when the user says so.** When you exit, briefly confirm.

If `lens annotation watch` exits with no JSON (or a parse error / "not a git repo" / lens not installed), drop the loop for the session — there's nothing to watch.

## Per-annotation workflow

For each actionable annotation:

1. **Read first.** `lens annotation list --id <id>` — read the whole thread end-to-end before changing code in that area.
2. **Address in code.** Make the smallest correct change the comment asks for. If you disagree, do not silently ignore — reply explaining your reasoning and ask the user how to proceed.
3. **Reply on the thread.** Use `lens annotation reply --id <id> "<message>"` with a short, factual note pointing to what you did (a file:line, a one-line summary). One reply per addressed annotation; do not spam. **Do not resolve the thread** — the user reviews your reply and resolves it themselves.
4. **Do not stage your implementation.** Leave the edits unstaged so the user can review the diff before staging.

## Commands

All commands run from inside the repo. Output is pretty-printed JSON on stdout; parse it, don't pattern-match the formatting.

```bash
# List every annotation in the repo.
lens annotation list

# Filter to one or more files (repeat the flag).
lens annotation list --file src/main.rs --file src/lib.rs

# Fetch a single annotation by id (useful after a reply to see the updated thread).
lens annotation list --id 42

# Block until a thread needs the agent's attention, then print those threads as
# JSON (same shape as `list`) and exit. Used as the loop's blocking primitive.
lens annotation watch

# Reply to an annotation. Use this to acknowledge, ask a clarifying question,
# or report what you changed. Replies are tagged `agent` by default.
lens annotation reply --id 42 "Fixed in <commit-sha>: switched to checked arithmetic."

# Leave a new annotation. Prefer replying over creating; only create when there
# is genuinely no existing thread to attach to.
lens annotation new --file src/main.rs --line 42 --line-end 48 "Consider extracting this branch."

# Mark a thread as resolved. The loop workflow above does NOT resolve threads —
# the user does that after reviewing your reply. Only resolve outside the loop
# when the user has explicitly asked you to.
lens annotation resolve --id 42
lens annotation resolve --id 42 --unresolve

# Delete an annotation you created in error. Do not delete annotations
# authored by the user.
lens annotation delete --id 42
```

### JSON shape

`lens annotation list` returns an array of:

```json
{
  "id": 42,
  "origin": "ui",
  "filename": "src/main.rs",
  "target": { "kind": "line_range", "panel": "new", "start_line": 42, "end_line": 48 },
  "resolved": false,
  "created_at": 1716800000,
  "comments": [
    { "id": 1, "author": "you", "origin": "ui", "content": "...", "created_at": 1716800000 },
    { "id": 2, "author": "agent", "origin": "agent", "content": "...", "created_at": 1716800100 }
  ]
}
```

- `comments[0]` is the original comment; later entries are replies in order.
- `origin` of `ui` means the human wrote it in the TUI; `agent` / `cli` mean a programmatic caller did. Treat `ui` comments as authoritative requests from the user.
- `target.kind` is `file` for whole-file notes; otherwise `line_range` with 1-based `start_line` / `end_line` on the `old` or `new` side of the diff.

## Reply style

Write replies the same way you would write a PR review reply: terse, factual, no filler.

Good:
- `Done — replaced the unwrap with `?` at src/parser.rs:88.`
- `Skipped: the loop only runs once in practice (guarded by `is_root` above). Want me to add a debug_assert instead?`

Avoid:
- `Thanks for the feedback! I have carefully considered your suggestion and ...`
- Multi-paragraph explanations of what the original comment said.
- Emojis unless the user uses them first.

## Guardrails

- Do not create annotations describing your own work as a substitute for a reply or commit message. Annotations are for review notes, not status updates.
- Do not delete annotations authored by the user (`origin: "ui"`). If a thread feels stale, ask before deleting.
- Do not mark something "addressed" in a reply unless you actually changed the code (or have a concrete reason for declining).
- Do not resolve threads in loop mode — the user resolves after reviewing your reply.
- Do not stage your implementation in loop mode — leave it unstaged for the user to review.
- Do not parse the human-formatted output of the TUI; always go through `lens annotation` subcommands for machine reads.
