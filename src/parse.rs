// SPDX-License-Identifier: GPL-2.0

use crate::archive;

pub fn format_raw_mail(raw: &str) -> String {
    let header_end = raw
        .find("\r\n\r\n")
        .map(|i| (i, 4))
        .or_else(|| raw.find("\n\n").map(|i| (i, 2)))
        .unwrap_or((raw.len(), 0));

    let header_block = &raw[..header_end.0];
    let body = &raw[(header_end.0 + header_end.1).min(raw.len())..];

    let mut out = String::new();
    for field in ["From", "Date", "Subject", "To", "Cc", "Message-ID"] {
        if let Some(v) = extract_header(header_block, field) {
            out.push_str(field);
            out.push_str(": ");
            out.push_str(&archive::decode_mime_header(&v));
            out.push('\n');
        }
    }
    out.push_str("\n--\n\n");
    out.push_str(body);
    out
}

pub fn extract_header(headers: &str, name: &str) -> Option<String> {
    let target = name.to_lowercase();
    let mut iter = headers.lines().peekable();
    while let Some(line) = iter.next() {
        let Some(idx) = line.find(':') else { continue };
        if line[..idx].to_lowercase() != target {
            continue;
        }
        let mut v = line[idx + 1..].trim().to_string();
        while let Some(cont) = iter.peek() {
            if cont.starts_with(' ') || cont.starts_with('\t') {
                v.push(' ');
                v.push_str(cont.trim());
                iter.next();
            } else {
                break;
            }
        }
        return Some(v);
    }
    None
}
