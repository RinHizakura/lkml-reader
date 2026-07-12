// SPDX-License-Identifier: GPL-2.0

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{
        disable_raw_mode, enable_raw_mode, size, EnterAlternateScreen, LeaveAlternateScreen,
    },
};
use std::io::{stdin, stdout, BufRead, Write};
use std::time::{Duration, Instant};

use lkml_core::archive;
use lkml_core::filter::{DateFilter, Filter, NameFilter};
use lkml_core::thread;

use crate::patch;
use crate::reply;
use crate::source::{FilteredSource, MailSource, Page, SourceStatus, StreamSource};
use crate::ui;

pub enum View {
    Loading(String),
    List,
    Detail,
    Help,
}

enum PromptAction<R> {
    Continue,
    Cancel,
    Accept(R),
}

pub struct App {
    list_name: String,
    subject_filter: NameFilter,
    author_filter: NameFilter,
    date_filter: DateFilter,

    available_epochs: Vec<u32>,
    epoch_cursor: usize,
    cur_epoch: u32,
    /// Whether the current epoch's mirror has been prepared; gates the
    /// "no local mirror" empty-state message. The archive module owns the
    /// actual paths, so the app only tracks readiness, not where it lives.
    repo_ready: bool,

    /// Where mails come from: the full unfiltered stream or an active filtered
    /// scan. Owns any per-source paging state (caches, pending page).
    source: MailSource,

    page_size: usize,
    current_page: Page,
    selected: usize,

    view: View,
    detail_text: String,
    detail_scroll: usize,

    /// The tree `git am` applies to, remembered across applies so the prompt
    /// only has to be answered once per session. Starts at the cwd.
    repo_path: String,

    /// Marquee scroll position for the currently selected row's title. Advances
    /// once per tick while sitting on a long-title row so the user can read
    /// past the column's right edge.
    selected_title_scroll: usize,
    scroll_last_tick: Instant,
}

fn page_size_for_terminal() -> usize {
    let (_, rows) = size().unwrap_or((80, 24));
    (rows as usize).saturating_sub(3).max(10)
}

/// Expand a leading `~`/`~/…` to `$HOME`, like a shell would. `git` is spawned
/// directly (no shell), so the prompt has to do this itself or the tilde reaches
/// git verbatim and fails.
fn expand_tilde(path: &str) -> String {
    let rest = path
        .strip_prefix("~/")
        .or_else(|| (path == "~").then_some(""));
    match (rest, std::env::var("HOME")) {
        (Some(""), Ok(home)) => home,
        (Some(rest), Ok(home)) => format!("{home}/{rest}"),
        _ => path.to_string(),
    }
}

/// Wait for the user to press Enter before the TUI paints back over whatever a
/// child process left on the plain terminal.
fn pause() {
    print!("\nPress Enter to return to the reader.");
    let _ = stdout().flush();
    let _ = stdin().lock().read_line(&mut String::new());
}

/// Run `f` with the TUI suspended so a child process (`$EDITOR`, `git`) owns the
/// terminal, wait for acknowledgement, then restore the alternate screen —
/// however `f` returned.
fn suspended<W, F>(out: &mut W, f: F) -> Result<()>
where
    W: Write,
    F: FnOnce() -> Result<()>,
{
    disable_raw_mode()?;
    execute!(out, LeaveAlternateScreen)?;
    let outcome = f();
    pause();
    enable_raw_mode()?;
    execute!(out, EnterAlternateScreen)?;
    outcome
}

impl App {
    pub fn new(list_name: String) -> Result<Self> {
        let source = MailSource::Stream(StreamSource::new(list_name.clone(), Vec::new()));
        Ok(Self {
            list_name,
            subject_filter: NameFilter::subject(),
            author_filter: NameFilter::author(),
            date_filter: DateFilter::new(),
            available_epochs: Vec::new(),
            epoch_cursor: 0,
            cur_epoch: 0,
            repo_ready: false,
            source,
            page_size: page_size_for_terminal(),
            current_page: Page::default(),
            selected: 0,
            view: View::Loading("Starting…".to_string()),
            detail_text: String::new(),
            detail_scroll: 0,
            repo_path: std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            selected_title_scroll: 0,
            scroll_last_tick: Instant::now(),
        })
    }

