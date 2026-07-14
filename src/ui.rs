// SPDX-License-Identifier: GPL-2.0

use anyhow::Result;
use crossterm::{
    cursor::{Hide, MoveTo, Show},
    execute, queue,
    style::{
        Attribute, Color, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
    },
    terminal::{size, Clear, ClearType},
};
use std::io::Write;

use lkml_core::filter::{DateFilter, NameFilter};
use lkml_core::mail::Mail;

#[derive(Clone, Copy)]
pub struct HeaderInfo<'a> {
    pub list_name: &'a str,
    pub epoch_label: &'a str,
    pub page_label: &'a str,
    pub subject_filter: &'a NameFilter,
    pub author_filter: &'a NameFilter,
    pub date_filter: &'a DateFilter,
}

pub struct ListView<'a> {
    pub header: HeaderInfo<'a>,
    /// Offset of the page's first mail in the stream; row numbering starts here.
    pub offset: usize,
    pub mails: &'a [Mail],
    /// Per row: draw it indented under the series head above it.
    pub indent: &'a [bool],
    pub selected: usize,
    /// First row shown. A page that grew to hold a whole patch series can be
    /// taller than the window, so the list scrolls within the page.
    pub scroll: usize,
    /// Tick counter driving the marquee scroll on the selected row's title.
    pub selected_scroll: usize,
    pub empty_message: &'a [String],
}

/// What a patch hangs under its series head by.
const INDENT: &str = "  ↳ ";
/// List row layout: `%Y/%m/%d %H:%M` fits in 16, a display name in 24.
const DATE_W: usize = 16;
const AUTHOR_W: usize = 24;

/// Paint a whole screen: header bar, `body` in the space between, hotkey bar.
/// `body` is handed the width and the first row past the content area.
fn draw_frame<W: Write, F>(out: &mut W, header: &HeaderInfo, hint: &str, body: F) -> Result<()>
where
    F: FnOnce(&mut W, u16, u16) -> Result<()>,
{
    let (cols, rows) = size()?;
    queue!(out, Hide, Clear(ClearType::All))?;
    draw_header(out, header, cols)?;
    body(out, cols, rows.saturating_sub(1))?;
    draw_hotkeys(out, hint, cols, rows)?;
    out.flush()?;
    Ok(())
}

pub fn draw_loading<W: Write>(out: &mut W, header: &HeaderInfo, message: &str) -> Result<()> {
    draw_frame(out, header, "", |out, cols, bottom| {
        draw_centered(out, &format!("⏳  {message}"), cols, bottom)
    })
}

pub fn draw_list<W: Write>(out: &mut W, view: &ListView) -> Result<()> {
    draw_frame(
        out,
        &view.header,
        "↑/↓ select  ←/→ page  Enter view  r reply  p apply  / subject  a author  d date  u update  ? help  q quit",
        |out, cols, bottom| draw_list_body(out, view, cols, bottom),
    )
}

pub fn draw_detail<W: Write>(
    out: &mut W,
    header: &HeaderInfo,
    text: &str,
    scroll: usize,
) -> Result<()> {
    draw_frame(
        out,
        header,
        "↑/↓/PgUp/PgDn scroll  g/G top/bottom  r reply  p apply  Esc/q back",
        |out, cols, bottom| draw_detail_body(out, text, scroll, cols, bottom),
    )
}

pub fn draw_help<W: Write>(out: &mut W, header: &HeaderInfo) -> Result<()> {
    draw_frame(out, header, "press any key to return", draw_help_body)
}

pub fn redraw_prompt<W: Write>(out: &mut W, label: &str, input: &str, y: u16) -> Result<()> {
    let (cols, _) = size()?;
    let max_w = (cols as usize).saturating_sub(1);
    let combined = format!("{}{}", label, input);
    let total = combined.chars().count();
    let display: String = if total > max_w {
        combined.chars().skip(total - max_w).collect()
    } else {
        combined
    };
    execute!(
        out,
        MoveTo(0, y),
        Clear(ClearType::CurrentLine),
        Print(display),
        Show
    )?;
    out.flush()?;
    Ok(())
}

fn draw_header<W: Write>(out: &mut W, h: &HeaderInfo, cols: u16) -> Result<()> {
    let title = format!(
        " LKML Reader  —  list: {}   epoch: {}   page: {}   subject: {}   author: {}   date: {}",
        h.list_name, h.epoch_label, h.page_label, h.subject_filter, h.author_filter, h.date_filter,
    );
    queue!(
        out,
        MoveTo(0, 0),
        SetBackgroundColor(Color::DarkBlue),
        SetForegroundColor(Color::White),
        SetAttribute(Attribute::Bold),
        Print(pad_or_truncate(&title, cols as usize)),
        SetAttribute(Attribute::Reset),
        ResetColor,
    )?;
    Ok(())
}

