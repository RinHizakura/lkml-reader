// SPDX-License-Identifier: GPL-2.0

//! Where the TUI pulls mails from. Paging, lazy epoch walking and background
//! filtering are concepts that only the interactive reader has, so they live
//! here rather than in the shared `lkml-core` library, which stays about mail
//! parsing and archive I/O.

use anyhow::{anyhow, Result};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::thread as stdthread;

use lkml_core::archive;
use lkml_core::filter::{DateFilter, Filter, NameFilter};
use lkml_core::mail::{self, Mail};
use lkml_core::thread::{self, SeriesTag};

/// How far past `page_size` a page may grow to finish a patch series that
/// straddles its tail. Bounds the walk when a series is only partly in the
/// archive and so would never complete.
const SERIES_EXTEND_MAX: usize = 64;

/// How many mails to read ahead while chasing the tail of a series that the page
/// cut. The page can end on any one of them, so most of a batch may go unused —
/// but reading a batch costs about what reading a single mail used to, so a
/// modest look-ahead is still far cheaper than one git process per mail.
const CHASE_READAHEAD: usize = 16;

/// One page of mails, with each patch series pulled together into a block —
/// cover letter first, its patches indented under it.
///
/// A page is identified by where it starts in the stream, not by a page number,
/// because pages are not all the same length: one that would cut a series in
/// half keeps loading until the whole series fits.
#[derive(Clone, Default)]
pub struct Page {
    pub mails: Vec<Mail>,
    /// Per row: this mail sits under a series head and is drawn indented.
    /// Always exactly as long as `mails` — private so only the constructors
    /// here, which build the two in lockstep, can establish that.
    indent: Vec<bool>,
    pub offset: usize,
}

/// Internal outcome of asking a source whether a page can be served yet.
/// `NeedsClone` never escapes this module: `MailSource::page` negotiates it
/// away before answering the caller.
enum SourceStatus {
    /// The page is ready.
    Ready(Page),
    /// Still working; show this loading message.
    Loading(String),
    /// Progress is blocked until this epoch is cloned.
    NeedsClone(u32),
    /// No more mails to show.
    Exhausted,
}

/// Outcome of asking a source for a page.
pub enum PageState {
    /// The page is ready.
    Ready(Page),
    /// Still working; show this message and ask again later.
    Pending(String),
    /// No page to serve: the stream ran out, a needed clone was declined, or
    /// cloning failed.
    End,
}

impl Page {
    /// A page of the unfiltered stream: patch series pulled into blocks and
    /// marked for indentation.
    pub fn grouped(mails: Vec<Mail>, offset: usize) -> Self {
        let (mails, indent) = group_series(&mails);
        Self {
            mails,
            indent,
            offset,
        }
    }

