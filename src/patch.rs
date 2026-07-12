// SPDX-License-Identifier: GPL-2.0

//! Applying a patch series, the way hackermail does it: hand the mails to
//! `git am` in the user's own tree and let git do the applying.
//!
//! Which mails make up the series is lkml-core's answer ([`thread::patch_series`]);
//! this module only runs git and talks to the terminal.

use anyhow::{bail, Context, Result};
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};
use std::{env, fs};

use lkml_core::mail::Mail;

/// A scratch directory that deletes itself on drop.
struct ScratchDir {
    path: PathBuf,
}

impl ScratchDir {
    fn new() -> Result<Self> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = env::temp_dir().join(format!("lkml-patch-{}-{nanos}", std::process::id()));
        fs::create_dir(&path).context("creating the temp patch dir")?;
        Ok(Self { path })
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.path).ok();
    }
}

/// Write `patches` out and `git am` them in `repo`. Prints progress to the
/// plain terminal (the caller must have left the TUI first) and asks before
/// applying a series with patches missing — a half-applied series is the one
/// outcome worth a prompt.
pub fn apply(repo: &str, patches: &[Mail]) -> Result<()> {
    if patches.is_empty() {
        bail!("no patch mails found in this thread");
    }
    if !is_git_repo(repo) {
        bail!("not a git repository: {repo}");
    }

    println!("\nApplying to {repo}:\n");
    for mail in patches {
        println!("  {}", mail.subject);
    }

    let expected = patches
        .iter()
        .filter_map(|m| m.patch_tag)
        .map(|t| t.total as usize)
        .max()
        .unwrap_or(1);
    if patches.len() != expected {
        println!(
            "\nWarning: the series says {expected} patches, but only {} were found in the mirror.",
            patches.len()
        );
        if !confirm("Apply the incomplete series anyway? [y/N]: ")? {
            bail!("aborted");
        }
    }

    // One file per mail: `git am` mailsplits an mbox on "From " lines, and a
    // patch that adds such a line would split in the wrong place.
    let dir = ScratchDir::new()?;
    let mut files = Vec::new();
    for (i, mail) in patches.iter().enumerate() {
        let path = dir.path.join(format!("{:04}.patch", i + 1));
        fs::write(&path, &mail.raw).context("writing a patch file")?;
        files.push(path);
    }

    println!();
    let status = Command::new("git")
        .args(["-C", repo, "am"])
        .args(&files)
        .status();

    if !status.context("running git am")?.success() {
        bail!("git am failed; the tree is mid-apply (git -C {repo} am --abort to undo)");
    }
    Ok(())
}

pub fn is_git_repo(repo: &str) -> bool {
    Command::new("git")
        .args(["-C", repo, "rev-parse", "--git-dir"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn confirm(prompt: &str) -> Result<bool> {
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y"))
}
