---
name: lens-annotations
description: Read, reply to, and resolve human review comments left in the working tree via `lens annotation`. Use whenever you are developing or reviewing code changes — before declaring work complete, after `git pull`/branch switches, and whenever the user mentions an annotation, comment, review note, or "lens".
---

# Lens annotations

`lens` is a terminal code-review tool. Humans leave review comments on the working tree (file-level or line-range, on either side of the diff) and persist them in a per-repo SQLite store at `.git/lens.db`. The `lens annotation` subcommand exposes that store to agents over JSON on stdout.

Treat unresolved annotations the same way you would treat unresolved PR review comments: read them, address the underlying request in code, and reply on the thread so the human knows what you did.

## When to use this skill

Use it without being asked when any of these are true:

- You just received a task in a repo that uses lens — list annotations before writing any code so you don't undo recent feedback.
- You are about to declare a piece of work complete — list annotations again to catch any new ones left while you were working.
- The user mentions an "annotation", a "comment", a "review note", or "lens" — go straight to `lens annotation list`.
- You did a `git pull`, switched branches, or rebased — annotations are stored per-repo, so new ones may have appeared.

Do not bring it up in repos that do not use lens. Check once with `lens annotation list` — if the command fails because lens is not installed, or returns `[]` and the repo shows no other sign of using lens, drop it for the session.

## Commands

All commands run from inside the repo. Output is pretty-printed JSON on stdout; parse it, don't pattern-match the formatting.

```bash
# List every annotation in the repo.
lens annotation list

# Filter to one or more files (repeat the flag).
lens annotation list --file src/main.rs --file src/lib.rs

# Fetch a single annotation by id (useful after a reply to see the updated thread).
lens annotation list --id 42

# Reply to an annotation. Use this to acknowledge, ask a clarifying question,
# or report what you changed. Replies are tagged `agent` by default.
lens annotation reply --id 42 "Fixed in <commit-sha>: switched to checked arithmetic."

# Leave a new annotation. Prefer replying over creating; only create when there
# is genuinely no existing thread to attach to.
lens annotation new --file src/main.rs --line 42 --line-end 48 "Consider extracting this branch."

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

## Workflow

1. **Read first.** Run `lens annotation list` (optionally `--file` filtered to the files you are about to touch). Read every unresolved thread end-to-end before changing code in that area.
2. **Address in code.** Make the change the comment asks for. If you disagree, do not silently ignore — reply explaining your reasoning and ask the user how to proceed.
3. **Reply on the thread.** After making the change, reply with a short note pointing to what you did (a commit sha, a file:line, or a one-line summary). One reply per addressed annotation; do not spam.
4. **Re-list before finishing.** Run `lens annotation list` once more before reporting the task complete, to make sure nothing landed mid-task and nothing was missed.

## Reply style

Write replies the same way you would write a PR review reply: terse, factual, no filler.

Good:
- `Done — replaced the unwrap with `?` at src/parser.rs:88.`
- `Skipped: the loop only runs once in practice (guarded by `is_root` above). Want me to add a debug_assert instead?`

Avoid:
- `Thanks for the feedback! I have carefully considered your suggestion and ...`
- Multi-paragraph explanations of what the original comment said.
- Emojis unless the user uses them first.

## Things to avoid

- Do not create annotations describing your own work as a substitute for a reply or commit message. Annotations are for review notes, not status updates.
- Do not delete annotations authored by the user (`origin: "ui"`). If a thread feels stale, ask before deleting.
- Do not mark something "addressed" in a reply unless you actually changed the code (or have a concrete reason for declining).
- Do not parse the human-formatted output of the TUI; always go through `lens annotation` subcommands for machine reads.
