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

use lkml_core::filter::{DateFilter, SubjectFilter};
use lkml_core::mail::Mail;

pub struct HeaderInfo<'a> {
    pub list_name: &'a str,
    pub epoch_label: &'a str,
    pub page_label: &'a str,
    pub subject_filter: &'a SubjectFilter,
    pub date_filter: &'a DateFilter,
}

pub struct LoadingView<'a> {
    pub header: HeaderInfo<'a>,
    pub message: &'a str,
}

pub struct ListView<'a> {
    pub header: HeaderInfo<'a>,
    pub page_idx: usize,
    pub page_size: usize,
    pub mails: &'a [Mail],
    pub selected: usize,
    /// Tick counter driving the marquee scroll on the selected row's title.
    pub selected_scroll: usize,
    pub empty_message: &'a [String],
}

pub struct DetailView<'a> {
    pub header: HeaderInfo<'a>,
    pub text: &'a str,
    pub scroll: usize,
}

pub struct HelpView<'a> {
    pub header: HeaderInfo<'a>,
}

pub fn draw_loading<W: Write>(out: &mut W, view: &LoadingView) -> Result<()> {
    let (cols, rows) = size()?;
    let bottom = content_bottom(rows);
    begin_frame(out)?;
    draw_header(out, &view.header, cols)?;
    draw_centered(out, &format!("⏳  {}", view.message), cols, bottom)?;
    draw_hotkeys(out, "", cols, rows)?;
    out.flush()?;
    Ok(())
}

pub fn draw_list<W: Write>(out: &mut W, view: &ListView) -> Result<()> {
    let (cols, rows) = size()?;
    let bottom = content_bottom(rows);
    begin_frame(out)?;
    draw_header(out, &view.header, cols)?;
    draw_list_body(out, view, cols, bottom)?;
    draw_hotkeys(
        out,
        "↑/↓ select  ←/→ page  Enter view  / subject  d date  u update  ? help  q quit",
        cols,
        rows,
    )?;
    out.flush()?;
    Ok(())
}

pub fn draw_detail<W: Write>(out: &mut W, view: &DetailView) -> Result<()> {
    let (cols, rows) = size()?;
    let bottom = content_bottom(rows);
    begin_frame(out)?;
    draw_header(out, &view.header, cols)?;
    draw_detail_body(out, view.text, view.scroll, cols, bottom)?;
    draw_hotkeys(
        out,
        "↑/↓/PgUp/PgDn scroll  g/G top/bottom  Esc/q back",
        cols,
        rows,
    )?;
    out.flush()?;
    Ok(())
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

pub fn draw_help<W: Write>(out: &mut W, view: &HelpView) -> Result<()> {
    let (cols, rows) = size()?;
    let bottom = content_bottom(rows);
    begin_frame(out)?;
    draw_header(out, &view.header, cols)?;
    draw_help_body(out, cols, bottom)?;
    draw_hotkeys(out, "press any key to return", cols, rows)?;
    out.flush()?;
    Ok(())
}

fn content_bottom(rows: u16) -> u16 {
    rows.saturating_sub(1)
}

fn begin_frame<W: Write>(out: &mut W) -> Result<()> {
    queue!(out, Hide, Clear(ClearType::All))?;
    Ok(())
}

fn draw_header<W: Write>(out: &mut W, h: &HeaderInfo, cols: u16) -> Result<()> {
    let title = format!(
        " LKML Reader  —  list: {}   epoch: {}   page: {}   subject: {}   date: {}",
        h.list_name, h.epoch_label, h.page_label, h.subject_filter, h.date_filter,
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

fn draw_list_body<W: Write>(
    out: &mut W,
    view: &ListView,
    cols: u16,
    bottom: u16,
) -> Result<()> {
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

    let total = view.mails.len();
    let row_count = visible.min(total);
    for row in 0..row_count {
        queue_list_row(out, view, row, cols, top)?;
    }
    Ok(())
}

/// Render a single row at its on-screen position, applying the marquee scroll
/// when this is the selected row. Extracted so the per-tick marquee update can
/// redraw just one line instead of repainting the whole screen.
fn queue_list_row<W: Write>(
    out: &mut W,
    view: &ListView,
    row: usize,
    cols: u16,
    top: u16,
) -> Result<()> {
    let total = view.mails.len();
    let page_offset = view.page_idx * view.page_size;
    let idx_w = (page_offset + total).to_string().len().max(3);
    let date_w = 16;
    let author_w = 24;

    let mail = &view.mails[row];
    let selected = row == view.selected;
    let abs_idx = page_offset + row + 1;
    let prefix = format!(
        " [{:0idx_w$}] {:date_w$}  ",
        abs_idx,
        mail.date_str(),
        idx_w = idx_w,
        date_w = date_w
    );
    let subject_w = (cols as usize).saturating_sub(prefix.len() + author_w + 1);
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
        "{prefix}{author:<author_w$} {subject}",
        author = truncate(&mail.author(), author_w),
        author_w = author_w,
    );
    let y = top + row as u16;
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
    let bottom = content_bottom(rows);
    let top = 1u16;
    if bottom <= top || view.mails.is_empty() || view.selected >= view.mails.len() {
        return Ok(());
    }
    queue!(out, Hide)?;
    queue_list_row(out, view, view.selected, cols, top)?;
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
        "  /          set subject filter (scans in the background; pages open as matches arrive)",
        "  d          set date filter (today | yesterday | YYYY/MM/DD HH:MM to YYYY/MM/DD HH:MM)",
        "  u          update current mirror (git remote update on the latest epoch)",
        "",
        "  Detail view:",
        "    ↑/↓/PgUp/PgDn scroll   Space = page down",
        "    g / G    jump to top / bottom",
        "    Esc / q  back to list",
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

/// Width of the subject column in the list view, given the terminal width and
/// the number of mails on the current page. Mirrors the prefix/author layout
/// in `draw_list_body` so callers (e.g. the marquee tick) can decide whether
/// scrolling is needed without re-rendering.
pub fn subject_column_width(cols: u16, page_idx: usize, page_size: usize, page_count: usize) -> usize {
    let page_offset = page_idx * page_size;
    let idx_w = (page_offset + page_count).to_string().len().max(3);
    let date_w = 16;
    let author_w = 24;
    // prefix is " [<idx>] <date>  ": 2 + idx_w + 2 + date_w + 2 chars.
    let prefix_len = 6 + idx_w + date_w;
    (cols as usize).saturating_sub(prefix_len + author_w + 1)
}

