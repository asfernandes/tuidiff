# difftui

A terminal side-by-side diff viewer and editor for folders and files, usable as a `git difftool -d` backend.

- A folder tree on the left shows only files that differ, drawn with Unicode tree guides (`├─ └─ │`) and folder icons.
  - `●` means modified, `✚` only on the right, `✖` only on the left, and `✔` identical after your edits. `✎` marks unsaved changes.
- Two diff panes show the old and new versions with aligned lines, syntax highlighting (syntect), and highlighted changed words within a line.
- A built-in modeless editor provides undo/redo, selection and a clipboard. Edits recompute the diff live.
- You can copy a change from one side to the other with Meld-style `«` / `»` arrows or `Alt+←/→`.
- Full mouse support: clicks, drag-selection, double-click word selection, wheel scrolling, clickable change arrows and buttons, and a draggable tree splitter.

## Usage

```
difftui <LEFT> <RIGHT>          # two folders or two files
difftui .                       # inside a git repo: HEAD vs. working tree (like `git difftool -d HEAD`)
difftui --git <LOCAL> <REMOTE>  # git mode
difftui --list-themes
difftui --theme "Solarized (dark)" a b
```

### As git difftool

```
git config --global difftool.difftui.cmd 'difftui --git "$LOCAL" "$REMOTE"'
git config --global difftool.prompt false
git config --global diff.tool difftui        # optional: make it the default

git difftool -d               # folder diff of the working tree vs. the index/HEAD
git difftool -d HEAD~3        # working tree vs. a commit
git difftool -d main feature  # two commits (read-only)
git difftool                  # file-by-file mode also works
```

Which files can be edited:

| Situation | Left | Right |
|---|---|---|
| Plain folder/file diff | editable | editable |
| git, right side is the working tree | read-only | **editable**: saved straight into your working tree through git's symlinks |
| git, right side is a commit | read-only | read-only, because git would throw your edits away |

`--git` is auto-detected when the paths are inside a `git-difftool.*` temp dir. If you use `git difftool -d --no-symlinks`, pass `--right-editable`. Git copies modified working-tree files back when the tool exits.

## Keys

| Where | Key | Action |
|---|---|---|
| global | `Tab` / `Shift+Tab` / `F6` | switch focus between tree, left and right (in an editable pane, `Tab` indents) |
| global | `Ctrl+D` / `Ctrl+E` (or `Alt+↓` / `Alt+↑`) | next / previous change |
| global | `Alt+→` / `Alt+←` | copy the change under the cursor left→right / right→left |
| global | `Ctrl+N` / `Ctrl+P` | next / previous file |
| global | `Ctrl+S` / `F2` | save the current file pair / save all |
| global | `Ctrl+Z` / `Ctrl+Y` | undo / redo |
| global | `F5` | rescan folders |
| global | `F12` | toggle mouse capture (turn it off to use the terminal's own text selection) |
| global | `Ctrl+Q` | quit; asks first if anything is unsaved |
| tree | `↑↓` `PgUp/PgDn` `Home/End` | select (opens the file) |
| tree | `Enter` / `→` / `←` / `Space` | focus the diff or expand / expand / collapse or go to parent / fold |
| tree | `Ctrl+←/→` | resize the tree |
| tree | `q` / `Esc` | quit |
| pane | arrows, `Ctrl+←/→`, `Home/End`, `Ctrl+Home/End`, `PgUp/PgDn` | move (hold `Shift` to select) |
| pane | `Ctrl+↑/↓` | scroll without moving the cursor |
| pane | `Ctrl+A` / `Ctrl+C` / `Ctrl+X` / `Ctrl+V` | select all / copy / cut / paste. With no selection, copy and cut take the whole line. Copy also goes to the system clipboard (natively, plus OSC 52 as a fallback). Some terminals intercept `Ctrl+Shift+C`; use `Ctrl+C` |
| pane | `Ctrl+K` / `Ctrl+Backspace` / `Ctrl+Del` | delete line (or selected lines) / previous word / next word |
| pane | `Esc` | back to the tree |

Mouse: click a tree row to open a file or fold a folder. Click or drag in a pane to place the cursor or select, and double-click to select a word. The mouse wheel scrolls; hold `Shift` to scroll sideways. Click `«` / `»` to copy a change, drag the tree border to resize it, and use the status-bar buttons.

## Build

```
cargo build --release   # needs a C compiler (bundled Oniguruma regex engine for highlighting)
cargo test
```
