// SPDX-License-Identifier: GPL-2.0

use chrono::{DateTime, FixedOffset};

#[derive(Debug, Clone)]
pub struct Mail {
    pub title: String,
    pub author: String,
    pub date: Option<DateTime<FixedOffset>>,
    pub epoch: u32,
    pub commit: String,
}

impl Mail {
    pub fn date_str(&self) -> String {
        match &self.date {
            Some(d) => d.format("%Y/%m/%d %H:%M").to_string(),
            None => "-".to_string(),
        }
    }
}

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
