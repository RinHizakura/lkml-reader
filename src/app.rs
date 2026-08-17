// SPDX-License-Identifier: GPL-2.0

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    terminal::size,
};
use std::time::{Duration, Instant};

use lkml_core::archive;
use lkml_core::thread;

use crate::pages::Pages;
use crate::patch;
use crate::reply;
use crate::source::{FilterSet, FilteredSource, MailSource, PageState, StreamSource};
use crate::tui::{PromptAction, Tui};
use crate::ui;

enum View {
    Loading(String),
    List,
    Detail,
    Help,
}

pub struct App {
    list_name: String,
    filters: FilterSet,

    available_epochs: Vec<u32>,
    /// The newest epoch: the one bootstrapped and refreshed by `u`. Paging walks
    /// every epoch, so this only names the mirror the reader keeps current.
    cur_epoch: u32,
    /// Whether the current epoch's mirror has been prepared; gates the
    /// "no local mirror" empty-state message. The archive module owns the
    /// actual paths, so the app only tracks readiness, not where it lives.
    repo_ready: bool,

    /// Where mails come from: the full unfiltered stream or an active filtered
    /// scan. Owns its own caches; the app drives which page it serves.
    source: MailSource,

    /// Which page is showing, the visited-page history, the selection and the
    /// window scroll. Only a filtered scan ever keeps a page pending across
    /// run-loop ticks.
    pages: Pages,

