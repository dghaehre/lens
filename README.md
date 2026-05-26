# lens

A fast terminal diff viewer and code review TUI, written in Rust.

[![GitHub Releases](https://img.shields.io/github/downloads/dghaehre/lens/total?label=dowloads%20%40releases)](https://github.com/jnsahaj/lens/releases)
![GitHub License](https://img.shields.io/github/license/dghaehre/lens)
![Crates.io Size](https://img.shields.io/crates/size/lens)

Review `git diff`, commits, branches, or GitHub PRs side-by-side without leaving your terminal. Ships as a single static Rust binary and stays snappy on multi-thousand-line diffs.

- Side-by-side diff viewer with tree-sitter syntax highlighting
- Review GitHub Pull Requests with `lens diff --pr 123`
- Annotate selections, hunks, or whole files
- Watch mode and stacked-commit review
- Works with Git repositories

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
1. `git` installed on your system

### Installation

#### Using Cargo
> [!IMPORTANT]
> `cargo` is a package manager for `rust`,
> and is installed automatically when you install `rust`.
> See [installation guide](https://doc.rust-lang.org/cargo/getting-started/installation.html)
```bash
cargo install lens
```

## Usage 🔅

### Visual Diff Viewer


```bash
# View uncommitted changes
lens diff

# View changes for a specific commit
lens diff HEAD~1

# View changes between branches
lens diff main..feature/A

# View changes in a GitHub Pull Request
lens diff --pr 123 # (--pr is optional)
lens diff https://github.com/owner/repo/pull/123

# Filter to specific files
lens diff --file src/main.rs --file src/lib.rs

# Watch mode - auto-refresh on file changes
lens diff --watch

# Stacked mode - review commits one by one
lens diff main..feature --stacked

# Jump to a specific file on open
lens diff --focus src/main.rs
```

#### Stacked Diff Mode

Review a range of commits one at a time with `--stacked`:

```bash
lens diff main..feature --stacked
lens diff HEAD~5..HEAD --stacked
```

This displays each commit individually, letting you navigate through them:
- `ctrl+h` / `ctrl+l`: Previous / next commit
- Click the `‹` / `›` arrows in the header

The header shows the current commit position, SHA, and message. Viewed files are tracked per commit, so your progress is preserved when navigating.

When viewing a PR, you can mark files as viewed (syncs with GitHub) using the `space` keybinding.

#### Theme Configuration

Customize the diff viewer colors with preset themes:

```bash
# Using CLI flag
lens diff --theme dracula

# Using environment variable
LENS_THEME=catppuccin-mocha lens diff

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
