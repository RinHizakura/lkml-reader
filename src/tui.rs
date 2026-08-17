// SPDX-License-Identifier: GPL-2.0

//! The owner of the terminal: raw mode, the alternate screen, the cursor's
//! output handle and the bottom-line prompt loop all live here, so entering,
//! suspending and restoring can never get out of step — and every prompt
//! shares the same key rules by construction.

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use std::io::{stdin, stdout, BufRead, Stdout, Write};

use crate::ui;

pub enum PromptAction<R> {
    Continue,
    Cancel,
    Accept(R),
}

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

    /// Drive a prompt on the bottom line until `handle` accepts or cancels.
    /// Only key presses reach `handle`, and Ctrl-C always cancels — no
    /// prompt needs its own copy of either rule.
    pub fn prompt<F, R>(&mut self, label: &str, mut handle: F) -> Result<Option<R>>
    where
        F: FnMut(KeyEvent, &mut String) -> PromptAction<R>,
    {
        let mut input = String::new();
        ui::redraw_prompt(&mut self.out, Self::size(), label, &input)?;

        loop {
            if let Event::Key(k) = event::read()? {
                if k.kind != KeyEventKind::Press {
                    continue;
                }
                let action = match k.code {
                    KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                        PromptAction::Cancel
                    }
                    _ => handle(k, &mut input),
                };
                match action {
                    PromptAction::Continue => {}
                    PromptAction::Cancel => return Ok(None),
                    PromptAction::Accept(r) => return Ok(Some(r)),
                }
                ui::redraw_prompt(&mut self.out, Self::size(), label, &input)?;
            }
        }
    }

    /// Prompt for a line of text with the usual editing keys: Enter accepts,
    /// Esc cancels (→ `None`), Backspace deletes, and any printable
    /// non-control character is appended.
    pub fn prompt_line(&mut self, label: &str) -> Result<Option<String>> {
        self.prompt(label, |k, input| match k.code {
            KeyCode::Enter => PromptAction::Accept(input.clone()),
            KeyCode::Esc => PromptAction::Cancel,
            KeyCode::Backspace => {
                input.pop();
                PromptAction::Continue
            }
            KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
                input.push(c);
                PromptAction::Continue
            }
            _ => PromptAction::Continue,
        })
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.out, LeaveAlternateScreen);
    }
}

/// Wait for the user to press Enter before the TUI paints back over whatever a
/// child process left on the plain terminal.
fn pause() {
    print!("\nPress Enter to return to the reader.");
    let _ = stdout().flush();
    let _ = stdin().lock().read_line(&mut String::new());
}