    view: View,
    /// One-shot message drawn on the bottom line over whatever view is up:
    /// errors and end-of-stream notes. The next key clears it and still acts,
    /// so a notice never swallows input or changes the view.
    notice: Option<String>,
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

/// Rows a page fills — whatever the ui's content area can show. Floored at 1
/// so a degenerate window never asks the source for empty pages.
fn page_size_for_terminal() -> usize {
    let (_, rows) = size().unwrap_or((80, 24));
    ui::content_rows(rows).max(1)
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

/// Prompt the user to confirm cloning `epoch`. Returns whether they agreed.
fn prompt_clone(tui: &mut Tui, list: &str, epoch: u32) -> Result<bool> {
    let label = format!("Clone {list} epoch {epoch}? [y/N]: ");
    Ok(tui
        .prompt(&label, |k, _| match k.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => PromptAction::Accept(()),
            _ => PromptAction::Cancel,
        })?
        .is_some())
}

impl App {
    pub fn new(list_name: String) -> Self {
        let source = MailSource::Stream(StreamSource::new(list_name.clone(), Vec::new()));
        Self {
            list_name,
            filters: FilterSet::new(),
            available_epochs: Vec::new(),
            cur_epoch: 0,
            repo_ready: false,
            source,
            pages: Pages::new(page_size_for_terminal()),
            view: View::Loading("Starting…".to_string()),
            notice: None,
            detail_text: String::new(),
            detail_scroll: 0,
            repo_path: std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            selected_title_scroll: 0,
            scroll_last_tick: Instant::now(),
        }
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
        let Some(mail) = self.pages.selected_mail() else {
            return false;
        };
        let page = self.pages.current();
        if !ui::title_overflows(
            mail,
            page.offset,
            page.mails.len(),
            page.indented(self.pages.selected()),
        ) {
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

    fn bootstrap_manifest(&mut self, tui: &mut Tui) -> Result<()> {
        self.view = View::Loading(format!("Fetching manifest for '{}'…", self.list_name));
        self.render(tui)?;

        // A network failure here is non-fatal: fall through to whatever mirror
        // is already cached locally.
        if let Ok(epochs) = archive::list_epochs(&self.list_name) {
            self.cur_epoch = epochs.last().copied().unwrap_or_default();
            self.available_epochs = epochs;
        }
        Ok(())
    }

    fn bootstrap_mirror(&mut self, tui: &mut Tui) -> Result<()> {
        let exists = archive::repo_exists(&self.list_name, self.cur_epoch);
        let loading_message = if exists {
            format!(
                "Updating mirror {} epoch {}…",
                self.list_name, self.cur_epoch
            )
        } else {
            format!(
                "Cloning mirror {} epoch {} (this may take a while)…",
                self.list_name, self.cur_epoch
            )
        };
        self.view = View::Loading(loading_message);
        self.render(tui)?;

        // The archive module decides clone-vs-update; `exists` above only picks
        // the right loading message.
        archive::ensure_epoch(&self.list_name, self.cur_epoch)?;
        Ok(())
    }

    /// Read mails from `source` from now on, starting over at page 0. Dropping
    /// the source it replaces cancels any worker that one owned.
    fn read_from(&mut self, source: MailSource, tui: &mut Tui) -> Result<()> {
        self.source = source;
        let target = self.pages.reset();
        self.resolve_page(target, tui)
    }

    /// The unfiltered stream over every epoch we know of.
    fn stream(&self) -> MailSource {
        MailSource::Stream(StreamSource::new(
            self.list_name.clone(),
            self.available_epochs.clone(),
        ))
    }

    /// Reload from scratch: drop to a fresh unfiltered stream, reset to page 0.
    fn refresh(&mut self, tui: &mut Tui) -> Result<()> {
        self.repo_ready = true;
        self.read_from(self.stream(), tui)
    }

    /// Step to the page after the current one, if it has anywhere to start.
    fn next_page(&mut self, tui: &mut Tui) -> Result<()> {
        match self.pages.next_target() {
            Some(target) => self.resolve_page(target, tui),
            None => Ok(()),
        }
    }

    /// Step back to the page before the current one, stopping at the first.
    fn prev_page(&mut self, tui: &mut Tui) -> Result<()> {
        match self.pages.prev_target() {
            Some(target) => self.resolve_page(target, tui),
            None => Ok(()),
        }
    }

    /// (Re)start filtering from the current constraints. When none is active,
    /// drop any running job and fall back to the unfiltered stream.
    fn apply_filter(&mut self, tui: &mut Tui) -> Result<()> {
        if !self.filters.is_active() {
            return self.read_from(self.stream(), tui);
        }
        let scan = MailSource::Filtered(FilteredSource::start(
            self.list_name.clone(),
            self.filters.clone(),
            &self.available_epochs,
        ));
        // The scan has nothing yet, so this leaves the source's own loading
        // screen up; the run loop serves the page once matches arrive.
        self.read_from(scan, tui)
    }

    /// Advance any background work and, if a page is still pending, try again to
    /// serve it. Returns true when the view changed and a redraw is warranted.
    fn poll_source(&mut self, tui: &mut Tui) -> Result<bool> {
        self.source.poll();
        match self.pages.pending() {
            Some(target) => {
                self.resolve_page(target, tui)?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Drive the active source toward serving the page starting at `target`:
    /// show it when ready, or keep a loading screen up while work is pending.
    /// Clone negotiation happens inside the source; the app only supplies the
    /// consent prompt (and its progress screen). The page stays pending only
    /// while the source is still working on it.
    fn resolve_page(&mut self, target: usize, tui: &mut Tui) -> Result<()> {
        self.pages.begin(target);
        // The consent adapter only needs the terminal and the list name, so
        // the source stays in place while it borrows neither.
        let list = self.list_name.clone();
        let state = self
            .source
            .page(target, self.pages.page_size(), &mut |epoch| {
                if !prompt_clone(tui, &list, epoch)? {
                    return Ok(false);
                }
                ui::redraw_prompt(
                    tui.out(),
                    &format!("Cloning {list} epoch {epoch} (this may take a while)…"),
                    "",
                )?;
                Ok(true)
            });
        match state? {
            PageState::Ready(page) => {
                self.pages.accept(page);
                self.reset_title_scroll();
            }
            PageState::Pending(message) => {
                self.view = View::Loading(message);
                return Ok(());
            }
            PageState::End => {
                // Page 0 empty is already explained by the list's empty
                // message; past it, say why nothing changed.
                if target > 0 {
                    self.notice = Some("No more mails.".to_string());
                }
            }
        }
        self.pages.settle();
        self.view = View::List;
        Ok(())
    }

    fn open_selected(&mut self) -> Result<()> {
        let Some(text) = self.pages.selected_mail().map(|mail| mail.render_full()) else {
            return Ok(());
        };
        self.detail_text = text;
        self.detail_scroll = 0;
        self.view = View::Detail;
        Ok(())
    }

    /// Reply to the selected mail, with `$EDITOR` and `git send-email` owning
    /// the terminal while it runs.
    fn reply_selected(&mut self, tui: &mut Tui) -> Result<()> {
        let Some(draft) = self.pages.selected_mail().map(|mail| mail.reply_draft()) else {
            return Ok(());
        };
        if let Err(e) = tui.suspended(|| reply::compose_and_send(&draft)) {
            self.notice = Some(format!("Reply not sent: {e}"));
        }
        Ok(())
    }

    /// Prompt for the target repo, then apply the selected mail's whole patch
    /// series with `git am`, with git owning the terminal while it runs.
    fn apply_patch(&mut self, tui: &mut Tui) -> Result<()> {
        let Some(mail) = self.pages.selected_mail().cloned() else {
            return Ok(());
        };
        if mail.patch_tag.is_none() {
            self.notice = Some("Not a patch mail.".to_string());
            return Ok(());
        }
        let label = format!("Apply series to git repo [{}]: ", self.repo_path);
        let Some(answer) = tui.prompt_line(&label)? else {
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
        let outcome = tui.suspended(|| {
            println!("Finding the rest of the series in the {list} mirror…");
            thread::patch_series(&list, &mail).and_then(|series| patch::apply(&target, &series))
        });
        if let Err(e) = outcome {
            self.notice = Some(format!("Not applied: {e}"));
        }
        Ok(())
    }

    /// Dispatch to the per-view renderer based on `self.view`.
    fn render(&self, tui: &mut Tui) -> Result<()> {
        let (epoch_label, page_label) = (self.epoch_label(), self.pages.label());
        let header = self.header_info(&epoch_label, &page_label);
        let out = tui.out();
        match &self.view {
            View::Loading(msg) => ui::draw_loading(out, &header, msg),
            View::List => ui::draw_list(out, &self.list_view(header, &self.empty_message())),
            View::Detail => ui::draw_detail(out, &header, &self.detail_text, self.detail_scroll),
            View::Help => ui::draw_help(out, &header),
        }?;
        if let Some(notice) = &self.notice {
            ui::draw_notice(tui.out(), notice)?;
        }
        Ok(())
    }

    /// Redraw only the selected row, used for marquee ticks. Avoids the full
    /// screen clear in `render()` that would otherwise flicker at the tick
    /// rate. Safe to call when not in List view (it no-ops).
    fn render_selected_title(&self, tui: &mut Tui) -> Result<()> {
        if !matches!(self.view, View::List) || self.pages.current().is_empty() {
            return Ok(());
        }
        let (epoch_label, page_label) = (self.epoch_label(), self.pages.label());
        let header = self.header_info(&epoch_label, &page_label);
        ui::redraw_selected_row(tui.out(), &self.list_view(header, &[]))
    }

    /// What to say instead of rows when the page has none.
    fn empty_message(&self) -> Vec<String> {
        if !self.pages.current().is_empty() {
            Vec::new()
        } else if !self.repo_ready {
            vec![
                format!("No local mirror for list '{}'.", self.list_name),
                "The TUI clones the latest epoch automatically — check your network and try again."
                    .to_string(),
            ]
        } else if !self.filters.is_active() {
            vec!["No mails on this page.".to_string()]
        } else {
            vec!["No mails match filter. Press '/', 'a' or 'd' to change it.".to_string()]
        }
    }

    fn list_view<'a>(
        &'a self,
        header: ui::HeaderInfo<'a>,
        empty_message: &'a [String],
    ) -> ui::ListView<'a> {
        ui::ListView {
            header,
            offset: self.pages.current().offset,
            mails: &self.pages.current().mails,
            indent: self.pages.current().indent(),
            selected: self.pages.selected(),
            scroll: self.pages.scroll(),
            selected_scroll: self.selected_title_scroll,
            empty_message,
        }
    }

    fn header_info<'a>(&'a self, epoch_label: &'a str, page_label: &'a str) -> ui::HeaderInfo<'a> {
        ui::HeaderInfo {
            list_name: &self.list_name,
            epoch_label,
            page_label,
            subject_filter: &self.filters.subject,
            author_filter: &self.filters.author,
            date_filter: &self.filters.date,
        }
    }

    fn epoch_label(&self) -> String {
        match self.available_epochs.len() {
            0 => "-".to_string(),
            n => format!("{} (newest of {n})", self.cur_epoch),
        }
    }

    pub fn run(&mut self) -> Result<()> {
        let mut tui = Tui::enter()?;
        self.initialize(&mut tui)?;
        self.run_loop(&mut tui)
    }

    fn initialize(&mut self, tui: &mut Tui) -> Result<()> {
        self.bootstrap_manifest(tui)?;
        self.bootstrap_mirror(tui)?;

        self.view = View::Loading("Loading mails…".to_string());
        self.render(tui)?;
        // The unfiltered stream resolves synchronously, so this lands on the
        // list view (or an empty one) — nothing stays pending.
        self.refresh(tui)?;
        self.render(tui)
    }

    fn run_loop(&mut self, tui: &mut Tui) -> Result<()> {
        loop {
            if self.poll_source(tui)? {
                self.render(tui)?;
            }
            if self.tick_title_scroll() {
                self.render_selected_title(tui)?;
            }
            if event::poll(Duration::from_millis(250))? {
                match event::read()? {
                    Event::Key(key) => {
                        if key.kind != KeyEventKind::Press {
                            continue;
                        }
                        if self.handle_key(tui, key)? {
                            break;
                        }
                        self.render(tui)?;
                    }
                    Event::Resize(_, _) => {
                        let target = self.pages.resize(page_size_for_terminal());
                        let _ = self.resolve_page(target, tui);
                        self.render(tui)?;
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    fn handle_key(&mut self, tui: &mut Tui, key: KeyEvent) -> Result<bool> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
            return Ok(true);
        }
        self.notice = None;
        match self.view {
            View::List => match key.code {
                KeyCode::Char('q') => return Ok(true),
                KeyCode::Down => {
                    if self.pages.select_next() {
                        self.reset_title_scroll();
                    }
                }
                KeyCode::Up => {
                    if self.pages.select_prev() {
                        self.reset_title_scroll();
                    }
                }
                KeyCode::Right => {
                    let _ = self.next_page(tui);
                }
                KeyCode::Left => {
                    let _ = self.prev_page(tui);
                }
                KeyCode::Enter => {
                    let _ = self.open_selected();
                }
                KeyCode::Char('r') => self.reply_selected(tui)?,
                KeyCode::Char('p') => self.apply_patch(tui)?,
                KeyCode::Char('/') => {
                    let label = format!(
                        "Filter (subject substring, empty=clear) [{}]: ",
                        self.filters.subject
                    );
                    if let Some(s) = tui.prompt_line(&label)? {
                        self.filters.subject.set(&s);
                        let _ = self.apply_filter(tui);
                    }
                }
                KeyCode::Char('a') => {
                    let label = format!(
                        "Filter (author substring, empty=clear) [{}]: ",
                        self.filters.author
                    );
                    if let Some(s) = tui.prompt_line(&label)? {
                        self.filters.author.set(&s);
                        let _ = self.apply_filter(tui);
                    }
                }
                KeyCode::Char('d') => {
                    let label = format!(
                        "Filter date (today | yesterday | YYYY/MM/DD HH:MM to YYYY/MM/DD HH:MM, empty=clear) [{}]: ",
                        self.filters.date
                    );
                    if let Some(s) = tui.prompt_line(&label)? {
                        match self.filters.date.set(&s) {
                            Ok(()) => {
                                let _ = self.apply_filter(tui);
                            }
                            Err(e) => {
                                self.notice = Some(format!("Invalid date filter: {e}"));
                            }
                        }
                    }
                }
                KeyCode::Char('u') => {
                    self.view = View::Loading(format!(
                        "Updating mirror {} epoch {}…",
                        self.list_name, self.cur_epoch
                    ));
                    self.render(tui)?;
                    if archive::ensure_epoch(&self.list_name, self.cur_epoch).is_ok() {
                        self.view = View::Loading("Reloading mails…".to_string());
                        self.render(tui)?;
                        if !self.filters.is_active() {
                            let _ = self.refresh(tui);
                            self.view = View::List;
                        } else {
                            // Re-run the background filter against the updated
                            // mirror; apply_filter leaves the loading screen up.
                            let _ = self.apply_filter(tui);
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
                KeyCode::Char('r') => self.reply_selected(tui)?,
                KeyCode::Char('p') => self.apply_patch(tui)?,
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
            // A loading screen only stays up while a scan is pending, and the
            // poll tick repaints it — dismissing it goes nowhere, but quitting
            // must still work.
            View::Loading(_) => {
                if key.code == KeyCode::Char('q') {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::expand_tilde;

    #[test]
    fn expand_tilde_covers_home_and_leaves_the_rest() {
        let home = std::env::var("HOME").expect("test needs HOME");
        assert_eq!(expand_tilde("~"), home);
        assert_eq!(expand_tilde("~/x/y"), format!("{home}/x/y"));
        assert_eq!(expand_tilde("/abs/path"), "/abs/path");
        assert_eq!(expand_tilde("~user/x"), "~user/x"); // only the own-home forms
    }
}
