mod app;
mod buffer;
mod diff;
mod gitdiff;
mod highlight;
mod scan;
mod text;
mod tree;
mod ui;

use std::io::{BufWriter, Stdout, Write, stdout};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use ratatui::crossterm::terminal::{
    BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen, enable_raw_mode,
};
use ratatui::crossterm::{execute, queue};

/// Large buffer so a full-screen redraw goes out in a few writes instead of one per KiB.
type Term = Terminal<CrosstermBackend<BufWriter<Stdout>>>;

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
    /// git difftool mode: left is read-only, right is editable
    /// (auto-detected for `git difftool -d` temp dirs, including
    /// `--no-symlinks` copies, which git copies back on exit)
    #[arg(long)]
    git: bool,
    /// In git mode, allow editing all right-side files (e.g. internal
    /// `tuidiff REV..REV` commit blobs, which are otherwise read-only)
    #[arg(long)]
    right_editable: bool,
    /// Open everything read-only
    #[arg(long)]
    readonly: bool,
    /// Do not pair left-only and right-only files as renames
    #[arg(long)]
    no_renames: bool,
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
    let (left, right, git_head, range_mode, _guard) = match (range, cli.right) {
        (Some((l, r, three)), dir) => {
            let d = gitdiff::prepare_range(dir.as_deref().unwrap_or(Path::new(".")), l, r, three)?;
            (d.left.clone(), d.right.clone(), true, true, Some(d))
        }
        (None, Some(right)) => (left, right, false, false, None),
        (None, None) => {
            let d = gitdiff::prepare(&left)?;
            (d.left.clone(), d.right.clone(), true, false, Some(d))
        }
    };
    let opts = Options {
        git: cli.git || git_head,
        right_editable: cli.right_editable,
        readonly: cli.readonly,
        range_mode,
        renames: !cli.no_renames,
        theme: cli.theme,
    };
    let mut app = App::new(left, right, opts)?;

    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(stdout(), DisableMouseCapture, DisableBracketedPaste);
        ratatui::restore();
        prev_hook(info);
    }));
    enable_raw_mode()?;
    execute!(stdout(), EnterAlternateScreen, EnableMouseCapture, EnableBracketedPaste)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(BufWriter::with_capacity(1 << 20, stdout())))?;
    let res = run(&mut terminal, &mut app);
    let _ = execute!(stdout(), DisableMouseCapture, DisableBracketedPaste);
    ratatui::restore();
    res
}

fn run(terminal: &mut Term, app: &mut App) -> Result<()> {
    while !app.quit {
        queue!(terminal.backend_mut(), BeginSynchronizedUpdate)?;
        terminal.draw(|f| ui::draw(f, app))?;
        execute!(terminal.backend_mut(), EndSynchronizedUpdate)?;
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
