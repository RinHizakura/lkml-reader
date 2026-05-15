// SPDX-License-Identifier: GPL-2.0

use anyhow::Result;
use chrono::{DateTime, FixedOffset, Local, Utc};
use std::fmt;

use crate::parse;

/// Half-open date range `[start, end)` stored in UTC.
#[derive(Clone, Debug)]
pub struct DateRange {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

impl fmt::Display for DateRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} to {}",
            self.start.with_timezone(&Local).format("%Y/%m/%d %H:%M"),
            self.end.with_timezone(&Local).format("%Y/%m/%d %H:%M"),
        )
    }
}

/// Subject-substring constraint. Empty (`None`) means "no constraint" — every
/// subject matches.
#[derive(Clone, Debug, Default)]
pub struct SubjectFilter {
    pub needle: Option<String>,
}

impl SubjectFilter {
    pub fn new() -> Self {
        Self { needle: None }
    }

    /// Replace the needle from raw user text. Empty text clears the filter.
    pub fn set(&mut self, text: &str) {
        let trimmed = text.trim();
        self.needle = if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        };
    }

    pub fn is_active(&self) -> bool {
        self.needle.is_some()
    }

    pub fn matches(&self, subject: &str) -> bool {
        match &self.needle {
            None => true,
            Some(n) => subject.to_lowercase().contains(&n.to_lowercase()),
        }
    }
}

impl fmt::Display for SubjectFilter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.needle {
            None => f.write_str("(none)"),
            Some(n) => f.write_str(n),
        }
    }
}

/// Date-range constraint. Empty (`None`) means "no constraint" — every mail
/// matches regardless of its `Date` header.
#[derive(Clone, Debug, Default)]
pub struct DateFilter {
    pub date_range: Option<DateRange>,
}

impl DateFilter {
    pub fn new() -> Self {
        Self { date_range: None }
    }

    /// Replace the range from raw user text. Accepts:
    ///   - `today`
    ///   - `yesterday`
    ///   - `YYYY/MM/DD HH:MM to YYYY/MM/DD HH:MM`
    ///
    /// Empty text clears the filter; malformed text returns an error and
    /// leaves the existing filter unchanged.
    pub fn set(&mut self, text: &str) -> Result<()> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            self.date_range = None;
            return Ok(());
        }
        let (start, end) = parse::parse_date_range(trimmed)?;
        self.date_range = Some(DateRange { start, end });
        Ok(())
    }

    pub fn is_active(&self) -> bool {
        self.date_range.is_some()
    }

    pub fn matches(&self, date: Option<&DateTime<FixedOffset>>) -> bool {
        let Some(range) = &self.date_range else {
            return true;
        };
        let Some(date) = date else {
            return false;
        };
        let utc = date.with_timezone(&Utc);
        utc >= range.start && utc < range.end
    }
}

impl fmt::Display for DateFilter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.date_range {
            None => f.write_str("(none)"),
            Some(r) => write!(f, "{}", r),
        }
    }
}