    fn update_cur_epoch(&mut self, epoch: usize) {
        self.epoch_cursor = epoch;
        self.cur_epoch = self.available_epochs[self.epoch_cursor];
    }

    fn reset_title_scroll(&mut self) {
        self.selected_title_scroll = 0;
        self.scroll_last_tick = Instant::now();
    }

    /// Advance the marquee on the selected row when its title overflows the
    /// subject column. Returns true when state changed and a redraw is needed.
    fn tick_title_scroll(&mut self) -> bool {
        if !matches!(self.view, View::List) {
            return false;
        }
        let Some(mail) = self.current_page.mails.get(self.selected) else {
            return false;
        };
        let (cols, _) = size().unwrap_or((80, 24));
        let subject_w = ui::subject_column_width(
            cols,
            self.current_page.page_idx,
            self.page_size,
            self.current_page.mails.len(),
        );
        if mail.subject.chars().count() <= subject_w {
            if self.selected_title_scroll != 0 {
                self.selected_title_scroll = 0;
                return true;
            }
            return false;
        }
        let now = Instant::now();
        if now.duration_since(self.scroll_last_tick) < Duration::from_millis(250) {
            return false;
        }
        self.scroll_last_tick = now;
        self.selected_title_scroll = self.selected_title_scroll.wrapping_add(1);
        true
    }

    fn bootstrap_manifest<W: Write>(&mut self, out: &mut W) -> Result<()> {
        self.view = View::Loading(format!("Fetching manifest for '{}'…", self.list_name));
        self.render(out)?;

        /* Ask the archive module for the list's epochs; it owns the HTTP
         * client and manifest parsing. A network failure here is non-fatal: we
         * fall through to whatever mirror may already be cached locally. */
        if let Ok(epochs) = archive::list_epochs(&self.list_name) {
            /* Start at the latest epoch. */
            self.available_epochs = epochs;
            self.update_cur_epoch(self.available_epochs.len() - 1);
        }
        Ok(())
    }

    fn bootstrap_mirror<W: Write>(&mut self, out: &mut W) -> Result<()> {
        let exists = archive::repo_exists(&self.list_name, self.cur_epoch);
        let loading_message = if exists {
            format!("Updating mirror {} epoch {}…", self.list_name, self.cur_epoch)
        } else {
            format!(
                "Cloning mirror {} epoch {} (this may take a while)…",
                self.list_name, self.cur_epoch
            )
        };
        self.view = View::Loading(loading_message);
        self.render(out)?;

        /* The archive module decides clone-vs-update; `exists` above only
         * picks the right loading message. */
        archive::ensure_epoch(&self.list_name, self.cur_epoch)?;

        /* Assume the mirror is up-to-date and always exists after this. */

        Ok(())
    }

    /// Reload from scratch: drop to a fresh unfiltered stream, reset to page 0.
    pub fn refresh<W: Write>(&mut self, out: &mut W) -> Result<()> {
        self.source = MailSource::Stream(StreamSource::new(
            self.list_name.clone(),
            self.available_epochs.clone(),
        ));
        self.current_page = Page::default();
        self.selected = 0;
        self.repo_ready = true;
        self.resolve_page(0, out)?;
        Ok(())
    }

    pub fn next_page<W: Write>(&mut self, out: &mut W) -> Result<()> {
        let target = self.current_page.page_idx + 1;
        self.source.request_page(target);
        self.resolve_page(target, out)
    }

    /// Step to the previous page, clamping at index 0.
    pub fn prev_page<W: Write>(&mut self, out: &mut W) -> Result<()> {
        if self.current_page.page_idx == 0 {
            return Ok(());
        }
        let target = self.current_page.page_idx - 1;
        self.source.request_page(target);
        self.resolve_page(target, out)
    }

    /// Whether any filter constrains the stream.
    fn any_filter_active(&self) -> bool {
        self.subject_filter.is_active()
            || self.author_filter.is_active()
            || self.date_filter.is_active()
    }

