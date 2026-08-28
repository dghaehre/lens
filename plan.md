# Plan: remove per-frame O(file-size) work from the diff render path

Status: proposed, not implemented. Working tree is clean as of writing.

## 1. Background — why this exists

Scrolling felt laggy. I instrumented the release binary (temporary timers around
`terminal.draw`, `render_diff`, and each helper inside it) and drove the real
binary through a pty at 200x50 against two synthetic repos: a 3.6k-line diff and
a 54k-line diff. The instrumentation has since been reverted.

Median per-frame timings, in microseconds:

| stage | 54k-line diff | 3.6k-line diff | scales with |
|---|---|---|---|
| `compute_context_lines` x2 | **~2400** | ~200 | file size |
| `compute_line_stats` | **~300-900** | ~40 | file size |
| widget render (`Paragraph`) | ~1000 | ~1000 | screen |
| visible-line span loop | ~400 | ~400 | screen |
| terminal flush | ~1500 | ~1500 | screen |
| **draw total** | **~5.8 ms** | ~3.2 ms | |

Only ~50 lines are ever visible. The two rows that scale with file size are pure
waste — they are the target of this plan. Everything else is screen-bound and
already near the floor for a full-screen repaint.

I confirmed the attribution by prototyping both fixes (throwaway memoization):
`compute_context_lines` went 2400us -> **85us**, `compute_line_stats` -> **0us**,
and `draw_total` dropped from **5.8 ms -> 3.6 ms** on the 54k-line diff.

### What this plan does NOT fix

The residual ~3 ms is screen-bound: building spans for visible lines, ratatui's
buffer fill, and the flush. Scrolling by one row invalidates every cell, so each
frame is a full-screen repaint of roughly **17-26 KB of escape sequences**. Under
tmux that byte volume costs meaningfully more, because tmux parses it into its own
virtual screen and re-emits it. Reducing it would require terminal scroll regions,
which ratatui does not expose. Out of scope here.

Also out of scope, noted while measuring: the scroll coalescing at
`src/command/diff/app.rs:1961-1988` is dead code. The main loop already drains
`app_rx` into `pending_events` at `app.rs:1328`, so `app_rx.try_recv()` inside the
handler always returns empty (measured: `events n=1` for all 120 events in a paced
wheel run). It costs no extra frames — a true 20-event burst still produces exactly
1 frame, because the outer loop batches — so it is misleading, not slow. Delete or
fix it separately.

---

## 2. Fix 1 — stop hashing the whole file every frame (the big one)

### The problem

`src/command/diff/context.rs:262`

```rust
fn hash_source(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}
```

`compute_context_lines` (`context.rs:329`) caches its tree-sitter parse in a
thread-local `CONTEXT_CACHE` (`context.rs:249-260`, 8-entry LRU keyed by
`filename` + `source_len` + `source_hash`). The parse is correctly cached. But the
cache *key* is a SipHash over the entire file content, recomputed on every call —
and `render_diff` calls it **twice per frame**, once for `old_content` and once for
`new_content` (`render/diff_view.rs:2096` and `:2103` in the side-by-side path;
also `:1601` new-file path and `:1846` deleted-file path).

On a 1.6 MB file that is 3.2 MB of SipHash per frame, purely to decide the cache
is still valid.

### Recommended approach: hoist the parse cache into `AppState` (Option D)

This is the same pattern the codebase already uses for the other expensive
tree-sitter work — `cached_highlighters` in `AppState` (`state.rs:267`,
`get_highlighters` at `state.rs:791`). Key by `current_file` index, invalidate
explicitly on reload. Content hashing disappears entirely rather than getting
cheaper.

Why keying by file index is safe here, and is the established pattern:

- `current_file` is only assigned in `select_file` (`state.rs:1304`) and in
  `reload`'s clamping (`state.rs:1278`, `:1285`).
- Both paths call `invalidate_cache()` (`state.rs:1291`, `state.rs:1306`), which
  clears `cached_side_by_side` / `cached_hunks` / `cached_total_lines` /
  `cached_highlighters` (`state.rs:816`).
- So content-change detection is already handled at the `AppState` level. The
  context cache's content hash is a **second, redundant** safety net paid for on
  every frame.

Steps:

1. In `src/command/diff/context.rs`:
   - Make `ContextNode` (`context.rs:242`) `pub` and export it.
   - Add `pub fn collect_context_nodes_pub(source, filename, tab_width) -> Vec<ContextNode>`
     (or just make the existing `collect_context_nodes` at `context.rs:270` public).
   - Add `pub fn context_lines_from_nodes(nodes: &[ContextNode], scroll_position, config, max_lines) -> Vec<ContextLine>`
     wrapping the existing `filter_to_lines` (`context.rs:309`), preserving the
     `!config.enabled || scroll_position == 0` early-outs currently at
     `context.rs:336-338`.
   - Delete `hash_source`, `CacheEntry`, `CONTEXT_CACHE`, `CACHE_CAPACITY`, and the
     lookup/insert blocks in `compute_context_lines` (`context.rs:344-377`).
   - Keep `compute_context_lines` as a thin uncached wrapper *only if* the tests
     need it (see step 5); otherwise delete it.

2. In `src/command/diff/state.rs`:
   - Add a field mirroring `cached_highlighters`:
     ```rust
     cached_context_nodes: Option<(usize, Vec<ContextNode>, Vec<ContextNode>)>, // (file, old, new)
     ```
     next to `cached_highlighters` (`state.rs:267`), init `None` (`state.rs:406`).
   - Add `pub fn get_context_nodes(&mut self) -> (&[ContextNode], &[ContextNode])`
     modelled directly on `get_highlighters` (`state.rs:791-805`): recompute when
     the cached file index differs from `current_file`.
   - Add `pub fn context_nodes_ref(&self) -> Option<(&[ContextNode], &[ContextNode])>`
     mirroring `highlighters_ref` (`state.rs:809`).
   - Clear the new field in `invalidate_cache` (`state.rs:816-827`).

3. In `src/command/diff/app.rs`:
   - Call `state.get_context_nodes();` alongside the existing
     `state.get_highlighters();` (currently `app.rs:999`), then bind
     `let (old_ctx, new_ctx) = state.context_nodes_ref().unwrap();` next to the
     existing `highlighters_ref()` bind (`app.rs:1003`).
   - Pass both into `render_diff`.

4. In `src/command/diff/render/diff_view.rs`:
   - Add two params to `render_diff` (signature at `:1422-1464`):
     `old_context_nodes: &[ContextNode], new_context_nodes: &[ContextNode]`.
     Put them next to `old_highlighter` / `new_highlighter` for symmetry.
   - Replace the four `compute_context_lines(...)` call sites (`:1601`, `:1846`,
     `:2096`, `:2103`) with `context_lines_from_nodes(...)`.

5. Tests: six tests in `context.rs` call `compute_context_lines` directly
   (`context.rs:387, 395, 408, 424, 434, 445`). Either keep `compute_context_lines`
   as an uncached wrapper so they compile unchanged, or rewrite them against
   `collect_context_nodes` + `context_lines_from_nodes`. Keeping the wrapper is
   less churn and it becomes a genuinely cheap function once the hashing is gone.

### Behavioural change to accept

The thread-local cache held **8** recently-parsed sources; the `AppState` cache
holds only the current file. Switching files and switching back will re-parse.
This already happens for `cached_highlighters`, which is the more expensive of the
two, so it is consistent and the cost is paid on a file switch (where a pause is
expected) rather than on every scroll row.

If that regression is unwanted, make `cached_context_nodes` a small LRU keyed by
file index instead of a single slot. Prefer starting simple.

### Smaller-diff alternative (Option A) if you want to keep `context.rs` self-contained

Compute the hash once at load and carry it on the data:

- Add `pub old_hash: u64, pub new_hash: u64` to `FileDiff` (`types.rs:41-47`).
- Populate in the two loader construction sites (`loader.rs:109`, `loader.rs:166`)
  and the two test helpers (`state.rs:1541`, `state.rs:1551`).
- Change `compute_context_lines` to take `source_id: u64` and use
  `(filename, source_len, source_id)` as the key — no per-frame hashing.

Correct and much smaller, but it pushes a caching concern into a plain data type
and keeps a redundant second invalidation mechanism. Option D is preferred.

### Rejected

Memoizing `hash_source` by `(ptr, len)` — this is what I prototyped to get the
measurement, and it is **not** safe to ship: a reallocation landing at the same
address with the same length would serve a stale parse (ABA). Fine for a
throwaway measurement, not for main.

---

## 3. Fix 2 — cache the per-file line stats

### The problem

`src/command/diff/render/diff_view.rs:596` and its single caller at `:1576`:

```rust
let line_stats = compute_line_stats(side_by_side);
```

This walks the entire `side_by_side` vector every frame to produce two integers
for the footer (`diff_view.rs:2783-2784`). The vector is already cached in
`AppState` and is immutable between file switches, so the scan is repeated
identically every frame. ~300-900us per frame on the 54k-line diff.

Note this is **distinct** from `total_added` / `total_removed` (`state.rs:289-291`,
passed at `app.rs:1071-1072`), which are repo-wide totals across all files, used by
the sidebar. Fix 2 needs a new per-file value; do not try to reuse those.

### Approach

Compute it exactly where the other derived-from-`side_by_side` values are computed.

1. `src/command/diff/render/diff_view.rs`:
   - Add `#[derive(Clone, Copy)]` to `LineStats` (`:142`) so it can be stored and
     passed by value.
   - Keep `compute_line_stats` public (it becomes the one-shot used by `AppState`).

2. `src/command/diff/state.rs`:
   - Add `cached_line_stats: Option<(usize, LineStats)>` next to
     `cached_total_lines` (`state.rs:265`), init `None` (`state.rs:405`).
   - Populate it in the cache-fill block alongside `cached_total_lines`, and add
     `pub fn line_stats(&self) -> LineStats`.
   - **Both** cache-fill sites must be updated — the logic is currently duplicated
     between `get_side_by_side` (`state.rs:681-707`) and `ensure_cache`
     (`state.rs:718-740`). Worth collapsing one into the other while you are in
     there; if you do not, keep them in sync or the new field goes stale.
   - Clear it in `invalidate_cache` (`state.rs:816-827`).

3. `src/command/diff/app.rs` / `render_diff`:
   - Pass the cached `LineStats` into `render_diff` as a param and delete the
     `compute_line_stats` call at `diff_view.rs:1576`, feeding `line_stats.added` /
     `.removed` through to `FooterData` as today.

`compute_line_stats` then has no per-frame caller.

---

## 4. Expected result

On the 54k-line diff, `draw_total` should go from ~5.8 ms to ~3.6 ms, and
`render_diff` should become **O(visible rows)** rather than O(file lines) — i.e.
scroll cost stops growing with file size, which is the actual complaint. Small
diffs improve slightly; the win scales with file size.

---

## 5. Verification

### Correctness

- `cargo test` — 138 tests currently. Watch the six `context.rs` tests noted above.
- `cargo clippy` if it is part of the normal loop here.
- Manual: open a diff, scroll into a nested scope, confirm the sticky context
  header still shows the right enclosing scopes; confirm footer +/- counts still
  match; switch files back and forth and confirm context + highlighting are correct
  for each.
- **Watch-mode regression is the one to be careful about.** With the content hash
  gone, staleness is caught only by `invalidate_cache()` on reload. Run with watch
  enabled (default), edit the file under view from another shell, and confirm the
  context header and the diff both update. Specifically test an edit that keeps the
  file the same byte length, since that is the case the old `source_len` check
  alone would have missed.

### Performance

To reproduce the measurement, re-add temporary instrumentation:

- In `app.rs`, gate on an env var (`LENS_PERF=<path>`), time the whole
  `terminal.draw(...)` call and, separately, `render_diff` inside the closure;
  append a line per frame.
- For per-helper attribution, a `static [AtomicU64; N]` in `diff_view.rs` plus a
  small `perf!(slot, expr)` macro around the call sites, drained and printed once
  per frame.

Drive it headlessly with a pty so the numbers are not polluted by a real terminal:
`pty.fork()`, `TIOCSWINSZ` to 200x50, write `b"j"` (or SGR wheel
`b"\x1b[<65;100;20M"`) on a delay, drain and count output bytes, `q` to quit.

Test fixtures: two git repos with a single large modified file — ~3.6k and ~54k
side-by-side lines — so you can confirm the *slope* against file size flattened,
not just that one number got smaller. That slope is the real acceptance criterion.

Revert all instrumentation before committing.

---

## 6. Suggested sequencing

1. Fix 2 first — small, self-contained, no behavioural risk, and it exercises the
   `AppState` cache plumbing that Fix 1 also needs.
2. Optionally collapse the duplicated `get_side_by_side` / `ensure_cache` fill
   logic as its own commit.
3. Fix 1.
4. Separately: delete or repair the dead scroll-coalescing block in `app.rs`.
