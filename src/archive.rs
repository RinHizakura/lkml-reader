// SPDX-License-Identifier: GPL-2.0

use anyhow::{bail, Context, Result};
use chrono::{DateTime, FixedOffset};
use flate2::read::GzDecoder;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::mail::Mail;

const BASE: &str = "https://lore.kernel.org";
const UA: &str = concat!("lkml-reader/", env!("CARGO_PKG_VERSION"));

pub fn http_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .user_agent(UA)
        .gzip(true)
        .timeout(Duration::from_secs(60))
        .build()
        .context("building HTTP client")
}

pub fn archive_root() -> PathBuf {
    let base = std::env::var("XDG_CACHE_HOME")
        .ok()
        .or_else(|| std::env::var("HOME").ok().map(|h| format!("{h}/.cache")))
        .unwrap_or_else(|| "/tmp".to_string());
    PathBuf::from(base).join("lkml-reader/archives")
}

pub fn local_repo_path(list: &str, epoch: u32) -> PathBuf {
    archive_root().join(format!("{list}/{epoch}.git"))
}

pub fn manifest_url() -> String {
    format!("{BASE}/manifest.js.gz")
}

pub fn fetch_manifest(client: &reqwest::blocking::Client) -> Result<String> {
    let url = manifest_url();
    let resp = client
        .get(&url)
        .send()
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        bail!("manifest fetch failed ({}): {url}", resp.status());
    }
    let bytes = resp.bytes().context("reading manifest body")?;
    let mut decoder = GzDecoder::new(&bytes[..]);
    let mut out = String::new();
    decoder
        .read_to_string(&mut out)
        .context("decompressing manifest")?;
    Ok(out)
}

pub fn manifest_epochs(json: &str, list: &str) -> Vec<u32> {
    let prefix = format!("\"/{}/git/", list);
    let mut epochs = std::collections::BTreeSet::new();
    for (i, _) in json.match_indices(&prefix) {
        let start = i + prefix.len();
        let end = json[start..]
            .find(|c: char| !c.is_ascii_digit())
            .map(|p| start + p)
            .unwrap_or(json.len());
        if start < end {
            if let Ok(n) = json[start..end].parse::<u32>() {
                epochs.insert(n);
            }
        }
    }
    epochs.into_iter().collect()
}

pub fn repo_url(list: &str, epoch: u32) -> String {
    format!("{BASE}/{list}/git/{epoch}.git")
}

pub fn repo_exists(list: &str, epoch: u32) -> bool {
    local_repo_path(list, epoch).exists()
}

