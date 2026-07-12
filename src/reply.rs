// SPDX-License-Identifier: GPL-2.0

//! Sending a reply, the way hackermail does it: put the draft in `$EDITOR`,
//! then hand the file to `git send-email`.
//!
//! No account credentials live here, and none should. `git send-email` already
//! reads `sendemail.smtpServer`, `sendemail.smtpUser`, `sendemail.smtpEncryption`
//! and a password (or a `sendmail`/msmtp binary) from git config, takes `From:`
//! from `user.name`/`user.email`, and prompts for confirmation before sending.

use anyhow::{bail, Result};
use std::process::Command;
use std::{env, fs};

/// Write `draft` to a temp file, open it in `$EDITOR`, then send it with
/// `git send-email`. Both take over the terminal, so the caller must have it out
/// of raw mode and off the alternate screen.
pub fn compose_and_send(draft: &str) -> Result<()> {
    let path = env::temp_dir().join(format!("lkml-reply-{}.mbox", std::process::id()));
    fs::write(&path, draft)?;

    let editor = env::var("EDITOR")
        .or_else(|_| env::var("VISUAL"))
        .unwrap_or_else(|_| "vi".to_string());
    let edited = Command::new(&editor).arg(&path).status()?;
    if !edited.success() {
        fs::remove_file(&path).ok();
        bail!("{editor} exited with {edited}, reply discarded");
    }

    let sent = Command::new("git")
        .args(["send-email", "--confirm=always"])
        .arg(&path)
        .status();
    fs::remove_file(&path).ok();
    if !sent?.success() {
        bail!("git send-email failed");
    }
    Ok(())
}
