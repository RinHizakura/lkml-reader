// SPDX-License-Identifier: GPL-2.0

//! Where the TUI pulls mails from. Paging, lazy epoch walking and background
//! filtering are concepts that only the interactive reader has, so they live
//! here rather than in the shared `lkml-core` library, which stays about mail
//! parsing and archive I/O.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::thread;

use anyhow::Result;

use lkml_core::archive;
use lkml_core::filter::{DateFilter, Filter, NameFilter};
use lkml_core::mail::{self, Mail};

/// A single screen of mails. Renders as one TUI page.
#[derive(Clone, Default)]
pub struct Page {
    pub mails: Vec<Mail>,
    pub page_idx: usize,
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
    pub fn new(mails: Vec<Mail>, page_idx: usize) -> Self {
        Self { mails, page_idx }
    }

    pub fn is_empty(&self) -> bool {
        self.mails.is_empty()
    }

    pub fn len(&self) -> usize {
        self.mails.len()
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

    /// Materialize page `page_idx`. Walks epochs newest-first to collect a
    /// page-worth of commits; returns `NeedsClone` for the first epoch that
    /// must be cloned to make progress, or `Exhausted` past the end of the
    /// stream.
    pub fn status(&mut self, page_idx: usize, page_size: usize) -> Result<SourceStatus> {
        if self.available_epochs.is_empty() {
            return Ok(SourceStatus::Exhausted);
        }

        let mut need = page_size;
        let mut to_skip = page_idx * page_size;
        let mut items: Vec<(u32, String)> = Vec::new();
        let mut eidx = self.available_epochs.len() - 1;
        loop {
            if need == 0 {
                break;
            }
            let epoch = self.available_epochs[eidx];
            if !self.epoch_commits.contains_key(&epoch) {
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
            let commits = &self.epoch_commits[&epoch];
            let n = commits.len();
            if to_skip >= n {
                to_skip -= n;
            } else {
                let start = to_skip;
                let end = (start + need).min(n);
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

        if items.is_empty() {
            return Ok(SourceStatus::Exhausted);
        }
        let mut mails = Vec::with_capacity(items.len());
        for (epoch, commit) in items {
            if let Ok(mail) = mail::fetch(&self.list_name, epoch, &commit) {
                mails.push(mail);
            }
        }
        if mails.is_empty() {
            return Ok(SourceStatus::Exhausted);
        }
        Ok(SourceStatus::Ready(Page::new(mails, page_idx)))
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
    /// Page index awaited while the worker collects enough matches.
    pending_page: Option<usize>,
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
            pending_page: Some(0),
        }
    }

    pub fn pending_page(&self) -> Option<usize> {
        self.pending_page
    }

    pub fn request_page(&mut self, page_idx: usize) {
        self.pending_page = Some(page_idx);
    }

    pub fn clear_pending(&mut self) {
        self.pending_page = None;
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

    fn page(&self, page_idx: usize, page_size: usize) -> Page {
        let start = page_idx * page_size;
        let end = (start + page_size).min(self.results.len());
        let mails = self
            .results
            .get(start..end)
            .map(|s| s.to_vec())
            .unwrap_or_default();
        Page::new(mails, page_idx)
    }

    /// Decide whether `page_idx` can be served yet: ready once enough matches
    /// exist (or the worker is done), otherwise loading, or — when the scan is
    /// done and results run out — the next epoch to clone.
    pub fn status(&self, page_idx: usize, page_size: usize) -> SourceStatus {
        let start = page_idx * page_size;
        let needed = start + page_size;
        let len = self.results.len();

        if len >= needed || (self.done && start < len) {
            SourceStatus::Ready(self.page(page_idx, page_size))
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
/// satisfies all filters. Stops promptly when `cancel` is set or the receiver
/// is dropped.
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
    thread::spawn(move || {
        for epoch in epochs {
            if cancel_worker.load(Ordering::Relaxed) {
                return;
            }
            if !archive::repo_exists(&list, epoch) {
                continue;
            }
            let Ok(commits) = archive::list_all_commits(&list, epoch) else {
                continue;
            };
            for commit in commits {
                if cancel_worker.load(Ordering::Relaxed) {
                    return;
                }
                if let Ok(mail) = mail::fetch(&list, epoch, &commit) {
                    if subject.matches(&mail)
                        && author.matches(&mail)
                        && date.matches(&mail)
                        && tx.send(mail).is_err()
                    {
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

    /// The page index awaited across run-loop ticks. The stream resolves
    /// synchronously, so it is never pending.
    pub fn pending_page(&self) -> Option<usize> {
        match self {
            MailSource::Stream(_) => None,
            MailSource::Filtered(f) => f.pending_page(),
        }
    }

    /// Mark `idx` as the page to serve. No-op for the stream.
    pub fn request_page(&mut self, idx: usize) {
        if let MailSource::Filtered(f) = self {
            f.request_page(idx);
        }
    }

    /// Clear the awaited page. No-op for the stream.
    pub fn clear_pending(&mut self) {
        if let MailSource::Filtered(f) = self {
            f.clear_pending();
        }
    }

    /// Ask whether page `page_idx` can be served yet.
    pub fn status(&mut self, page_idx: usize, page_size: usize) -> Result<SourceStatus> {
        match self {
            MailSource::Stream(s) => s.status(page_idx, page_size),
            MailSource::Filtered(f) => Ok(f.status(page_idx, page_size)),
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
