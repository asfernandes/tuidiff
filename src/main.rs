mod app;
mod buffer;
mod diff;
mod gitdiff;
mod highlight;
mod scan;
mod text;
mod tree;
mod ui;

use std::io::{Write, stdout};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use ratatui::crossterm::execute;

use app::{App, Options};

/// Side-by-side folder/file diff viewer and editor for the terminal.
///
/// Use as git difftool:
///   git config --global difftool.tuidiff.cmd 'tuidiff --git "$LOCAL" "$REMOTE"'
///   git difftool -d -t tuidiff
#[derive(Parser)]
#[command(version, verbatim_doc_comment)]
struct Cli {
    /// Left (old) folder or file; or, when given alone, a folder inside a git
    /// repository to compare against HEAD (like `git difftool -d HEAD`); or a
    /// commit range `REV1..REV2` / `REV1...REV2` (RIGHT then optionally limits
    /// the diff to a folder, default `.`)
    #[arg(required_unless_present = "list_themes")]
    left: Option<PathBuf>,
    /// Right (new) folder or file; with a commit range, the folder to limit it to
    right: Option<PathBuf>,
    /// git difftool mode: left is read-only, right is editable only for
    /// working-tree files (auto-detected for `git difftool -d` temp dirs)
    #[arg(long)]
    git: bool,
    /// In git mode, allow editing all right-side files (e.g. with --no-symlinks)
    #[arg(long)]
    right_editable: bool,
    /// Open everything read-only
    #[arg(long)]
    readonly: bool,
    /// Syntax highlighting theme
    #[arg(long, default_value = "base16-eighties.dark")]
    theme: String,
    /// List available themes and exit
    #[arg(long)]
    list_themes: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if cli.list_themes {
        for t in highlight::Highlighter::theme_names() {
            println!("{t}");
        }
        return Ok(());
    }
    let left = cli.left.unwrap();
    // Keep the guard alive until exit so the temp dirs are removed.
    let range = left.to_str().filter(|_| !left.exists()).and_then(gitdiff::split_range);
    let (left, right, git_head, _guard) = match (range, cli.right) {
        (Some((l, r, three)), dir) => {
            let d = gitdiff::prepare_range(dir.as_deref().unwrap_or(Path::new(".")), l, r, three)?;
            (d.left.clone(), d.right.clone(), true, Some(d))
        }
        (None, Some(right)) => (left, right, false, None),
        (None, None) => {
            let d = gitdiff::prepare(&left)?;
            (d.left.clone(), d.right.clone(), true, Some(d))
        }
    };
    let opts = Options {
        git: cli.git || git_head,
        right_editable: cli.right_editable,
        readonly: cli.readonly,
        theme: cli.theme,
    };
    let mut app = App::new(left, right, opts)?;

    let mut terminal = ratatui::init();
    execute!(stdout(), EnableMouseCapture, EnableBracketedPaste)?;
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(stdout(), DisableMouseCapture, DisableBracketedPaste);
        prev_hook(info);
    }));
    let res = run(&mut terminal, &mut app);
    let _ = execute!(stdout(), DisableMouseCapture, DisableBracketedPaste);
    ratatui::restore();
    res
}

fn run(terminal: &mut DefaultTerminal, app: &mut App) -> Result<()> {
    while !app.quit {
        terminal.draw(|f| ui::draw(f, app))?;
        if !app.osc_out.is_empty() {
            let mut out = stdout();
            for s in app.osc_out.drain(..) {
                out.write_all(s.as_bytes())?;
            }
            out.flush()?;
        }
        if app.mouse_toggle_requested {
            app.mouse_toggle_requested = false;
            app.mouse_enabled = !app.mouse_enabled;
            if app.mouse_enabled {
                execute!(stdout(), EnableMouseCapture)?;
                app.set_message("Mouse capture on", false);
            } else {
                execute!(stdout(), DisableMouseCapture)?;
                app.set_message(
                    "Mouse capture off (terminal selection enabled) — F12 to re-enable",
                    false,
                );
            }
            continue;
        }
        if app.hl_pending && !event::poll(Duration::from_millis(1))? {
            continue; // keep highlighting while idle
        }
        app.handle_event(event::read()?);
        // Coalesce bursts (e.g. wheel scrolling) into a single redraw.
        while !app.quit && event::poll(Duration::ZERO)? {
            app.handle_event(event::read()?);
        }
    }
    Ok(())
}
