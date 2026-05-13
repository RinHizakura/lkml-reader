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
}

impl Page {
    pub fn new(mails: Vec<Mail>) -> Self {
        Self { mails }
    }

    pub fn is_empty(&self) -> bool {
        self.mails.is_empty()
    }

    pub fn len(&self) -> usize {
        self.mails.len()
    }
}