    /// A page in stream order, nothing grouped. Filtered results are an
    /// arbitrary subset of the archive — a series is rarely all there, so there
    /// is nothing to hang an indent off.
    pub fn flat(mails: Vec<Mail>, offset: usize) -> Self {
        let indent = vec![false; mails.len()];
        Self {
            mails,
            indent,
            offset,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.mails.is_empty()
    }

    pub fn len(&self) -> usize {
        self.mails.len()
    }

    /// Whether row `i` sits under a series head; false past the page's end.
    pub fn indented(&self, i: usize) -> bool {
        self.indent.get(i).copied().unwrap_or(false)
    }

    /// One indent flag per row of `mails`.
    pub fn indent(&self) -> &[bool] {
        &self.indent
    }
}

/// Reorder `mails` so every patch series forms one block — cover letter (or
/// lowest-numbered patch) first, the rest ascending — sitting where the series'
/// newest mail was, and flag the members that belong under the head.
fn group_series(mails: &[Mail]) -> (Vec<Mail>, Vec<bool>) {
    let tags: Vec<Option<SeriesTag>> = mails.iter().map(thread::series_tag).collect();
    let mut series: HashMap<&SeriesTag, Vec<usize>> = HashMap::new();
    for (i, tag) in tags.iter().enumerate() {
        if let Some(tag) = tag {
            series.entry(tag).or_default().push(i);
        }
    }
    for members in series.values_mut() {
        members.sort_by_key(|&i| mails[i].patch_tag.map_or(0, |t| t.number));
    }

    let mut out = Vec::with_capacity(mails.len());
    let mut indent = Vec::with_capacity(mails.len());
    let mut placed = vec![false; mails.len()];
    for i in 0..mails.len() {
        if placed[i] {
            continue;
        }
        // The whole series lands here, where its newest mail sat. A lone member
        // stays put: there is nothing to indent it under.
        let block = match tags[i].as_ref().and_then(|tag| series.get(tag)) {
            Some(members) if members.len() > 1 => members.as_slice(),
            _ => std::slice::from_ref(&i),
        };
        for (nth, &j) in block.iter().enumerate() {
            placed[j] = true;
            out.push(mails[j].clone());
            indent.push(nth > 0);
        }
    }
    (out, indent)
}

/// Has the walk collected everything the page needs? Short of `page_size`, never.
/// At that point the page either ends cleanly and is done, or ends mid-series and
/// `chasing` takes up the rest of that series — bounded by [`SERIES_EXTEND_MAX`],
/// since a series only partly in the archive would never complete.
fn page_done(mails: &[Mail], page_size: usize, chasing: &mut Option<SeriesTag>) -> bool {
    if mails.len() < page_size {
        return false;
    }
    match chasing {
        Some(tag) => is_whole(mails, tag) || mails.len() >= page_size + SERIES_EXTEND_MAX,
        // Only the mail at the boundary counts. A series that looks half-present
        // further up the page is one whose siblings live somewhere else entirely
        // — an old patch resent, a stray `2/9` — and chasing every one of those
        // drags in mails that cut yet more series, page after page.
        None => {
            *chasing = thread::series_tag(mails.last().expect("page_size > 0"))
                .filter(|tag| !is_whole(mails, tag));
            chasing.is_none()
        }
    }
}

/// Is every patch of `tag` on the page? The 0/m cover letter is optional;
/// 1/m..m/m are not.
fn is_whole(mails: &[Mail], tag: &SeriesTag) -> bool {
    let seen: HashSet<u32> = mails
        .iter()
        .filter(|mail| thread::series_tag(mail).as_ref() == Some(tag))
        .filter_map(|mail| mail.patch_tag.map(|patch| patch.number))
        .collect();
    (1..=tag.total).all(|n| seen.contains(&n))
}

/// One of the three filter constraints, for driving the shared prompt flow.
#[derive(Clone, Copy)]
pub enum Constraint {
    Subject,
    Author,
    Date,
}

/// The subject, author and date constraints as one value. What the user is
/// prompted with, what is pushed down into `git log`, what runs as a
/// Rust-side predicate on the survivors, and how a scan reports progress all
/// come from here — the one home for every representation of "matches".
#[derive(Clone)]
pub struct FilterSet {
    subject: NameFilter,
    author: NameFilter,
    date: DateFilter,
}

impl FilterSet {
    pub fn new() -> Self {
        Self {
            subject: NameFilter::subject(),
            author: NameFilter::author(),
            date: DateFilter::new(),
        }
    }

    /// Whether any constraint is set.
    pub fn is_active(&self) -> bool {
        self.subject.is_active() || self.author.is_active() || self.date.is_active()
    }

    /// The prompt label for editing `which`, current value included.
    pub fn prompt_label(&self, which: Constraint) -> String {
        match which {
            Constraint::Subject => format!(
                "Filter (subject substring, empty=clear) [{}]: ",
                self.subject
            ),
            Constraint::Author => format!(
                "Filter (author substring, empty=clear) [{}]: ",
                self.author
            ),
            Constraint::Date => format!(
                "Filter date (today | yesterday | YYYY/MM/DD HH:MM to YYYY/MM/DD HH:MM, empty=clear) [{}]: ",
                self.date
            ),
        }
    }

    /// Set `which` from the user's answer (empty clears it). Only the date can
    /// fail: it is the only constraint with a syntax to get wrong.
    pub fn set(&mut self, which: Constraint, answer: &str) -> Result<()> {
        match which {
            Constraint::Subject => self.subject.set(answer),
            Constraint::Author => self.author.set(answer),
            Constraint::Date => self
                .date
                .set(answer)
                .map_err(|e| anyhow!("Invalid date filter: {e}"))?,
        }
        Ok(())
    }

