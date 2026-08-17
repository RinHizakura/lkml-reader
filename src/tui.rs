// SPDX-License-Identifier: GPL-2.0

//! The owner of the terminal: raw mode, the alternate screen, the cursor's
//! output handle, the screen measurement and the blocking bottom-line
//! confirm all live here, so entering, suspending and restoring can never
//! get out of step.

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use std::io::{stdin, stdout, BufRead, Stdout, Write};

use crate::ui;

pub struct Tui {
    out: Stdout,
}

impl Tui {
    /// Enter raw mode on the alternate screen. `Drop` restores the terminal,
    /// however the app exits.
    pub fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut out = stdout();
        execute!(out, EnterAlternateScreen)?;
        Ok(Self { out })
    }

    /// The output handle every draw goes through.
    pub fn out(&mut self) -> &mut Stdout {
        &mut self.out
    }

    /// The terminal size, with a sane default when it cannot be read. The one
    /// place the screen is measured — every layout question starts here, and
    /// the draw functions take the answer as a parameter so they never have to
    /// touch a real terminal.
    pub fn size() -> (u16, u16) {
        crossterm::terminal::size().unwrap_or((80, 24))
    }

    /// Run `f` with the TUI suspended so a child process (`$EDITOR`, `git`)
    /// owns the plain terminal, wait for acknowledgement, then restore the
    /// alternate screen — however `f` returned.
    pub fn suspended<F>(&mut self, f: F) -> Result<()>
    where
        F: FnOnce() -> Result<()>,
    {
        disable_raw_mode()?;
        execute!(self.out, LeaveAlternateScreen)?;
        let outcome = f();
        pause();
        enable_raw_mode()?;
        execute!(self.out, EnterAlternateScreen)?;
        outcome
    }

    /// Ask a yes/no question on the bottom line, blocking until a key press
    /// answers it: y/Y agrees, anything else (Ctrl-C included) declines.
    /// Deliberately blocking — its one use gates a clone that will block
    /// everything anyway.
    pub fn confirm(&mut self, label: &str) -> Result<bool> {
        ui::redraw_prompt(&mut self.out, Self::size(), label, "")?;
        loop {
            if let Event::Key(k) = event::read()? {
                if k.kind != KeyEventKind::Press {
                    continue;
                }
                return Ok(matches!(k.code, KeyCode::Char('y') | KeyCode::Char('Y')));
            }
        }
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.out, LeaveAlternateScreen);
    }
}

/// Ask a yes/no question on the plain terminal, answered with a typed line —
/// the suspended-mode sibling of [`Tui::confirm`], for code running inside
/// [`Tui::suspended`] where raw mode is off and stdin is line-buffered.
pub fn confirm_line(prompt: &str) -> Result<bool> {
    print!("{prompt}");
    stdout().flush()?;
    let mut answer = String::new();
    stdin().lock().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y"))
}

/// Wait for the user to press Enter before the TUI paints back over whatever a
/// child process left on the plain terminal.
fn pause() {
    print!("\nPress Enter to return to the reader.");
    let _ = stdout().flush();
    let _ = stdin().lock().read_line(&mut String::new());
}
