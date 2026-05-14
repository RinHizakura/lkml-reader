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

    let encoding = extract_header(header_block, "Content-Transfer-Encoding")
        .unwrap_or_default()
        .to_ascii_lowercase();
    if encoding.trim() == "quoted-printable" {
        out.push_str(&decode_quoted_printable(body));
    } else {
        out.push_str(body);
    }
    out
}

/// Decode an RFC 2045 quoted-printable body: join soft line breaks
/// (`=` followed by CRLF or LF) and decode `=XX` hex escapes.
fn decode_quoted_printable(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'=' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        if i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
            i += 2;
            continue;
        }
        if i + 2 < bytes.len() && bytes[i + 1] == b'\r' && bytes[i + 2] == b'\n' {
            i += 3;
            continue;
        }
        if i + 2 < bytes.len() {
            let h1 = (bytes[i + 1] as char).to_digit(16);
            let h2 = (bytes[i + 2] as char).to_digit(16);
            if let (Some(a), Some(b)) = (h1, h2) {
                out.push(((a << 4) | b) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b'=');
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
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