    /// The constraints as header-ready display strings: subject, author, date.
    pub fn labels(&self) -> [String; 3] {
        [
            self.subject.to_string(),
            self.author.to_string(),
            self.date.to_string(),
        ]
    }

    /// The needles `git log` narrows an epoch by: (subject, author).
    fn search_args(&self) -> (Option<&str>, Option<&str>) {
        (
            self.subject.needle.as_deref(),
            self.author.needle.as_deref(),
        )
    }

    /// The Rust-side predicate for what the git pushdown cannot narrow.
    fn matches(&self, mail: &Mail) -> bool {
        self.date.matches(mail)
    }

    /// The loading-screen line for a scan with `count` matches so far.
    fn progress(&self, count: usize) -> String {
        format!(
            "Filtering subject='{}' author='{}' date='{}'… ({count} match{} so far)",
            self.subject,
            self.author,
            self.date,
            if count == 1 { "" } else { "es" }
        )
    }
}

/// The unfiltered mail stream: every mail across all epochs, newest-first.
/// Pages are materialized lazily by walking epochs only as far as needed, with
/// per-epoch commit hashes cached on first visit.
pub struct StreamSource {
    list_name: String,
    available_epochs: Vec<u32>,
    /// Lazy cache of commit hashes per epoch, populated on first visit.
    epoch_commits: HashMap<u32, Vec<String>>,
}

impl StreamSource {
    pub fn new(list_name: String, available_epochs: Vec<u32>) -> Self {
        Self {
            list_name,
            available_epochs,
            epoch_commits: HashMap::new(),
        }
    }

    /// Materialize the page starting at stream `offset`. Walks epochs
    /// newest-first collecting `page_size` mails, then keeps going while the
    /// page still cuts a patch series in half. Returns `NeedsClone` for the
    /// first epoch that must be cloned to make progress, or `Exhausted` past
    /// the end of the stream.
    fn status(&mut self, offset: usize, page_size: usize) -> SourceStatus {
        if self.available_epochs.is_empty() {
            return SourceStatus::Exhausted;
        }

        let mut mails: Vec<Mail> = Vec::new();
        let mut to_skip = offset;
        // The series the page ended in the middle of, once it is otherwise full.
        let mut chasing: Option<SeriesTag> = None;
        let mut eidx = self.available_epochs.len();
        'epochs: while eidx > 0 {
            eidx -= 1;
            let epoch = self.available_epochs[eidx];
            if !self.epoch_commits.contains_key(&epoch) {
                // Cloning is only worth asking about while the page proper is
                // still short; an extension chasing the tail of a series just
                // stops at the epoch edge.
                if mails.len() >= page_size {
                    break;
                }
                if !archive::repo_exists(&self.list_name, epoch) {
                    return SourceStatus::NeedsClone(epoch);
                }
                match archive::list_all_commits(&self.list_name, epoch) {
                    Ok(commits) => {
                        self.epoch_commits.insert(epoch, commits);
                    }
                    Err(_) => break,
                }
            }
            let n = self.epoch_commits[&epoch].len();
            if to_skip >= n {
                to_skip -= n;
                continue;
            }
            let mut i = to_skip;
            to_skip = 0;
            while i < n {
                if page_done(&mails, page_size, &mut chasing) {
                    break 'epochs;
                }
                // Read ahead to the next point the page could possibly end: the
                // whole shortfall while the page is still short, or a look-ahead
                // while chasing the tail of a series. Reading a batch costs about
                // one mail's worth of git, so over-reading the tail is far cheaper
                // than the process it would otherwise take to read each mail.
                let want = match page_size.checked_sub(mails.len()) {
                    Some(0) | None => CHASE_READAHEAD,
                    Some(short) => short,
                };
                let end = (i + want).min(n);
                let batch = &self.epoch_commits[&epoch][i..end];
                i = end;

                // Still append one at a time: the page ends the moment the series
                // it was cutting completes, and only a per-mail check finds that
                // boundary. Whatever of the batch is past it goes unused.
                for mail in mail::fetch(&self.list_name, epoch, batch)
                    .unwrap_or_default()
                    .into_iter()
                {
                    if page_done(&mails, page_size, &mut chasing) {
                        break 'epochs;
                    }
                    mails.push(mail);
                }
            }
        }

