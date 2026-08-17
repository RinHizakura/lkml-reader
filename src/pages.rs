// SPDX-License-Identifier: GPL-2.0

//! Pagination state: which page of the stream is showing, where every visited
//! page starts, which row is selected and how far the window has scrolled.
//! Stream offsets never leave this module — callers navigate with
//! `next_target`/`prev_target`/`reset` and read the current page — so the
//! boundary bookkeeping, the reset rules and the scroll-follow math live (and
//! are tested) in one place.

use crate::source::Page;
use lkml_core::mail::Mail;

pub struct Pages {
    /// Rows a page aims to fill. A page can outgrow it (a long series is never
    /// split), in which case the window scrolls inside the page.
    page_size: usize,
    current: Page,
    /// Where every page visited so far starts in the stream, ascending. Pages
    /// are variable-length, so stepping back is only possible because we
    /// remember where the earlier ones started.
    offsets: Vec<usize>,
    /// Where the page we are trying to show starts, while the source is still
    /// working on it. `None` once it has been served (or given up on).
    pending: Option<usize>,
    selected: usize,
    /// First row of the current page shown on screen. Non-zero only when the
    /// page is taller than the window.
    scroll: usize,
}

impl Pages {
    pub fn new(page_size: usize) -> Self {
        Self {
            page_size,
            current: Page::default(),
            offsets: vec![0],
            pending: None,
            selected: 0,
            scroll: 0,
        }
    }

