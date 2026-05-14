// SPDX-License-Identifier: GPL-2.0

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::thread;

use crate::archive;
use crate::mail::{Mail, Page, SourceStatus};

/// Spawn a worker that scans `epochs` (newest-first) and sends every mail whose
/// lowercased subject contains `needle`. Stops promptly when `cancel` is set or
/// the receiver is dropped.
fn spawn_worker(
    list: String,
    epochs: Vec<u32>,
    needle: String,
) -> (Receiver<Mail>, Arc<AtomicBool>) {
    let (tx, rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_worker = cancel.clone();
    thread::spawn(move || {
        for epoch in epochs {
            if cancel_worker.load(Ordering::Relaxed) {
                return;
            }
            let repo = archive::local_repo_path(&list, epoch);
            if !repo.exists() {
                continue;
            }
            let Ok(commits) = archive::list_all_commits(&repo) else {
                continue;
            };
            for commit in commits {
                if cancel_worker.load(Ordering::Relaxed) {
                    return;
                }
                if let Ok(raw) = archive::show_mail(&repo, &commit) {
                    let mail = archive::parse_mail_from_raw(&raw, epoch, commit);
                    if mail.title.to_lowercase().contains(&needle) && tx.send(mail).is_err() {
                        return;
                    }
                }
            }
        }
    });
    (rx, cancel)
}

/// A background subject-filter scan. A worker thread streams matching mails over
/// `rx`; the owner drains them into `results` via `poll` and serves pages from
/// there. Dropping the filter cancels its worker.
pub struct SubjectFilter {
    list_name: String,
    /// Original filter text, shown verbatim in loading messages.
    display: String,
    /// Trimmed, lowercased substring the worker matches against.
    needle: String,
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

impl SubjectFilter {
    /// Start a background scan for `filter_text` over `available_epochs`.
    /// Epochs present locally are scanned right away; the rest are queued for
    /// on-demand cloning.
    pub fn start(list_name: String, filter_text: String, available_epochs: &[u32]) -> Self {
        let needle = filter_text.trim().to_lowercase();
        let mut scan: Vec<u32> = Vec::new();
        let mut uncloned: Vec<u32> = Vec::new();
        for &epoch in available_epochs.iter().rev() {
            if archive::repo_exists(&list_name, epoch) {
                scan.push(epoch);
            } else {
                uncloned.push(epoch);
            }
        }
        let (rx, cancel) = spawn_worker(list_name.clone(), scan, needle.clone());
        Self {
            list_name,
            display: filter_text,
            needle,
            rx,
            cancel,
            results: Vec::new(),
            done: false,
            uncloned,
            pending_page: Some(0),
        }
    }

    /// The page index currently awaited, if any.
    pub fn pending_page(&self) -> Option<usize> {
        self.pending_page
    }

    /// Mark `page_idx` as the page to serve once enough matches arrive.
    pub fn request_page(&mut self, page_idx: usize) {
        self.pending_page = Some(page_idx);
    }

    /// Clear the awaited page — it has been served or abandoned.
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

    /// Materialize page `page_idx` from the accumulated results.
    pub fn page(&self, page_idx: usize, page_size: usize) -> Page {
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
                "Filtering '{}'… ({} match{} so far)",
                self.display,
                len,
                if len == 1 { "" } else { "es" }
            ))
        }
    }

    /// Drop `epoch` from the uncloned queue without scanning it (e.g. the user
    /// declined to clone it).
    pub fn discard_uncloned(&mut self, epoch: u32) {
        self.uncloned.retain(|&e| e != epoch);
    }

    /// Resume scanning over `epoch` — just cloned — so its matches are appended
    /// to the existing results.
    pub fn extend(&mut self, epoch: u32) {
        self.discard_uncloned(epoch);
        let (rx, cancel) = spawn_worker(self.list_name.clone(), vec![epoch], self.needle.clone());
        self.rx = rx;
        self.cancel = cancel;
        self.done = false;
    }
}

impl Drop for SubjectFilter {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}
