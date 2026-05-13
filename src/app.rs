// SPDX-License-Identifier: GPL-2.0

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{
        disable_raw_mode, enable_raw_mode, size, EnterAlternateScreen, LeaveAlternateScreen,
    },
};
use std::collections::HashMap;
use std::io::{stdout, Write};
use std::path::PathBuf;
use std::time::Duration;

use crate::archive;
use crate::mail::{Mail, Page};
use crate::parse;
use crate::ui;

pub enum View {
    Loading(String),
    List,
    Detail,
    Help,
}

pub struct App {
    list_name: String,
    filter: String,

    available_epochs: Vec<u32>,
    epoch_cursor: usize,
    cur_epoch: u32,
    current_repo: Option<PathBuf>,

    /// Lazy cache of commit hashes per epoch. Populated on first visit.
    epoch_commits: HashMap<u32, Vec<String>>,

    /// When Some, pagination operates on this precomputed filtered list.
    filtered_stream: Option<Vec<(u32, String)>>,

    page_size: usize,
    page_idx: usize,
    current_page: Page,
    selected: usize,

    view: View,
    detail_text: String,
    detail_scroll: usize,
}

fn page_size_for_terminal() -> usize {
    let (_, rows) = size().unwrap_or((80, 24));
    (rows as usize).saturating_sub(3).max(10)
}

impl App {
    pub fn new(list_name: String) -> Result<Self> {
        Ok(Self {
            list_name,
            filter: String::new(),
            available_epochs: Vec::new(),
            epoch_cursor: 0,
            cur_epoch: 0,
            current_repo: None,
            epoch_commits: HashMap::new(),
            filtered_stream: None,
            page_size: page_size_for_terminal(),
            page_idx: 0,
            current_page: Page::default(),
            selected: 0,
            view: View::Loading("Starting…".to_string()),
            detail_text: String::new(),
            detail_scroll: 0,
        })
    }

    fn update_cur_epoch(&mut self, epoch: usize) {
        self.epoch_cursor = epoch;
        self.cur_epoch = self.available_epochs[self.epoch_cursor];
    }