        if mails.is_empty() {
            return SourceStatus::Exhausted;
        }
        SourceStatus::Ready(Page::grouped(mails, offset))
    }
}

/// A background filtered scan. A worker thread streams matching mails over
/// `rx`; the owner drains them into `results` via `poll` and serves pages from
/// there. Dropping the source cancels its worker.
pub struct FilteredSource {
    list_name: String,
    filters: FilterSet,
    rx: Receiver<Mail>,
    cancel: Arc<AtomicBool>,
    results: Vec<Mail>,
    /// The current worker thread has finished scanning its epochs.
    done: bool,
    /// Epochs not present locally, newest-first, awaiting on-demand clone.
    uncloned: Vec<u32>,
}

impl FilteredSource {
    /// Start a background scan over `available_epochs` for mails matching
    /// `filters`. At least one filter should be active; an entirely inert set
    /// is allowed but pointless (caller should use the unfiltered stream
    /// instead). Epochs present locally are scanned right away; the rest are
    /// queued for on-demand cloning.
    pub fn start(list_name: String, filters: FilterSet, available_epochs: &[u32]) -> Self {
        let mut scan: Vec<u32> = Vec::new();
        let mut uncloned: Vec<u32> = Vec::new();
        for &epoch in available_epochs.iter().rev() {
            if archive::repo_exists(&list_name, epoch) {
                scan.push(epoch);
            } else {
                uncloned.push(epoch);
            }
        }
        let (rx, cancel) = spawn_worker(list_name.clone(), scan, filters.clone());
        Self {
            list_name,
            filters,
            rx,
            cancel,
            results: Vec::new(),
            done: false,
            uncloned,
        }
    }

    /// Drain the worker's channel into `results`.
    fn poll(&mut self) {
        loop {
            match self.rx.try_recv() {
                Ok(mail) => self.results.push(mail),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.done = true;
                    break;
                }
            }
        }
    }

    fn page(&self, offset: usize, page_size: usize) -> Page {
        let end = (offset + page_size).min(self.results.len());
        let mails = self
            .results
            .get(offset..end)
            .map(|s| s.to_vec())
            .unwrap_or_default();
        Page::flat(mails, offset)
    }

    /// Decide whether the page at `offset` can be served yet: ready once enough
    /// matches exist (or the worker is done), otherwise loading, or — when the
    /// scan is done and results run out — the next epoch to clone.
    fn status(&self, offset: usize, page_size: usize) -> SourceStatus {
        let needed = offset + page_size;
        let len = self.results.len();

        if len >= needed || (self.done && offset < len) {
            SourceStatus::Ready(self.page(offset, page_size))
        } else if self.done {
            match self.uncloned.first().copied() {
                Some(epoch) => SourceStatus::NeedsClone(epoch),
                None => SourceStatus::Exhausted,
            }
        } else {
            SourceStatus::Loading(self.filters.progress(len))
        }
    }

    fn discard_uncloned(&mut self, epoch: u32) {
        self.uncloned.retain(|&e| e != epoch);
    }

    /// Resume scanning over `epoch` — just cloned — so its matches are appended
    /// to the existing results.
    fn extend(&mut self, epoch: u32) {
        self.discard_uncloned(epoch);
        let (rx, cancel) = spawn_worker(self.list_name.clone(), vec![epoch], self.filters.clone());
        self.rx = rx;
        self.cancel = cancel;
        self.done = false;
    }
}

