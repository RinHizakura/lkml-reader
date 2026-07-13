// SPDX-License-Identifier: GPL-2.0

//! Where the TUI pulls mails from. Paging, lazy epoch walking and background
//! filtering are concepts that only the interactive reader has, so they live
//! here rather than in the shared `lkml-core` library, which stays about mail
//! parsing and archive I/O.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::thread as stdthread;

use anyhow::Result;

use lkml_core::archive;
use lkml_core::filter::{DateFilter, Filter, NameFilter};
use lkml_core::mail::{self, Mail};
use lkml_core::thread::{self, SeriesTag};

/// How far past `page_size` a page may grow to finish a patch series that
/// straddles its tail. Bounds the walk when a series is only partly in the
/// archive and so would never complete.
const SERIES_EXTEND_MAX: usize = 64;

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
    pub indent: Vec<bool>,
    pub offset: usize,
}

/// Outcome of asking a mail source whether a given page can be served yet.
pub enum SourceStatus {
    /// The page is ready.
    Ready(Page),
    /// Still working; show this loading message.
    Loading(String),
    /// Progress is blocked until this epoch is cloned.
    NeedsClone(u32),
    /// No more mails to show.
    Exhausted,
}

impl Page {
    /// A page of the unfiltered stream: patch series pulled into blocks and
    /// marked for indentation.
    pub fn grouped(mails: Vec<Mail>, offset: usize) -> Self {
        let (mails, indent) = group_series(mails);
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
}

/// Reorder `mails` so every patch series forms one block — cover letter (or
/// lowest-numbered patch) first, the rest ascending — sitting where the series'
/// newest mail was, and flag the members that belong under the head.
fn group_series(mails: Vec<Mail>) -> (Vec<Mail>, Vec<bool>) {
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

    let mut out = Vec::with_capacity(tags.len());
    let mut indent = Vec::with_capacity(tags.len());
    let mut pool: Vec<Option<Mail>> = mails.into_iter().map(Some).collect();
    for i in 0..pool.len() {
        if pool[i].is_none() {
            continue;
        }
        match tags[i].as_ref().and_then(|tag| series.get(tag)) {
            // A lone member is left where it is: nothing to indent it under.
            Some(members) if members.len() > 1 => {
                for (nth, &j) in members.iter().enumerate() {
                    if let Some(mail) = pool[j].take() {
                        out.push(mail);
                        indent.push(nth > 0);
                    }
                }
            }
            _ => {
                out.extend(pool[i].take());
                indent.push(false);
            }
        }
    }
    (out, indent)
}

/// Has the walk collected everything the page needs? Up to `page_size` it never
/// has. At that point the page either ends cleanly and is done, or ends
/// mid-series, and `wanted` picks up the chase for the patches still to come —
/// bounded by [`SERIES_EXTEND_MAX`], since a series only partly in the archive
/// would otherwise drag the whole epoch in.
fn page_done(
    mails: &[Mail],
    page_size: usize,
    wanted: &mut Option<(SeriesTag, HashSet<u32>)>,
) -> bool {
    match wanted {
        Some((_, missing)) => missing.is_empty() || mails.len() >= page_size + SERIES_EXTEND_MAX,
        None if mails.len() >= page_size => {
            *wanted = cut_at_tail(mails);
            wanted.is_none()
        }
        None => false,
    }
}

/// Cross a just-fetched mail off the patches the chase is waiting for.
fn cross_off(wanted: &mut Option<(SeriesTag, HashSet<u32>)>, mail: &Mail) {
    let Some((tag, missing)) = wanted else { return };
    if thread::series_tag(mail).as_ref() == Some(tag) {
        if let Some(patch) = mail.patch_tag {
            missing.remove(&patch.number);
        }
    }
}

/// The series the page's last mail belongs to and the patches of it still
/// missing, or `None` when the page does not end mid-series.
///
/// Only the mail at the boundary counts. A series that looks half-present
/// further up the page is a mail whose siblings live somewhere else entirely —
/// an old patch resent, a stray `2/9` — and chasing every one of those drags in
/// mails that cut yet more series, page after page.
fn cut_at_tail(mails: &[Mail]) -> Option<(SeriesTag, HashSet<u32>)> {
    let tag = thread::series_tag(mails.last()?)?;
    let seen: HashSet<u32> = mails
        .iter()
        .filter(|mail| thread::series_tag(mail).as_ref() == Some(&tag))
        .filter_map(|mail| mail.patch_tag.map(|patch| patch.number))
        .collect();
    // The 0/m cover letter is optional; 1/m..m/m are not.
    let missing: HashSet<u32> = (1..=tag.total).filter(|n| !seen.contains(n)).collect();
    (!missing.is_empty()).then_some((tag, missing))
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
    pub fn status(&mut self, offset: usize, page_size: usize) -> Result<SourceStatus> {
        if self.available_epochs.is_empty() {
            return Ok(SourceStatus::Exhausted);
        }

        let mut mails: Vec<Mail> = Vec::new();
        let mut to_skip = offset;
        // The series the full page ended in the middle of, and the patches of it
        // still to come. `None` until the page is full; empty once the series is
        // whole and the page is done.
        let mut wanted: Option<(SeriesTag, HashSet<u32>)> = None;
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
                    return Ok(SourceStatus::NeedsClone(epoch));
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
            for i in to_skip..n {
                if page_done(&mails, page_size, &mut wanted) {
                    break 'epochs;
                }
                let commit = self.epoch_commits[&epoch][i].clone();
                if let Ok(mail) = mail::fetch(&self.list_name, epoch, &commit) {
                    cross_off(&mut wanted, &mail);
                    mails.push(mail);
                }
            }
            to_skip = 0;
        }

        if mails.is_empty() {
            return Ok(SourceStatus::Exhausted);
        }
        Ok(SourceStatus::Ready(Page::grouped(mails, offset)))
    }
}