    fn bootstrap_manifest<W: Write>(&mut self, out: &mut W) -> Result<()> {
        self.view = View::Loading(format!("Fetching manifest for '{}'…", self.list_name));
        self.render(out)?;

        let client = archive::http_client()?;
        if let Ok(manifest) = archive::fetch_manifest(&client) {
            let epochs = archive::manifest_epochs(&manifest, &self.list_name);
            if epochs.is_empty() {
                return Err(anyhow::anyhow!(
                    "No epochs found for list '{}'",
                    self.list_name
                ));
            }
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

        if exists {
            archive::update_mirror(&self.list_name, self.cur_epoch)?;
        } else {
            archive::clone_mirror(&self.list_name, self.cur_epoch)?;
        }

        /* Assume the mirror is up-to-date and always exists after this. */

        Ok(())
    }

    /// Reload from scratch: clear caches, reset to page 0, load.
    pub fn refresh<W: Write>(&mut self, out: &mut W) -> Result<()> {
        self.epoch_commits.clear();
        self.filtered_stream = None;
        self.page_idx = 0;
        self.current_page = Page::default();
        self.selected = 0;
        self.current_repo = None;

        self.current_repo = Some(archive::local_repo_path(&self.list_name, self.cur_epoch));

        if !self.filter.trim().is_empty() {
            self.run_filter_scan(out)?;
        }
        self.load_page_at(0, out)?;
        Ok(())
    }

    /// Load all commit hashes for `epoch` into the cache, auto-cloning if needed.
    fn ensure_epoch_commits<W: Write>(&mut self, epoch: u32, out: &mut W) -> Result<()> {
        if self.epoch_commits.contains_key(&epoch) {
            return Ok(());
        }
        if !archive::repo_exists(&self.list_name, epoch) {
            let prev_view = std::mem::replace(
                &mut self.view,
                View::Loading(format!(
                    "Cloning {} epoch {} (this may take a while)…",
                    self.list_name, epoch
                )),
            );
            let _ = self.render(out);
            let clone_result = archive::clone_mirror(&self.list_name, epoch);
            self.view = prev_view;
            clone_result?;
        }
        let repo = archive::local_repo_path(&self.list_name, epoch);
        let commits = archive::list_all_commits(&repo)?;
        self.epoch_commits.insert(epoch, commits);
        Ok(())
    }

    /// Length of the active stream, when knowable.
    pub fn stream_total(&self) -> Option<usize> {
        if let Some(filtered) = &self.filtered_stream {
            return Some(filtered.len());
        }
        let mut total = 0usize;
        for &epoch in &self.available_epochs {
            match self.epoch_commits.get(&epoch) {
                Some(v) => total += v.len(),
                None => return None,
            }
        }
        Some(total)
    }

    pub fn total_pages(&self) -> Option<usize> {
        let total = self.stream_total()?;
        if total == 0 {
            return Some(0);
        }
        Some((total + self.page_size - 1) / self.page_size)
    }

    /// Walk epochs newest-first to collect `count` (epoch, commit) pairs
    /// starting at global offset `offset`. Used when no filter is active.
    fn resolve_stream_window<W: Write>(
        &mut self,
        offset: usize,
        count: usize,
        out: &mut W,
    ) -> Result<Vec<(u32, String)>> {
        let mut items: Vec<(u32, String)> = Vec::new();
        let mut to_skip = offset;
        let mut need = count;
        let mut eidx = self.available_epochs.len() - 1;
        loop {
            if need == 0 {
                break;
            }
            let epoch = self.available_epochs[eidx];
            if self.ensure_epoch_commits(epoch, out).is_err() {
                break;
            }
            let n = self
                .epoch_commits
                .get(&epoch)
                .map(|v| v.len())
                .unwrap_or(0);
            if to_skip >= n {
                to_skip -= n;
            } else if n > 0 {
                let start = to_skip;
                let end = (start + need).min(n);
                let commits = self.epoch_commits.get(&epoch).unwrap();
                for c in &commits[start..end] {
                    items.push((epoch, c.clone()));
                }
                need -= end - start;
                to_skip = 0;
            }
            if eidx == 0 {
                break;
            }
            eidx -= 1;
        }
        Ok(items)
    }

    /// Fetch and materialize a single Page at the given page index.
    /// Returns an empty `Page` when past the end of the stream.
    fn fetch_page<W: Write>(&mut self, page_idx: usize, out: &mut W) -> Result<Page> {
        let start = page_idx * self.page_size;
        let stream_slice: Vec<(u32, String)> = match &self.filtered_stream {
            Some(filtered) => {
                if start >= filtered.len() {
                    Vec::new()
                } else {
                    let end = (start + self.page_size).min(filtered.len());
                    filtered[start..end].to_vec()
                }
            }
            None => self.resolve_stream_window(start, self.page_size, out)?,
        };
        let mut mails: Vec<Mail> = Vec::with_capacity(stream_slice.len());
        for (epoch, commit) in stream_slice {
            let repo = archive::local_repo_path(&self.list_name, epoch);
            if let Ok(raw) = archive::show_mail(&repo, &commit) {
                mails.push(archive::parse_mail_from_raw(&raw, epoch, commit));
            }
        }
        Ok(Page::new(mails))
    }

    /// Pin selection bounds and update header context from the current page.
    fn snap_view_from_current_page(&mut self) {
        if self.current_page.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.current_page.len() {
            self.selected = self.current_page.len() - 1;
        }
        if let Some(first_epoch) = self.current_page.mails.first().map(|m| m.epoch) {
            if let Some(i) = self.available_epochs.iter().position(|&e| e == first_epoch) {
                self.update_cur_epoch(i);
                self.current_repo = Some(archive::local_repo_path(&self.list_name, first_epoch));
            }
        }
    }

    /// Fetch and display page `idx`.
    pub fn load_page_at<W: Write>(&mut self, idx: usize, out: &mut W) -> Result<()> {
        self.page_idx = idx;
        self.current_page = self.fetch_page(idx, out)?;
        self.snap_view_from_current_page();
        Ok(())
    }

    /// Step to the next page. Fetches eagerly; if the result is empty we
    /// treat that as end-of-stream and leave the current page untouched.
    pub fn next_page<W: Write>(&mut self, out: &mut W) -> Result<()> {
        let next_idx = self.page_idx + 1;
        let next_page = self.fetch_page(next_idx, out)?;
        if next_page.is_empty() {
            return Ok(());
        }
        self.page_idx = next_idx;
        self.current_page = next_page;
        self.selected = 0;
        self.snap_view_from_current_page();
        Ok(())
    }

    /// Step to the previous page, clamping at index 0.
    pub fn prev_page<W: Write>(&mut self, out: &mut W) -> Result<()> {
        if self.page_idx == 0 {
            return Ok(());
        }
        self.load_page_at(self.page_idx - 1, out)
    }

    /// Eagerly scan every locally-cloned epoch and collect commits whose
    /// subject matches the current filter. Populates `filtered_stream`.
    fn run_filter_scan<W: Write>(&mut self, out: &mut W) -> Result<()> {
        let needle = self.filter.trim().to_lowercase();
        if needle.is_empty() {
            self.filtered_stream = None;
            return Ok(());
        }

        let prev_view = std::mem::replace(&mut self.view, View::Loading(String::new()));

        let mut matches: Vec<(u32, String)> = Vec::new();
        let mut scanned: usize = 0;

        for i in (0..self.available_epochs.len()).rev() {
            let epoch = self.available_epochs[i];
            if !archive::repo_exists(&self.list_name, epoch) {
                continue;
            }
            let repo = archive::local_repo_path(&self.list_name, epoch);
            let Ok(commits) = archive::list_all_commits(&repo) else {
                continue;
            };
            for commit in &commits {
                if let Ok(raw) = archive::show_mail(&repo, commit) {
                    let mail = archive::parse_mail_from_raw(&raw, epoch, commit.clone());
                    if mail.title.to_lowercase().contains(&needle) {
                        matches.push((epoch, commit.clone()));
                    }
                }
                scanned += 1;
                if scanned % 200 == 0 {
                    self.view = View::Loading(format!(
                        "Filtering '{}': scanned {}, matches {}…",
                        self.filter,
                        scanned,
                        matches.len()
                    ));
                    let _ = self.render(out);
                }
            }
            self.epoch_commits.insert(epoch, commits);
        }

        self.view = prev_view;
        self.filtered_stream = Some(matches);
        self.selected = 0;
        Ok(())
    }

    pub fn apply_filter<W: Write>(&mut self, out: &mut W) -> Result<()> {
        if self.filter.trim().is_empty() {
            self.filtered_stream = None;
            self.selected = 0;
        } else {
            self.run_filter_scan(out)?;
        }
        self.load_page_at(0, out)?;
        Ok(())
    }

    pub fn open_selected(&mut self) -> Result<()> {
        let Some(mail) = self.current_page.mails.get(self.selected) else {
            return Ok(());
        };
        let repo = archive::local_repo_path(&self.list_name, mail.epoch);
        let raw = archive::show_mail(&repo, &mail.commit)?;
        self.detail_text = parse::format_raw_mail(&raw);
        self.detail_scroll = 0;
        self.view = View::Detail;
        Ok(())
    }

    /// Build per-view structs from current state and dispatch to ui::draw_*.
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

    pub fn render_list<W: Write>(&self, out: &mut W) -> Result<()> {
        let epoch_label = self.epoch_label();
        let page_label = self.page_label();
        let empty_message: Vec<String> = if self.current_page.is_empty() {
            if self.current_repo.is_none() {
                let expected = match self.available_epochs.last() {
                    Some(&epoch) => archive::local_repo_path(&self.list_name, epoch)
                        .display()
                        .to_string(),
                    None => archive::archive_root()
                        .join(&self.list_name)
                        .display()
                        .to_string(),
                };
                vec![
                    format!("No local mirror for list '{}'.", self.list_name),
                    format!("Expected: {}", expected),
                    "The TUI clones the latest epoch automatically — check your network and try again.".to_string(),
                ]
            } else if self.filter.trim().is_empty() {
                vec!["No mails on this page.".to_string()]
            } else {
                vec!["No mails match filter. Press '/' to change it.".to_string()]
            }
        } else {
            Vec::new()
        };
        ui::draw_list(
            out,
            &ui::ListView {
                header: self.header_info(&epoch_label, &page_label),
                page_idx: self.page_idx,
                page_size: self.page_size,
                stream_total: self.stream_total(),
                mails: &self.current_page.mails,
                selected: self.selected,
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
            filter: &self.filter,
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
        match self.total_pages() {
            Some(n) => format!("{}/{}", self.page_idx + 1, n.max(1)),
            None => format!("{}+", self.page_idx + 1),
        }
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
                        let prev_global = self.page_idx * self.page_size + self.selected;
                        self.page_size = page_size_for_terminal();
                        let new_idx = prev_global / self.page_size;
                        self.selected = prev_global % self.page_size;
                        let _ = self.load_page_at(new_idx, out);
                        self.render(out)?;
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    fn handle_prompt(&self, label: &str) -> Result<Option<String>> {
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
                match k.code {
                    KeyCode::Enter => return Ok(Some(input)),
                    KeyCode::Esc => return Ok(None),
                    KeyCode::Backspace => {
                        input.pop();
                    }
                    KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
                        input.push(c);
                    }
                    _ => {}
                }
                ui::redraw_prompt(&mut out, label, &input, y)?;
            }
        }
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
                    }
                }
                KeyCode::Up => {
                    if self.selected > 0 {
                        self.selected -= 1;
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
                KeyCode::Char('/') => {
                    if let Some(s) = self.handle_prompt(&format!(
                        "Filter (subject substring, empty=clear) [{}]: ",
                        self.filter
                    ))? {
                        self.filter = s;
                        let _ = self.apply_filter(out);
                    }
                }
                KeyCode::Char('u') => {
                    self.view = View::Loading(format!(
                        "Updating mirror {} epoch {}…",
                        self.list_name, self.cur_epoch
                    ));
                    self.render(out)?;
                    if archive::update_mirror(&self.list_name, self.cur_epoch).is_ok() {
                        self.view = View::Loading("Reloading mails…".to_string());
                        self.render(out)?;
                        let _ = self.refresh(out);
                    }
                    self.view = View::List;
                }
                KeyCode::Char('?') => self.view = View::Help,
                _ => {}
            },
            View::Detail => match key.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Backspace => {
                    self.view = View::List;
                }
                KeyCode::Down => self.detail_scroll += 1,
                KeyCode::Up => self.detail_scroll = self.detail_scroll.saturating_sub(1),
                KeyCode::PageDown | KeyCode::Char(' ') => self.detail_scroll += 20,
                KeyCode::PageUp => self.detail_scroll = self.detail_scroll.saturating_sub(20),
                KeyCode::Home | KeyCode::Char('g') => self.detail_scroll = 0,
                KeyCode::End | KeyCode::Char('G') => self.detail_scroll = usize::MAX,
                _ => {}
            },
            View::Help => self.view = View::List,
            View::Loading(_) => {}
        }
        Ok(false)
    }
}