pub fn update_mirror(list: &str, epoch: u32) -> Result<()> {
    let dir = local_repo_path(list, epoch);
    if !dir.is_dir() {
        bail!("mirror not present: {}", dir.display());
    }
    let out = Command::new("git")
        .arg(format!("--git-dir={}", dir.display()))
        .arg("remote")
        .arg("update")
        .output()
        .context("running git remote update")?;
    if !out.status.success() {
        bail!(
            "git remote update failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

pub fn clone_mirror(list: &str, epoch: u32) -> Result<()> {
    let dir = local_repo_path(list, epoch);
    if dir.exists() {
        bail!("mirror already exists: {}", dir.display());
    }
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent).context("creating cache dir")?;
    }
    let url = repo_url(list, epoch);
    let out = Command::new("git")
        .arg("clone")
        .arg("--mirror")
        .arg(&url)
        .arg(&dir)
        .output()
        .context("running git clone --mirror")?;
    if !out.status.success() {
        bail!(
            "git clone --mirror failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

pub fn list_all_commits(git_dir: &Path) -> Result<Vec<String>> {
    let out = Command::new("git")
        .arg(format!("--git-dir={}", git_dir.display()))
        .arg("log")
        .arg("--pretty=format:%H")
        .output()
        .context("running git log")?;
    if !out.status.success() {
        bail!(
            "git log failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect())
}

pub fn show_mail(git_dir: &Path, commit: &str) -> Result<String> {
    let out = Command::new("git")
        .arg(format!("--git-dir={}", git_dir.display()))
        .arg("show")
        .arg(format!("{commit}:m"))
        .output()
        .context("running git show")?;
    if !out.status.success() {
        bail!(
            "git show failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn parse_mail_from_raw(raw: &str, epoch: u32, commit: String) -> Mail {
    let header_end = raw
        .find("\r\n\r\n")
        .or_else(|| raw.find("\n\n"))
        .unwrap_or(raw.len());
    let headers = &raw[..header_end];
    let title = header(headers, "Subject")
        .map(|s| decode_mime_header(&s))
        .unwrap_or_default();
    let from = header(headers, "From")
        .map(|s| decode_mime_header(&s))
        .unwrap_or_default();
    let author = pretty_from(&from);
    let date = header(headers, "Date").and_then(|s| parse_rfc_date(&s));
    Mail {
        title,
        author,
        date,
        epoch,
        commit,
    }
}

/// Decode RFC 2047 encoded-words like `=?UTF-8?B?...?=` into a UTF-8 string.
/// Handles `B` (base64) and `Q` (quoted-printable) encodings. Non-UTF-8
/// charsets are decoded as if their byte stream were UTF-8 (lossy).
pub fn decode_mime_header(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    let mut last_was_encoded = false;
    while !rest.is_empty() {
        let Some(start) = rest.find("=?") else {
            out.push_str(rest);
            break;
        };
        let between = &rest[..start];
        if !(last_was_encoded && between.chars().all(char::is_whitespace)) {
            out.push_str(between);
        }
        let after = &rest[start + 2..];
        let Some(end) = after.find("?=") else {
            out.push_str("=?");
            rest = after;
            last_was_encoded = false;
            continue;
        };
        let body = &after[..end];
        match decode_encoded_word(body) {
            Some(decoded) => {
                out.push_str(&decoded);
                last_was_encoded = true;
            }
            None => {
                out.push_str("=?");
                out.push_str(body);
                out.push_str("?=");
                last_was_encoded = false;
            }
        }
        rest = &after[end + 2..];
    }
    out
}

fn decode_encoded_word(body: &str) -> Option<String> {
    let mut parts = body.splitn(3, '?');
    let _charset = parts.next()?;
    let enc = parts.next()?;
    let text = parts.next()?;
    let bytes = match enc.to_ascii_uppercase().as_str() {
        "B" => decode_base64(text)?,
        "Q" => decode_q(text),
        _ => return None,
    };
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

fn decode_base64(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;
    for c in s.bytes() {
        if c.is_ascii_whitespace() {
            continue;
        }
        let val: u32 = match c {
            b'A'..=b'Z' => (c - b'A') as u32,
            b'a'..=b'z' => (c - b'a') as u32 + 26,
            b'0'..=b'9' => (c - b'0') as u32 + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            _ => return None,
        };
        buf = (buf << 6) | val;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xFF) as u8);
        }
    }
    Some(out)
}

fn decode_q(s: &str) -> Vec<u8> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'_' => {
                out.push(b' ');
                i += 1;
            }
            b'=' if i + 2 < bytes.len() => {
                let h1 = (bytes[i + 1] as char).to_digit(16);
                let h2 = (bytes[i + 2] as char).to_digit(16);
                if let (Some(a), Some(b)) = (h1, h2) {
                    out.push(((a << 4) | b) as u8);
                    i += 3;
                } else {
                    out.push(b'=');
                    i += 1;
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

fn header(headers: &str, name: &str) -> Option<String> {
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

fn pretty_from(from: &str) -> String {
    if let Some(start) = from.find('<') {
        let name = from[..start].trim().trim_matches('"');
        if !name.is_empty() {
            return name.to_string();
        }
        if let Some(end) = from[start..].find('>') {
            return from[start + 1..start + end].to_string();
        }
    }
    from.trim().to_string()
}

fn parse_rfc_date(s: &str) -> Option<DateTime<FixedOffset>> {
    if s.is_empty() {
        return None;
    }
    DateTime::parse_from_rfc2822(s)
        .ok()
        .or_else(|| DateTime::parse_from_rfc3339(s).ok())
}


