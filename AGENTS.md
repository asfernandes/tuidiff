# AGENTS.md

This file provides guidance to AI coding agents when working with code in this repository.

## What this is

`tuidiff` is a Rust (edition 2024) terminal side-by-side diff viewer/editor for two folders or two files, built on
ratatui/crossterm. It also serves as a `git difftool -d` backend. README.md documents the CLI, the editability rules and
all key bindings; keep it in sync when adding or changing shortcuts or modes.

## Commands

```
cargo build --release            # needs a C compiler (syntect uses bundled Oniguruma)
cargo build --release --target x86_64-unknown-linux-musl   # how releases are built; needs musl-gcc (musl-tools)
cargo test                       # all tests (unit tests live in #[cfg(test)] modules in each src file)
cargo test find_prompt_and_next  # a single test by name (substring match)
cargo test buffer::              # tests of one module
cargo fmt                        # rustfmt.toml sets max_width = 120 (CI runs `cargo fmt --check`)
cargo clippy --all-targets -- -D warnings   # what CI runs; lints are configured in Cargo.toml [lints]
cargo run -- <LEFT> <RIGHT>      # or `cargo run -- .` inside a git repo
```

## Architecture

Single binary, flat module layout in `src/`:

- `main.rs`: CLI (clap), terminal setup/teardown (mouse capture, bracketed paste, panic hook), and the event loop. The
  loop draws, flushes queued OSC 52 sequences (`app.osc_out`), handles the F12 mouse-capture toggle, keeps redrawing
  while `app.hl_pending` (incremental highlighting), and coalesces event bursts into one redraw.
- `app.rs`: all state (`App`) and input handling (`App::handle_event` for keys and mouse). One `FileView` per opened
  relative path is cached in `App::views`, so unsaved edits survive switching files. `FileView` holds the two `Buffer`s,
  their `HlCache`s, the `DiffResult` and the shared vertical scroll.
- `buffer.rs`: the editable text buffer for one side (lines, cursor/anchor selection, undo/redo snapshots, load/save,
  CRLF/trailing-newline/binary handling). Each content change bumps `version` and lowers `hl_invalid_from`.
- `diff.rs`: line alignment with `similar` (Patience, 2 s deadline) into display `Row`s (each side's line index or a
  filler) and `Hunk`s, plus word-level `inline_changes`.
- `highlight.rs`: syntect highlighting with a per-buffer cache of parser states per line, so highlighting is incremental
  and time-budgeted, and is invalidated from the first edited line.
- `ui.rs`: all rendering. Drawing also records screen regions in `app.areas` (`Areas`), which the mouse handling in
  `app.rs` hit-tests against, so layout changes in `ui.rs` must keep `Areas` accurate.
- `scan.rs` / `tree.rs`: recursive folder comparison (skips `.git`, lists only differing files) and the collapsible tree
  built from it.
- `gitdiff.rs`: single-path mode (`tuidiff .`). It shells out to `git`, writes changed HEAD files into a temp dir (left)
  and symlinks to the working tree (right), so the rest of the app just sees two folders. `tuidiff REV1..REV2 [PATH]` (`prepare_range`) does the same for two commits, writing both sides as real files. The `TempDir` guard is held in
  `main` until exit.
- `text.rs`: display-width helpers (tabs, wide chars, control-char stand-ins) shared by rendering and mouse hit-testing,
  plus base64 for OSC 52.

Key flow: edits mutate a `Buffer`, then `FileView::refresh()` recomputes the diff only when a buffer `version` changed
and pushes `hl_invalid_from` into the highlight cache. Positions are char indices into a line (`Pos.col`); convert to
display columns with `text::display_col` / `col_at_display`.

Editability (`App::editable`): `--readonly` and `/dev/null` sides are never editable. In git mode the left side is
always read-only. The right side is editable when it is a symlink (git's link into the working tree), when it is a real
file outside the temp dir, or with `--right-editable`. Git mode is auto-detected from `git-difftool.*` temp-dir paths.

Clipboard: copy writes to the internal clipboard, the system clipboard via `arboard`, and OSC 52 as a fallback. Paste
reads the system clipboard.

## Testing conventions

Tests build fixtures in `tempfile::tempdir()`. UI/integration tests in `ui.rs` drive an `App` through `handle_event`
with synthetic key and mouse events, then render to a ratatui `TestBackend` and assert on the screen text.

## GitHub Actions

In any workflow under `.github/workflows/`, pin each action to a full commit SHA, with the version in a trailing comment,
e.g. `uses: owner/action@<40-char-sha> # v1.2.3`. Use the same SHA for an action across all jobs and workflows. To add
or bump one, take the latest release tag and resolve it with `git ls-remote --tags https://github.com/<owner>/<repo>`.
For annotated tags, use the peeled `^{}` commit, not the tag object. If an action has no version tags and is used by
branch (e.g. `dtolnay/rust-toolchain@stable`), pin the branch tip (`git ls-remote <url> refs/heads/<branch>`) and put
the branch name in the comment.