    /// (Re)start filtering from the current subject, author and date
    /// constraints. When none is active, drop any running job and fall back to
    /// the unfiltered stream.
    pub fn apply_filter<W: Write>(&mut self, out: &mut W) -> Result<()> {
        self.current_page = Page::default();
        self.selected = 0;

        if !self.any_filter_active() {
            // Reassigning drops any previous filter, cancelling its worker.
            self.source = MailSource::Stream(StreamSource::new(
                self.list_name.clone(),
                self.available_epochs.clone(),
            ));
            self.resolve_page(0, out)?;
            return Ok(());
        }

        // Reassigning drops any previous filter, cancelling its worker.
        self.source = MailSource::Filtered(FilteredSource::start(
            self.list_name.clone(),
            self.subject_filter.clone(),
            self.author_filter.clone(),
            self.date_filter.clone(),
            &self.available_epochs,
        ));
        self.view = View::Loading(format!(
            "Filtering subject='{}' author='{}' date='{}'…",
            self.subject_filter, self.author_filter, self.date_filter
        ));
        Ok(())
    }

    /// Advance any background work and, if a page is pending, try to serve it.
    /// Returns true when the view changed and a redraw is warranted.
    fn poll_source<W: Write>(&mut self, out: &mut W) -> Result<bool> {
        self.source.poll();
        match self.source.pending_page() {
            Some(target) => {
                self.resolve_page(target, out)?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Drive the active source toward serving page `target`: show it when
    /// ready, keep a loading screen up while work is pending, or prompt to
    /// clone the next epoch when the source is blocked.
    fn resolve_page<W: Write>(&mut self, target: usize, out: &mut W) -> Result<()> {
        loop {
            match self.source.status(target, self.page_size)? {
                SourceStatus::Ready(page) => {
                    self.current_page = page;
                    self.selected = 0;
                    self.view = View::List;
                    self.source.clear_pending();
                    self.reset_title_scroll();
                    return Ok(());
                }
                SourceStatus::Loading(message) => {
                    self.view = View::Loading(message);
                    return Ok(());
                }
                SourceStatus::Exhausted => {
                    self.source.clear_pending();
                    self.view = View::List;
                    return Ok(());
                }
                SourceStatus::NeedsClone(epoch) => {
                    if self.prompt_clone(epoch)? {
                        self.view = View::Loading(format!(
                            "Cloning {} epoch {} (this may take a while)…",
                            self.list_name, epoch
                        ));
                        self.render(out)?;
                        if archive::ensure_epoch(&self.list_name, epoch).is_err() {
                            self.source.clear_pending();
                            self.view = View::List;
                            return Ok(());
                        }
                        self.source.on_cloned(epoch);
                    } else if !self.source.decline_clone(epoch) {
                        self.source.clear_pending();
                        self.view = View::List;
                        return Ok(());
                    }
                }
            }
        }
    }

    /// Prompt the user to confirm cloning `epoch`. Returns whether they agreed.
    fn prompt_clone(&self, epoch: u32) -> Result<bool> {
        let label = format!("Clone {} epoch {}? [y/N]: ", self.list_name, epoch);
        Ok(self
            .handle_prompt(&label, |k, _| match k.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => PromptAction::Accept(()),
                _ => PromptAction::Cancel,
            })?
            .is_some())
    }

    pub fn open_selected(&mut self) -> Result<()> {
        let Some(text) = self
            .current_page
            .mails
            .get(self.selected)
            .map(|mail| mail.render_full())
        else {
            return Ok(());
        };
        self.detail_text = text;
        self.detail_scroll = 0;
        self.view = View::Detail;
        Ok(())
    }

    /// Reply to the selected mail, with `$EDITOR` and `git send-email` owning
    /// the terminal while it runs.
    fn reply_selected<W: Write>(&mut self, out: &mut W) -> Result<()> {
        let Some(draft) = self
            .current_page
            .mails
            .get(self.selected)
            .map(|mail| mail.reply_draft())
        else {
            return Ok(());
        };
        if let Err(e) = suspended(out, || reply::compose_and_send(&draft)) {
            self.view = View::Loading(format!("Reply not sent: {e}"));
        }
        Ok(())
    }

    /// Prompt for the target repo, then apply the selected mail's whole patch
    /// series with `git am`, with git owning the terminal while it runs.
    fn apply_patch<W: Write>(&mut self, out: &mut W) -> Result<()> {
        let Some(mail) = self.current_page.mails.get(self.selected).cloned() else {
            return Ok(());
        };
        if mail.patch_tag.is_none() {
            self.handle_prompt::<_, ()>("Not a patch mail. Press any key.", |_, _| {
                PromptAction::Cancel
            })?;
            return Ok(());
        }
        let label = format!("Apply series to git repo [{}]: ", self.repo_path);
        let Some(answer) = self.prompt_text(&label)? else {
            return Ok(());
        };
        let answer = answer.trim();
        let target = if answer.is_empty() {
            self.repo_path.clone()
        } else {
            expand_tilde(answer)
        };
        // Adopt the prompted path as the session default only once it proves to
        // be a real repo.
        if patch::is_git_repo(&target) {
            self.repo_path = target.clone();
        }

        let list = self.list_name.clone();
        let outcome = suspended(out, || {
            println!("Finding the rest of the series in the {list} mirror…");
            thread::patch_series(&list, &mail).and_then(|series| patch::apply(&target, &series))
        });
        if let Err(e) = outcome {
            self.view = View::Loading(format!("Not applied: {e}"));
        }
        Ok(())
    }

    /// Dispatch to the per-view renderer based on `self.view`.
    pub fn render<W: Write>(&self, out: &mut W) -> Result<()> {
        match &self.view {
            View::Loading(msg) => self.render_loading(out, msg),
            View::List => self.render_list(out),
            View::Detail => self.render_detail(out),
            View::Help => self.render_help(out),
        }
    }

    pub fn render_loading<W: Write>(&self, out: &mut W, message: &str) -> Result<()> {
        let epoch_label = self.epoch_label();
        let page_label = self.page_label();
        ui::draw_loading(
            out,
            &ui::LoadingView {
                header: self.header_info(&epoch_label, &page_label),
                message,
            },
        )
    }

    /// Redraw only the selected row, used for marquee ticks. Avoids the full
    /// screen clear in `render()` that would otherwise flicker at the tick
    /// rate. Safe to call when not in List view (it no-ops).
    fn render_selected_title<W: Write>(&self, out: &mut W) -> Result<()> {
        if !matches!(self.view, View::List) || self.current_page.is_empty() {
            return Ok(());
        }
        let epoch_label = self.epoch_label();
        let page_label = self.page_label();
        let empty: Vec<String> = Vec::new();
        ui::redraw_selected_row(
            out,
            &ui::ListView {
                header: self.header_info(&epoch_label, &page_label),
                page_idx: self.current_page.page_idx,
                page_size: self.page_size,
                mails: &self.current_page.mails,
                selected: self.selected,
                selected_scroll: self.selected_title_scroll,
                empty_message: &empty,
            },
        )
    }

    pub fn render_list<W: Write>(&self, out: &mut W) -> Result<()> {
        let epoch_label = self.epoch_label();
        let page_label = self.page_label();
        let empty_message: Vec<String> = if self.current_page.is_empty() {
            if !self.repo_ready {
                vec![
                    format!("No local mirror for list '{}'.", self.list_name),
                    "The TUI clones the latest epoch automatically — check your network and try again.".to_string(),
                ]
            } else if !self.any_filter_active() {
                vec!["No mails on this page.".to_string()]
            } else {
                vec!["No mails match filter. Press '/', 'a' or 'd' to change it.".to_string()]
            }
        } else {
            Vec::new()
        };
        ui::draw_list(
            out,
            &ui::ListView {
                header: self.header_info(&epoch_label, &page_label),
                page_idx: self.current_page.page_idx,
                page_size: self.page_size,
                mails: &self.current_page.mails,
                selected: self.selected,
                selected_scroll: self.selected_title_scroll,
                empty_message: &empty_message,
            },
        )
    }

    pub fn render_detail<W: Write>(&self, out: &mut W) -> Result<()> {
        let epoch_label = self.epoch_label();
        let page_label = self.page_label();
        ui::draw_detail(
            out,
            &ui::DetailView {
                header: self.header_info(&epoch_label, &page_label),
                text: &self.detail_text,
                scroll: self.detail_scroll,
            },
        )
    }

    pub fn render_help<W: Write>(&self, out: &mut W) -> Result<()> {
        let epoch_label = self.epoch_label();
        let page_label = self.page_label();
        ui::draw_help(
            out,
            &ui::HelpView {
                header: self.header_info(&epoch_label, &page_label),
            },
        )
    }

    fn header_info<'a>(&'a self, epoch_label: &'a str, page_label: &'a str) -> ui::HeaderInfo<'a> {
        ui::HeaderInfo {
            list_name: &self.list_name,
            epoch_label,
            page_label,
            subject_filter: &self.subject_filter,
            author_filter: &self.author_filter,
            date_filter: &self.date_filter,
        }
    }