fn draw_centered<W: Write>(out: &mut W, msg: &str, cols: u16, bottom: u16) -> Result<()> {
    let top = 1u16;
    if bottom <= top {
        return Ok(());
    }
    let y = top + (bottom - top) / 2;
    let x = ((cols as usize).saturating_sub(msg.chars().count()) / 2) as u16;
    queue!(
        out,
        MoveTo(x, y),
        SetAttribute(Attribute::Bold),
        Print(msg),
        SetAttribute(Attribute::Reset),
    )?;
    Ok(())
}

fn draw_list_body<W: Write>(out: &mut W, view: &ListView, cols: u16, bottom: u16) -> Result<()> {
    let top = 1u16;
    if bottom <= top {
        return Ok(());
    }
    let visible = (bottom - top) as usize;

    if view.mails.is_empty() {
        for (i, line) in view.empty_message.iter().enumerate() {
            let y = top + 1 + i as u16;
            if y >= bottom {
                break;
            }
            queue!(out, MoveTo(2, y), Print(line))?;
        }
        return Ok(());
    }

    for (row, idx) in (view.scroll..view.mails.len()).take(visible).enumerate() {
        queue_list_row(out, view, idx, cols, top + row as u16)?;
    }
    Ok(())
}

/// Render the row for `idx` at screen line `y`, applying the marquee scroll when
/// it is the selected row. Extracted so the per-tick marquee update can redraw
/// just one line instead of repainting the whole screen.
fn queue_list_row<W: Write>(
    out: &mut W,
    view: &ListView,
    idx: usize,
    cols: u16,
    y: u16,
) -> Result<()> {
    let mail = &view.mails[idx];
    let selected = idx == view.selected;
    let indent = if view.indent[idx] { INDENT } else { "" };
    let page_count = view.mails.len();
    let prefix = format!(
        " [{:0idx_w$}] {:DATE_W$}  ",
        view.offset + idx + 1,
        mail.date_str(),
        idx_w = index_width(view.offset, page_count),
    );
    let subject_w = subject_column_width(cols, view.offset, page_count, view.indent[idx]);
    let title_chars = mail.subject.chars().count();
    let subject: String = if selected && title_chars > subject_w {
        // Ring the title with a tab-width gap so it scrolls continuously:
        // when the end passes the column's right edge, the start reappears
        // after the gap.
        const TAB_GAP: &str = "    ";
        let cycle = title_chars + TAB_GAP.chars().count();
        let skip = view.selected_scroll % cycle;
        mail.subject
            .chars()
            .chain(TAB_GAP.chars())
            .cycle()
            .skip(skip)
            .take(subject_w)
            .collect()
    } else {
        mail.subject.chars().take(subject_w).collect()
    };
    let line = format!(
        "{prefix}{author:<AUTHOR_W$} {indent}{subject}",
        author = truncate(&mail.author, AUTHOR_W),
    );
    if selected {
        queue!(
            out,
            MoveTo(0, y),
            SetBackgroundColor(Color::Blue),
            SetForegroundColor(Color::White),
            Print(pad_or_truncate(&line, cols as usize)),
            ResetColor,
        )?;
    } else {
        queue!(
            out,
            MoveTo(0, y),
            Print(pad_or_truncate(&line, cols as usize))
        )?;
    }
    Ok(())
}

/// Redraw only the selected row in-place. Used by the marquee tick to update
/// the scrolling title without clearing the screen (which would flicker at
/// the tick rate).
pub fn redraw_selected_row<W: Write>(out: &mut W, view: &ListView) -> Result<()> {
    let (cols, rows) = size()?;
    let bottom = rows.saturating_sub(1);
    let top = 1u16;
    if bottom <= top || view.selected >= view.mails.len() || view.selected < view.scroll {
        return Ok(());
    }
    queue!(out, Hide)?;
    let y = top + (view.selected - view.scroll) as u16;
    queue_list_row(out, view, view.selected, cols, y)?;
    out.flush()?;
    Ok(())
}

