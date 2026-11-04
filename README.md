# lens

A fast terminal diff viewer and code review TUI, written in Rust.

[![GitHub Releases](https://img.shields.io/github/downloads/dghaehre/lens/total?label=dowloads%20%40releases)](https://github.com/jnsahaj/lens/releases)
![GitHub License](https://img.shields.io/github/license/dghaehre/lens)
![Crates.io Size](https://img.shields.io/crates/size/lens)

Review your working tree against any ref side-by-side without leaving your terminal. Ships as a single static Rust binary and stays snappy on multi-thousand-line diffs.

- Side-by-side diff viewer with tree-sitter syntax highlighting
- Annotate selections, hunks, or whole files
- Watch mode and stacked-commit review
- Works with Git and Jujutsu (jj) repositories

## Table of Contents
- [Getting Started](#getting-started-)
  - [Prerequisites](#prerequisites)
  - [Installation](#installation)
- [Usage](#usage-)
  - [Visual Diff Viewer](#visual-diff-viewer)
- [Advanced Configuration](#advanced-configuration-)
  - [Configuration File](#configuration-file)
  - [Configuration Precedence](#configuration-precedence)

## Getting Started 🔅

### Prerequisites
Before you begin, ensure you have:
1. `git` installed on your system (or `jj`, if you work in Jujutsu repositories)

### Installation

#### Using Cargo
> [!IMPORTANT]
> `cargo` is a package manager for `rust`,
> and is installed automatically when you install `rust`. Building Lens requires Rust 1.97.1 or newer.
> See [installation guide](https://doc.rust-lang.org/cargo/getting-started/installation.html)
```bash
cargo install lens
```

## Usage 🔅

### Visual Diff Viewer


```bash
# View uncommitted changes (worktree vs HEAD)
lens

# View worktree vs an earlier ref (commit, branch, tag, ...)
lens HEAD~3
lens main

# Filter to specific files
lens --file src/main.rs --file src/lib.rs

# Auto-refresh on file changes is enabled by default.
# Disable it with --no-watch
lens --no-watch

# Stacked mode - walk commits in <ref>..HEAD one by one
lens main --stacked

# jj: review the combined diff across a range
lens 'main..@'
lens --revset 'trunk()..@'

# Jump to a specific file on open
lens --focus src/main.rs
```

lens always diffs the working tree against the supplied ref. To review a
single historical commit, check it out (e.g. `git checkout <sha>`) and
diff against its parent (`lens HEAD~1`).

In a jj repository, bounded ranges such as `main..@` and `trunk()..@` show
one combined diff between the endpoint trees. They work as positional CLI
arguments, with `--revset`, and in the `b` picker with Enter. Current edits
are snapshotted before loading, so `main..@` includes the working copy;
a historical right endpoint uses that revision's contents. Edits reverted
within the range disappear from the combined diff. Each endpoint must resolve
to one revision. Use `lens main --stacked` for commit-by-commit review.
Other `--revset` expressions continue to review selected commits individually.

Lens loads effective revset aliases (including `trunk()`) using the installed
`jj config list` at startup, then parses and evaluates revsets with `jj-lib`.
Restart Lens after changing aliases.

The `b` picker shows an ancestry graph with branch and merge connections (`@` marks
the working change). Choose a listed change and press `enter` to review its own diff.
Type a highlighted change-ID prefix from `jj log` and press `enter` to review
that change. ID-shaped input takes precedence over fuzzy row matches and uses
jj’s `revsets.short-prefixes` scope (or `revsets.log` when unset); ambiguous
prefixes show an error. Use the full description for fuzzy text searches.
Choose the working-copy row to return to the live diff. Type any
revset and press `ctrl+enter` to review that expression. A revset with
no matching picker row enters that mode with regular `enter` as well.

#### Stacked Diff Mode

Review the commits between a base ref and `HEAD` one at a time with `--stacked`:

```bash
lens main --stacked
lens HEAD~5 --stacked
```

This displays each commit individually, letting you navigate through them:
- `ctrl+h` / `ctrl+l`: Previous / next commit
- Click the `‹` / `›` arrows in the header

The header shows the current commit position, SHA, and message. Viewed files are tracked per commit, so your progress is preserved when navigating.

#### Theme Configuration

Customize the diff viewer colors with preset themes:

```bash
# Using CLI flag
lens --theme dracula

# Using environment variable
LENS_THEME=catppuccin-mocha lens

# Or set permanently in config file (~/.config/lens/lens.config.json)
{
  "theme": "dracula"
}
```

**Available themes:**
| Theme | Value |
|-------|-------|
| Default (auto-detect) | `dark`, `light` |
| Catppuccin | `catppuccin-mocha`, `catppuccin-latte` |
| Dracula | `dracula` |
| Nord | `nord` |
| One Dark | `one-dark` |
| Gruvbox | `gruvbox-dark`, `gruvbox-light` |
| Solarized | `solarized-dark`, `solarized-light` |
| Thamos | `thamos` |

Priority: CLI flag > config file > `LENS_THEME` env var > OS auto-detect.

#### Selection & Annotations

**Selection**: Click-drag in the content area for character-level selection, or on line numbers for line-level selection. Selected text can be copied or annotated.

**Annotations**: Add review comments at three levels of granularity:
- **Selection** — select lines with mouse, press `i` to annotate the selected range
- **Hunk line/range** — focus a hunk with `[`/`]`, press `v`, move with `j/k`, press `space` to start a range, then press `i`
- **Hunk** — focus a hunk with `[`/`]`, press `i` to annotate the hunk
- **File** — press `i` with no selection or hunk focus to annotate the whole file

Annotated lines display a `▍` gutter indicator. Use `I` to view, edit, delete, copy, or export all annotations.

#### Keybindings

- `j/k` or arrow keys: Navigate
- `[`/`]`: Jump between hunks
- `v`: Select a line inside the focused hunk
- `tab`: Toggle sidebar
- `space`: Mark file as viewed
- `e`: Open file in editor
- `y`: Copy selection (or filename)
- `i`: Annotate selection / hunk / file
- `I`: View all annotations
- `ctrl+h/l`: Previous/next commit (stacked mode)
- `?`: Show all keybindings

### Agent Skill

`lens` ships a [SKILL.md](./SKILL.md) that teaches coding agents how to read, reply to, and continuously watch for annotations via the `lens annotation` subcommands. The watch loop uses `lens annotation watch`, an fs-notify-backed blocking subcommand that returns when an unresolved thread has a non-agent latest comment. Install the skill once per machine:

```bash
# Claude Code → ~/.claude/skills/lens-annotations/SKILL.md
lens skill install claude

# Codex → ~/.codex/skills/lens-annotations/SKILL.md
lens skill install codex
```

Both installs overwrite in place on every run, so re-run them whenever `SKILL.md` changes.

## Advanced Configuration 🔅

### Configuration File
Lens supports configuration through a JSON file. You can place the configuration file in one of the following locations:

1. Project Root: Create a lens.config.json file in your project's root directory.
2. Custom Path: Specify a custom path using the --config CLI option.
3. Global Configuration (Optional): Place a lens.config.json file in your system's default configuration directory:
    - Linux/macOS: `~/.config/lens/lens.config.json`
    - Windows: `%USERPROFILE%\.config\lens\lens.config.json`

Lens will load configurations in the following order of priority:

1. CLI arguments (highest priority)
2. Configuration file specified by --config
3. Project root lens.config.json
4. Global configuration file (lowest priority)

```json
{
  "theme": "catppuccin-mocha"
}
```

### Configuration Precedence
Options are applied in the following order (highest to lowest priority):
1. CLI Flags
2. Configuration File
3. Environment Variables
4. Default options
## Contributors

<a href="https://github.com/dghaehre/lens/graphs/contributors">
  <img src="https://contrib.rocks/image?repo=dghaehre/lens" />
</a>

Made with [contrib.rocks](https://contrib.rocks).

### Interested in Contributing?

Contributions are welcome! Please feel free to submit a Pull Request.

# Star History

<p align="center">
  <a target="_blank" href="https://star-history.com/#dghaehre/lens&Date">
    <picture>
      <source media="(prefers-color-scheme: dark)" srcset="https://api.star-history.com/svg?repos=jnsahaj/lens&type=Date&theme=dark">
      <img alt="GitHub Star History for jnsahaj/lens" src="https://api.star-history.com/svg?repos=jnsahaj/lens&type=Date">
    </picture>
  </a>
</p>
