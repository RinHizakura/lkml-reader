// SPDX-License-Identifier: GPL-2.0

use std::collections::HashMap;

use anyhow::Result;

use crate::archive;
use crate::filter::SubjectFilter;
use crate::mail::{Page, SourceStatus};

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
                let repo = archive::local_repo_path(&self.list_name, epoch);
                match archive::list_all_commits(&repo) {
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
            let repo = archive::local_repo_path(&self.list_name, epoch);
            if let Ok(raw) = archive::show_mail(&repo, &commit) {
                mails.push(archive::parse_mail_from_raw(&raw, epoch, commit));
            }
        }
        if mails.is_empty() {
            return Ok(SourceStatus::Exhausted);
        }
        Ok(SourceStatus::Ready(Page::new(mails, page_idx)))
    }
}

/// Where `App` pulls mails from: either the full unfiltered stream or an active
/// background subject filter. Both answer the same "give me page N" question
/// through `status`, so `App` can drive them with one code path.
pub enum MailSource {
    Stream(StreamSource),
    Filtered(SubjectFilter),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `linux-pm` epoch 0 is the local mirror used as a test fixture. Tests
    /// that need it skip gracefully when it has not been cloned.
    fn fixture_stream() -> Option<StreamSource> {
        archive::repo_exists("linux-pm", 0)
            .then(|| StreamSource::new("linux-pm".to_string(), vec![0]))
    }

    #[test]
    fn stream_pages_are_sequential_and_non_overlapping() {
        let Some(mut src) = fixture_stream() else {
            eprintln!("skipping: linux-pm/0 mirror not present");
            return;
        };
        let page_size = 10;

        let p0 = match src.status(0, page_size).unwrap() {
            SourceStatus::Ready(p) => p,
            _ => panic!("page 0 should be Ready"),
        };
        assert_eq!(p0.page_idx, 0);
        assert_eq!(p0.len(), page_size);

        let p1 = match src.status(1, page_size).unwrap() {
            SourceStatus::Ready(p) => p,
            _ => panic!("page 1 should be Ready"),
        };
        assert_eq!(p1.page_idx, 1);
        assert_eq!(p1.len(), page_size);

        let p0_commits: Vec<&String> = p0.mails.iter().map(|m| &m.commit).collect();
        for m in &p1.mails {
            assert!(!p0_commits.contains(&&m.commit), "page 1 overlaps page 0");
        }
    }

    #[test]
    fn stream_past_end_is_exhausted() {
        let Some(mut src) = fixture_stream() else {
            eprintln!("skipping: linux-pm/0 mirror not present");
            return;
        };
        let status = src.status(10_000_000, 10).unwrap();
        assert!(matches!(status, SourceStatus::Exhausted));
    }

    #[test]
    fn stream_with_no_epochs_is_exhausted() {
        let mut src = StreamSource::new("nonexistent-list".to_string(), vec![]);
        assert!(matches!(src.status(0, 10).unwrap(), SourceStatus::Exhausted));
    }

    #[test]
    fn stream_needs_clone_for_uncloned_epoch() {
        // Epoch 0 of `linux-pm` is cloned, but a high epoch number is not, so
        // a stream over only that uncloned epoch must ask to clone it.
        let mut src = StreamSource::new("linux-pm".to_string(), vec![9999]);
        assert!(matches!(
            src.status(0, 10).unwrap(),
            SourceStatus::NeedsClone(9999)
        ));
    }
}