fn draw_detail_body<W: Write>(
    out: &mut W,
    text: &str,
    scroll: usize,
    cols: u16,
    bottom: u16,
) -> Result<()> {
    let top = 1u16;
    if bottom <= top {
        return Ok(());
    }
    let visible = (bottom - top) as usize;
    let lines: Vec<&str> = text.lines().collect();
    let max_scroll = lines.len().saturating_sub(visible);
    let scroll = scroll.min(max_scroll);
    for row in 0..visible {
        let idx = scroll + row;
        if idx >= lines.len() {
            break;
        }
        let y = top + row as u16;
        let line = truncate(lines[idx], cols as usize);
        queue!(out, MoveTo(0, y))?;
        if let Some(color) = diff_line_color(&line) {
            queue!(out, SetForegroundColor(color), Print(line), ResetColor)?;
        } else {
            queue!(out, Print(line))?;
        }
    }
    Ok(())
}

/// Pick a foreground color for unified-diff lines so patches embedded in mails
/// render with the familiar red/green/cyan scheme. `+++`/`---` file headers
/// match their hunk side (green/red); `@@` hunk markers are cyan.
fn diff_line_color(line: &str) -> Option<Color> {
    if line.starts_with("+++") {
        Some(Color::Green)
    } else if line.starts_with("---") {
        Some(Color::Red)
    } else if line.starts_with("@@") {
        Some(Color::Cyan)
    } else if line.starts_with('+') {
        Some(Color::Green)
    } else if line.starts_with('-') {
        Some(Color::Red)
    } else {
        None
    }
}

fn draw_help_body<W: Write>(out: &mut W, cols: u16, bottom: u16) -> Result<()> {
    let lines = [
        "LKML Reader — Keys",
        "",
        "  q          quit",
        "  ↑          move selection up (within page)",
        "  ↓          move selection down (within page)",
        "  →          next page",
        "  ←          previous page",
        "  Enter      open selected mail",
        "  r          reply to selected mail ($EDITOR, then git send-email)",
        "  p          apply the selected mail's patch series to a git tree (git am)",
        "  /          set subject filter (scans in the background; pages open as matches arrive)",
        "  a          set author filter (substring of the From header: name or address)",
        "  d          set date filter (today | yesterday | YYYY/MM/DD HH:MM to YYYY/MM/DD HH:MM)",
        "  u          update current mirror (git remote update on the latest epoch)",
        "",
        "  Detail view:",
        "    ↑/↓/PgUp/PgDn scroll   Space = page down",
        "    g / G    jump to top / bottom",
        "    r        reply   p  apply patch series   Esc / q  back to list",
        "",
        "Press any key to return.",
    ];
    let top = 2u16;
    for (i, line) in lines.iter().enumerate() {
        let y = top + i as u16;
        if y >= bottom {
            break;
        }
        queue!(
            out,
            MoveTo(2, y),
            Print(truncate(line, (cols as usize).saturating_sub(2)))
        )?;
    }
    Ok(())
}

fn draw_hotkeys<W: Write>(out: &mut W, hint: &str, cols: u16, rows: u16) -> Result<()> {
    queue!(
        out,
        MoveTo(0, rows.saturating_sub(1)),
        SetBackgroundColor(Color::DarkGrey),
        SetForegroundColor(Color::White),
        Print(pad_or_truncate(&format!(" {}", hint), cols as usize)),
        ResetColor,
    )?;
    Ok(())
}

fn pad_or_truncate(s: &str, w: usize) -> String {
    let truncated: String = s.chars().take(w).collect();
    let len = truncated.chars().count();
    if len >= w {
        truncated
    } else {
        let mut out = truncated;
        out.extend(std::iter::repeat_n(' ', w - len));
        out
    }
}

fn truncate(s: &str, w: usize) -> String {
    s.chars().take(w).collect()
}

/// Width of the row number column: wide enough for the last mail on the page.
fn index_width(offset: usize, page_count: usize) -> usize {
    (offset + page_count).to_string().len().max(3)
}

/// Width of the subject column for a row of the current page. The one place the
/// row layout is worked out: `queue_list_row` renders against it, and the marquee
/// tick asks it whether a title overflows without re-rendering.
pub fn subject_column_width(cols: u16, offset: usize, page_count: usize, indented: bool) -> usize {
    // prefix is " [<idx>] <date>  ": 2 + idx_w + 2 + DATE_W + 2 chars.
    let prefix_w = 6 + index_width(offset, page_count) + DATE_W;
    let indent_w = if indented { INDENT.chars().count() } else { 0 };
    (cols as usize).saturating_sub(prefix_w + AUTHOR_W + 1 + indent_w)
}