    fn epoch_label(&self) -> String {
        if self.available_epochs.is_empty() {
            "-".to_string()
        } else {
            format!(
                "{} ({}/{})",
                self.cur_epoch,
                self.epoch_cursor + 1,
                self.available_epochs.len()
            )
        }
    }

    fn page_label(&self) -> String {
        format!("{}", self.current_page.page_idx + 1)
    }

    pub fn run(&mut self) -> Result<()> {
        let mut out = stdout();
        enable_raw_mode()?;
        execute!(out, EnterAlternateScreen)?;

        let result = match self.initialize(&mut out) {
            Ok(()) => self.run_loop(&mut out),
            Err(e) => Err(e),
        };

        disable_raw_mode().ok();
        execute!(out, LeaveAlternateScreen).ok();
        result
    }

    fn initialize<W: Write>(&mut self, out: &mut W) -> Result<()> {
        self.bootstrap_manifest(out)?;
        self.bootstrap_mirror(out)?;

        self.view = View::Loading("Loading mails…".to_string());
        self.render(out)?;
        let _ = self.refresh(out);

        self.view = View::List;
        self.render(out)?;

        Ok(())
    }

    pub fn run_loop<W: Write>(&mut self, out: &mut W) -> Result<()> {
        loop {
            if self.poll_source(out)? {
                self.render(out)?;
            }
            if self.tick_title_scroll() {
                self.render_selected_title(out)?;
            }
            if event::poll(Duration::from_millis(250))? {
                match event::read()? {
                    Event::Key(key) => {
                        if key.kind != KeyEventKind::Press {
                            continue;
                        }
                        if self.handle_key(out, key)? {
                            break;
                        }
                        self.render(out)?;
                    }
                    Event::Resize(_, _) => {
                        let prev_global = self.current_page.page_idx * self.page_size + self.selected;
                        self.page_size = page_size_for_terminal();
                        let new_idx = prev_global / self.page_size;
                        self.selected = prev_global % self.page_size;
                        self.source.request_page(new_idx);
                        let _ = self.resolve_page(new_idx, out);
                        self.render(out)?;
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    fn handle_prompt<F, R>(&self, label: &str, mut handle: F) -> Result<Option<R>>
    where
        F: FnMut(KeyEvent, &mut String) -> PromptAction<R>,
    {
        let (_, h) = size()?;
        let y = h.saturating_sub(1);
        let mut out = stdout();
        let mut input = String::new();

        ui::redraw_prompt(&mut out, label, &input, y)?;

        loop {
            if let Event::Key(k) = event::read()? {
                if k.kind != KeyEventKind::Press {
                    continue;
                }
                match handle(k, &mut input) {
                    PromptAction::Continue => {}
                    PromptAction::Cancel => return Ok(None),
                    PromptAction::Accept(r) => return Ok(Some(r)),
                }
                ui::redraw_prompt(&mut out, label, &input, y)?;
            }
        }
    }

    /// Prompt for a line of text on the bottom bar with the usual editing keys:
    /// Enter accepts, Esc cancels (→ `None`), Backspace deletes, and any
    /// printable non-control character is appended. The shared shape behind the
    /// filter prompts and the patch-repo prompt.
    fn prompt_text(&self, label: &str) -> Result<Option<String>> {
        self.handle_prompt(label, |k, input| match k.code {
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

    fn handle_key<W: Write>(&mut self, out: &mut W, key: KeyEvent) -> Result<bool> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
            return Ok(true);
        }
        match self.view {
            View::List => match key.code {
                KeyCode::Char('q') => return Ok(true),
                KeyCode::Down => {
                    if self.selected + 1 < self.current_page.len() {
                        self.selected += 1;
                        self.reset_title_scroll();
                    }
                }
                KeyCode::Up => {
                    if self.selected > 0 {
                        self.selected -= 1;
                        self.reset_title_scroll();
                    }
                }
                KeyCode::Right => {
                    let _ = self.next_page(out);
                }
                KeyCode::Left => {
                    let _ = self.prev_page(out);
                }
                KeyCode::Enter => {
                    let _ = self.open_selected();
                }
                KeyCode::Char('r') => self.reply_selected(out)?,
                KeyCode::Char('p') => self.apply_patch(out)?,
                KeyCode::Char('/') => {
                    let label = format!(
                        "Filter (subject substring, empty=clear) [{}]: ",
                        self.subject_filter
                    );
                    if let Some(s) = self.prompt_text(&label)? {
                        self.subject_filter.set(&s);
                        let _ = self.apply_filter(out);
                    }
                }
                KeyCode::Char('a') => {
                    let label = format!(
                        "Filter (author substring, empty=clear) [{}]: ",
                        self.author_filter
                    );
                    if let Some(s) = self.prompt_text(&label)? {
                        self.author_filter.set(&s);
                        let _ = self.apply_filter(out);
                    }
                }
                KeyCode::Char('d') => {
                    let label = format!(
                        "Filter date (today | yesterday | YYYY/MM/DD HH:MM to YYYY/MM/DD HH:MM, empty=clear) [{}]: ",
                        self.date_filter
                    );
                    if let Some(s) = self.prompt_text(&label)? {
                        match self.date_filter.set(&s) {
                            Ok(()) => {
                                let _ = self.apply_filter(out);
                            }
                            Err(e) => {
                                self.view = View::Loading(format!("Invalid date filter: {e}"));
                            }
                        }
                    }
                }
                KeyCode::Char('u') => {
                    self.view = View::Loading(format!(
                        "Updating mirror {} epoch {}…",
                        self.list_name, self.cur_epoch
                    ));
                    self.render(out)?;
                    if archive::ensure_epoch(&self.list_name, self.cur_epoch).is_ok() {
                        self.view = View::Loading("Reloading mails…".to_string());
                        self.render(out)?;
                        if !self.any_filter_active() {
                            let _ = self.refresh(out);
                            self.view = View::List;
                        } else {
                            // Re-run the background filter against the updated
                            // mirror; apply_filter leaves the loading screen up.
                            let _ = self.apply_filter(out);
                        }
                    } else {
                        self.view = View::List;
                    }
                }
                KeyCode::Char('?') => self.view = View::Help,
                _ => {}
            },
            View::Detail => match key.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Backspace => {
                    self.view = View::List;
                }
                KeyCode::Char('r') => self.reply_selected(out)?,
                KeyCode::Char('p') => self.apply_patch(out)?,
                KeyCode::Down => self.detail_scroll = self.detail_scroll.saturating_add(1),
                KeyCode::Up => self.detail_scroll = self.detail_scroll.saturating_sub(1),
                KeyCode::PageDown | KeyCode::Char(' ') => {
                    self.detail_scroll = self.detail_scroll.saturating_add(20)
                }
                KeyCode::PageUp => self.detail_scroll = self.detail_scroll.saturating_sub(20),
                KeyCode::Home | KeyCode::Char('g') => self.detail_scroll = 0,
                KeyCode::End | KeyCode::Char('G') => self.detail_scroll = usize::MAX,
                _ => {}
            },
            View::Help => self.view = View::List,
            View::Loading(_) => self.view = View::List,
        }
        Ok(false)
    }
}