/// A background filtered scan. A worker thread streams matching mails over
/// `rx`; the owner drains them into `results` via `poll` and serves pages from
/// there. Dropping the source cancels its worker.
pub struct FilteredSource {
    list_name: String,
    subject: NameFilter,
    author: NameFilter,
    date: DateFilter,
    rx: Receiver<Mail>,
    cancel: Arc<AtomicBool>,
    results: Vec<Mail>,
    /// The current worker thread has finished scanning its epochs.
    done: bool,
    /// Epochs not present locally, newest-first, awaiting on-demand clone.
    uncloned: Vec<u32>,
    /// Start offset of the page awaited while the worker collects enough matches.
    pending_offset: Option<usize>,
}

impl FilteredSource {
    /// Start a background scan over `available_epochs` for mails matching the
    /// given filters. At least one filter should be active; an entirely inert
    /// set is allowed but pointless (caller should use the unfiltered stream
    /// instead). Epochs present locally are scanned right away; the rest are
    /// queued for on-demand cloning.
    pub fn start(
        list_name: String,
        subject: NameFilter,
        author: NameFilter,
        date: DateFilter,
        available_epochs: &[u32],
    ) -> Self {
        let mut scan: Vec<u32> = Vec::new();
        let mut uncloned: Vec<u32> = Vec::new();
        for &epoch in available_epochs.iter().rev() {
            if archive::repo_exists(&list_name, epoch) {
                scan.push(epoch);
            } else {
                uncloned.push(epoch);
            }
        }
        let (rx, cancel) = spawn_worker(
            list_name.clone(),
            scan,
            subject.clone(),
            author.clone(),
            date.clone(),
        );
        Self {
            list_name,
            subject,
            author,
            date,
            rx,
            cancel,
            results: Vec::new(),
            done: false,
            uncloned,
            pending_offset: Some(0),
        }
    }

    pub fn pending_offset(&self) -> Option<usize> {
        self.pending_offset
    }

    pub fn request_offset(&mut self, offset: usize) {
        self.pending_offset = Some(offset);
    }

    pub fn clear_pending(&mut self) {
        self.pending_offset = None;
    }

    /// Drain the worker's channel into `results`.
    pub fn poll(&mut self) {
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
    pub fn status(&self, offset: usize, page_size: usize) -> SourceStatus {
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
            SourceStatus::Loading(format!(
                "Filtering subject='{}' author='{}' date='{}'… ({} match{} so far)",
                self.subject,
                self.author,
                self.date,
                len,
                if len == 1 { "" } else { "es" }
            ))
        }
    }

    pub fn discard_uncloned(&mut self, epoch: u32) {
        self.uncloned.retain(|&e| e != epoch);
    }

    /// Resume scanning over `epoch` — just cloned — so its matches are appended
    /// to the existing results.
    pub fn extend(&mut self, epoch: u32) {
        self.discard_uncloned(epoch);
        let (rx, cancel) = spawn_worker(
            self.list_name.clone(),
            vec![epoch],
            self.subject.clone(),
            self.author.clone(),
            self.date.clone(),
        );
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
/// are read and parsed, and the date filter runs on those. Stops promptly when
/// `cancel` is set or the receiver is dropped.
fn spawn_worker(
    list: String,
    epochs: Vec<u32>,
    subject: NameFilter,
    author: NameFilter,
    date: DateFilter,
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
            let Ok(commits) = archive::search_commits(
                &list,
                epoch,
                subject.needle.as_deref(),
                author.needle.as_deref(),
            ) else {
                continue;
            };
            for commit in commits {
                if cancel_worker.load(Ordering::Relaxed) {
                    return;
                }
                if let Ok(mail) = mail::fetch(&list, epoch, &commit) {
                    if date.matches(&mail) && tx.send(mail).is_err() {
                        return;
                    }
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

    /// The offset of the page awaited across run-loop ticks. The stream
    /// resolves synchronously, so it is never pending.
    pub fn pending_offset(&self) -> Option<usize> {
        match self {
            MailSource::Stream(_) => None,
            MailSource::Filtered(f) => f.pending_offset(),
        }
    }

    /// Mark the page at `offset` as the one to serve. No-op for the stream.
    pub fn request_offset(&mut self, offset: usize) {
        if let MailSource::Filtered(f) = self {
            f.request_offset(offset);
        }
    }

    /// Clear the awaited page. No-op for the stream.
    pub fn clear_pending(&mut self) {
        if let MailSource::Filtered(f) = self {
            f.clear_pending();
        }
    }

    /// Ask whether the page starting at `offset` can be served yet.
    pub fn status(&mut self, offset: usize, page_size: usize) -> Result<SourceStatus> {
        match self {
            MailSource::Stream(s) => s.status(offset, page_size),
            MailSource::Filtered(f) => Ok(f.status(offset, page_size)),
        }
    }

    /// Resume after `epoch` was just cloned. The stream picks it up on the next
    /// `status` walk; the filter restarts its worker over the new epoch.
    pub fn on_cloned(&mut self, epoch: u32) {
        if let MailSource::Filtered(f) = self {
            f.extend(epoch);
        }
    }

    /// Handle the user declining to clone `epoch`. Returns whether the source
    /// can still make progress: the stream stops at the missing epoch, while
    /// the filter drops it and tries the next uncloned epoch.
    pub fn decline_clone(&mut self, epoch: u32) -> bool {
        match self {
            MailSource::Stream(_) => false,
            MailSource::Filtered(f) => {
                f.discard_uncloned(epoch);
                true
            }
        }
    }
}