impl Drop for FilteredSource {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Spawn a worker that scans `epochs` (newest-first) and sends every mail that
/// satisfies all filters. Subject and author are pushed down into `git log`,
/// which narrows a whole epoch in about a second; only the surviving commits
/// are read and parsed, and the rest of the predicate runs on those. Stops
/// promptly when `cancel` is set or the receiver is dropped.
fn spawn_worker(
    list: String,
    epochs: Vec<u32>,
    filters: FilterSet,
) -> (Receiver<Mail>, Arc<AtomicBool>) {
    let (tx, rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_worker = cancel.clone();
    stdthread::spawn(move || {
        for epoch in epochs {
            if cancel_worker.load(Ordering::Relaxed) {
                return;
            }
            if !archive::repo_exists(&list, epoch) {
                continue;
            }
            let (subject, author) = filters.search_args();
            let Ok(commits) = archive::search_commits(&list, epoch, subject, author) else {
                continue;
            };
            // mail::read batches the git work and streams mails as they parse,
            // so results show up as they are found and a cancel lands within
            // one batch.
            for mail in mail::read(&list, epoch, &commits) {
                if cancel_worker.load(Ordering::Relaxed) {
                    return;
                }
                if filters.matches(&mail) && tx.send(mail).is_err() {
                    return;
                }
            }
        }
    });
    (rx, cancel)
}

/// Where `App` pulls mails from: either the full unfiltered stream or an active
/// background filtered scan. Both answer the same "give me page N" question
/// through `status`, so `App` can drive them with one code path.
pub enum MailSource {
    Stream(StreamSource),
    Filtered(FilteredSource),
}

impl MailSource {
    /// Advance any background work (filter worker). No-op for the stream.
    pub fn poll(&mut self) {
        if let MailSource::Filtered(f) = self {
            f.poll();
        }
    }

    /// Serve the page starting at `offset`, negotiating any missing epochs
    /// along the way. `consent` is asked once per missing epoch and answers
    /// whether the user agreed to clone it (having painted its own progress
    /// screen first — cloning blocks). Everything else stays internal: the
    /// clone itself, resuming the filter worker over a fresh epoch, and
    /// whether a refusal stops the stream or just skips the epoch.
    pub fn page(
        &mut self,
        offset: usize,
        page_size: usize,
        consent: &mut dyn FnMut(u32) -> Result<bool>,
    ) -> Result<PageState> {
        self.poll();
        loop {
            match self.status(offset, page_size) {
                SourceStatus::Ready(page) => return Ok(PageState::Ready(page)),
                SourceStatus::Loading(message) => return Ok(PageState::Pending(message)),
                SourceStatus::Exhausted => return Ok(PageState::End),
                SourceStatus::NeedsClone(epoch) => {
                    if consent(epoch)? {
                        if archive::ensure_epoch(self.list_name(), epoch).is_err() {
                            return Ok(PageState::End);
                        }
                        self.on_cloned(epoch);
                    } else if !self.decline_clone(epoch) {
                        return Ok(PageState::End);
                    }
                }
            }
        }
    }

    fn list_name(&self) -> &str {
        match self {
            MailSource::Stream(s) => &s.list_name,
            MailSource::Filtered(f) => &f.list_name,
        }
    }

    /// Ask whether the page starting at `offset` can be served yet.
    fn status(&mut self, offset: usize, page_size: usize) -> SourceStatus {
        match self {
            MailSource::Stream(s) => s.status(offset, page_size),
            MailSource::Filtered(f) => f.status(offset, page_size),
        }
    }

    /// Resume after `epoch` was just cloned. The stream picks it up on the next
    /// `status` walk; the filter restarts its worker over the new epoch.
    fn on_cloned(&mut self, epoch: u32) {
        if let MailSource::Filtered(f) = self {
            f.extend(epoch);
        }
    }

    /// Handle the user declining to clone `epoch`. Returns whether the source
    /// can still make progress: the stream stops at the missing epoch, while
    /// the filter drops it and tries the next uncloned epoch.
    fn decline_clone(&mut self, epoch: u32) -> bool {
        match self {
            MailSource::Stream(_) => false,
            MailSource::Filtered(f) => {
                f.discard_uncloned(epoch);
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lkml_core::mail::PatchTag;

    /// Patch `number/total` (v1) of the series rooted at `<root>`; `number` 0
    /// is the cover letter, so it is its own root.
    fn patch(root: &str, number: u32, total: u32) -> Mail {
        Mail {
            subject: format!("[{root} {number}/{total}]"),
            message_id: if number == 0 {
                format!("<{root}>")
            } else {
                format!("<{root}.{number}>")
            },
            references: if number == 0 {
                Vec::new()
            } else {
                vec![format!("<{root}>")]
            },
            patch_tag: Some(PatchTag {
                version: 1,
                number,
                total,
            }),
            ..Mail::default()
        }
    }

    fn plain(subject: &str) -> Mail {
        Mail {
            subject: subject.to_string(),
            ..Mail::default()
        }
    }

    fn subjects(mails: &[Mail]) -> Vec<&str> {
        mails.iter().map(|m| m.subject.as_str()).collect()
    }

    #[test]
    fn series_forms_a_block_where_its_newest_mail_sat() {
        let mails = vec![
            patch("a", 2, 3),
            plain("x"),
            patch("a", 1, 3),
            patch("a", 3, 3),
        ];
        let (out, indent) = group_series(&mails);
        assert_eq!(subjects(&out), ["[a 1/3]", "[a 2/3]", "[a 3/3]", "x"]);
        assert_eq!(indent, [false, true, true, false]);
    }

    #[test]
    fn cover_letter_heads_its_block() {
        let mails = vec![patch("a", 1, 2), patch("a", 0, 2), patch("a", 2, 2)];
        let (out, indent) = group_series(&mails);
        assert_eq!(subjects(&out), ["[a 0/2]", "[a 1/2]", "[a 2/2]"]);
        assert_eq!(indent, [false, true, true]);
    }

    #[test]
    fn two_series_group_independently() {
        let mails = vec![
            patch("a", 2, 2),
            patch("b", 2, 2),
            patch("b", 1, 2),
            patch("a", 1, 2),
        ];
        let (out, _) = group_series(&mails);
        assert_eq!(subjects(&out), ["[a 1/2]", "[a 2/2]", "[b 1/2]", "[b 2/2]"]);
    }

    #[test]
    fn stray_member_and_lone_patch_stay_put() {
        // Only 2/9 of its series is here, and a lone [PATCH 1/1] is no series:
        // nothing to pull together, nothing indented.
        let mails = vec![plain("x"), patch("s", 2, 9), patch("l", 1, 1)];
        let (out, indent) = group_series(&mails);
        assert_eq!(subjects(&out), ["x", "[s 2/9]", "[l 1/1]"]);
        assert_eq!(indent, [false, false, false]);
    }

    #[test]
    fn is_whole_ignores_missing_cover_and_other_series() {
        let mails = vec![patch("a", 1, 2), patch("b", 2, 2), patch("a", 2, 2)];
        let tag_a = thread::series_tag(&mails[0]).unwrap();
        assert!(is_whole(&mails, &tag_a)); // 1..=2 present; no cover needed
        let tag_b = thread::series_tag(&mails[1]).unwrap();
        assert!(!is_whole(&mails, &tag_b)); // b is missing 1/2
    }

    #[test]
    fn short_page_is_never_done() {
        let mut chasing = None;
        assert!(!page_done(&[plain("x")], 2, &mut chasing));
        assert!(chasing.is_none());
    }

    #[test]
    fn only_the_boundary_mail_starts_a_chase() {
        // A half-present series further up the page does not: its siblings
        // live somewhere else entirely.
        let mails = [patch("a", 1, 2), plain("x")];
        let mut chasing = None;
        assert!(page_done(&mails, 2, &mut chasing));
        assert!(chasing.is_none());
    }

    #[test]
    fn page_cutting_a_series_chases_until_it_is_whole() {
        let mut mails = vec![plain("x"), patch("a", 1, 3)];
        let mut chasing = None;
        assert!(!page_done(&mails, 2, &mut chasing));
        assert!(chasing.is_some());
        mails.push(patch("a", 2, 3));
        assert!(!page_done(&mails, 2, &mut chasing));
        mails.push(patch("a", 3, 3));
        assert!(page_done(&mails, 2, &mut chasing));
    }

    #[test]
    fn chase_gives_up_at_the_extend_cap() {
        let mut mails = vec![patch("a", 1, 99)];
        let mut chasing = None;
        assert!(!page_done(&mails, 1, &mut chasing));
        mails.resize_with(1 + SERIES_EXTEND_MAX, || plain("x"));
        assert!(page_done(&mails, 1, &mut chasing));
    }
}