    pub fn current(&self) -> &Page {
        &self.current
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn scroll(&self) -> usize {
        self.scroll
    }

    pub fn page_size(&self) -> usize {
        self.page_size
    }

    pub fn pending(&self) -> Option<usize> {
        self.pending
    }

    /// The mail under the cursor, if the page has one.
    pub fn selected_mail(&self) -> Option<&Mail> {
        self.current.mails.get(self.selected)
    }

    /// The 1-based page number: how many boundaries were crossed to get here.
    pub fn label(&self) -> String {
        self.offsets
            .iter()
            .position(|&s| s == self.current.offset)
            .map_or(1, |pos| pos + 1)
            .to_string()
    }

    /// Start over at page 0 (source swap). Everything resets — page, history,
    /// selection, scroll and any pending target — and 0 is the page to serve.
    pub fn reset(&mut self) -> usize {
        *self = Self::new(self.page_size);
        0
    }

    /// The page at `target` is still being worked on; the poll tick retries it.
    pub fn begin(&mut self, target: usize) {
        self.pending = Some(target);
    }

    /// The source served a page: nothing is pending anymore, and its start is
    /// a boundary worth remembering — recorded only now, so a target that was
    /// never served leaves no phantom behind. A new page shows from the top;
    /// the current page re-served (a resize's re-cut) keeps the selection,
    /// clamped to what still exists and scrolled back into view.
    pub fn accept(&mut self, page: Page) {
        // Offsets ascend, so a boundary we have already crossed is at or
        // before the end; only a brand new one goes past it.
        if self.offsets.last() < Some(&page.offset) {
            self.offsets.push(page.offset);
        }
        if page.offset != self.current.offset {
            self.selected = 0;
            self.scroll = 0;
        } else {
            self.selected = self.selected.min(page.len().saturating_sub(1));
            self.scroll = self.scroll.min(self.selected);
            if self.selected + 1 > self.scroll + self.page_size {
                self.scroll = self.selected + 1 - self.page_size;
            }
        }
        self.current = page;
        self.pending = None;
    }

    /// Serving was given up on (the stream ended); nothing is pending anymore.
    pub fn settle(&mut self) {
        self.pending = None;
    }

    /// Where the next page starts — only known now that the current one has
    /// been served. `None` while the current page is empty.
    pub fn next_target(&self) -> Option<usize> {
        if self.current.is_empty() {
            return None;
        }
        Some(self.current.offset + self.current.len())
    }

    /// Where the previous page starts; `None` on the first page.
    pub fn prev_target(&self) -> Option<usize> {
        self.offsets
            .iter()
            .position(|&s| s == self.current.offset)
            .filter(|&pos| pos > 0)
            .map(|pos| self.offsets[pos - 1])
    }

    /// The window changed height: every boundary after the current page was
    /// cut for the old size and is now wrong, so forget them. Returns the
    /// current page's start, for the caller to re-serve.
    pub fn resize(&mut self, page_size: usize) -> usize {
        self.page_size = page_size;
        let offset = self.current.offset;
        self.offsets.retain(|&s| s <= offset);
        offset
    }

    /// Move the selection down one row, scrolling the window along when the
    /// page outgrows it. Returns whether the selection moved.
    pub fn select_next(&mut self) -> bool {
        if self.selected + 1 >= self.current.len() {
            return false;
        }
        self.selected += 1;
        if self.selected >= self.scroll + self.page_size {
            self.scroll = self.selected + 1 - self.page_size;
        }
        true
    }

    /// Move the selection up one row, scrolling the window along when needed.
    /// Returns whether the selection moved.
    pub fn select_prev(&mut self) -> bool {
        if self.selected == 0 {
            return false;
        }
        self.selected -= 1;
        self.scroll = self.scroll.min(self.selected);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(offset: usize, len: usize) -> Page {
        Page::flat(vec![Mail::default(); len], offset)
    }

    #[test]
    fn boundaries_are_remembered_and_never_duplicated() {
        let mut p = Pages::new(5);
        p.accept(page(0, 5));
        assert_eq!(p.next_target(), Some(5));
        p.accept(page(5, 7)); // grew past page_size to finish a series
        assert_eq!(p.label(), "2");
        assert_eq!(p.next_target(), Some(12));
        p.accept(page(12, 5));
        assert_eq!(p.prev_target(), Some(5));
        p.accept(page(5, 7));
        // Crossing a known boundary again must not record it twice.
        assert_eq!(p.next_target(), Some(12));
        assert_eq!(p.prev_target(), Some(0));
        p.accept(page(0, 5));
        assert_eq!(p.prev_target(), None);
        assert_eq!(p.label(), "1");
    }

    #[test]
    fn empty_page_has_no_next() {
        let p = Pages::new(5);
        assert_eq!(p.next_target(), None);
    }

    #[test]
    fn reset_starts_over_at_page_zero() {
        let mut p = Pages::new(5);
        p.accept(page(0, 5));
        p.next_target();
        p.begin(5);
        assert_eq!(p.pending(), Some(5));
        assert_eq!(p.reset(), 0);
        assert_eq!(p.pending(), None);
        assert!(p.current().is_empty());
        assert_eq!(p.prev_target(), None);
    }

    #[test]
    fn resize_forgets_boundaries_cut_for_the_old_height() {
        let mut p = Pages::new(5);
        p.accept(page(0, 5));
        p.accept(page(5, 5));
        p.accept(page(10, 5)); // visited, so 10 is a recorded boundary
        p.accept(page(5, 5)); // back one page
        assert_eq!(p.resize(3), 5); // re-serve the current page…
        p.accept(page(5, 3));
        assert_eq!(p.prev_target(), Some(0)); // …the way back survives
        assert_eq!(p.next_target(), Some(8)); // …the way forward is recut
    }

    #[test]
    fn unserved_targets_leave_no_boundary() {
        let mut p = Pages::new(5);
        p.accept(page(0, 5));
        let target = p.next_target().unwrap();
        p.begin(target);
        assert_eq!(p.pending(), Some(target));
        p.settle(); // the stream had nothing there
        assert_eq!(p.pending(), None);
        // Still page 1 of 1: the failed target recorded nothing.
        assert_eq!(p.label(), "1");
        assert_eq!(p.prev_target(), None);
    }

    #[test]
    fn reserving_the_same_page_keeps_the_selection_in_view() {
        let mut p = Pages::new(5);
        p.accept(page(0, 5));
        for _ in 0..4 {
            p.select_next();
        }
        let target = p.resize(2);
        p.accept(page(target, 3)); // re-cut shorter for the new height
        assert_eq!(p.selected(), 2); // clamped to the last remaining row
        assert_eq!(p.scroll(), 1); // and scrolled into the 2-row window
        p.accept(page(3, 5)); // a different page still shows from the top
        assert_eq!((p.selected(), p.scroll()), (0, 0));
    }

    #[test]
    fn selection_scrolls_the_window_along() {
        let mut p = Pages::new(2);
        p.accept(page(0, 4)); // page taller than the window
        assert!(!p.select_prev());
        assert!(p.select_next());
        assert_eq!((p.selected(), p.scroll()), (1, 0));
        assert!(p.select_next());
        assert_eq!((p.selected(), p.scroll()), (2, 1)); // window followed down
        assert!(p.select_next());
        assert!(!p.select_next()); // bottom row
        assert_eq!((p.selected(), p.scroll()), (3, 2));
        while p.select_prev() {}
        assert_eq!((p.selected(), p.scroll()), (0, 0)); // and back up
    }

    #[test]
    fn accept_shows_the_new_page_from_the_top() {
        let mut p = Pages::new(2);
        p.accept(page(0, 4));
        p.select_next();
        p.select_next();
        p.accept(page(4, 4));
        assert_eq!((p.selected(), p.scroll()), (0, 0));
    }
}
