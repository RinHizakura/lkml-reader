# lkml-reader

A Rust-based interactive reader for Linux Kernel Mailing Lists hosted on
[lore.kernel.org](https://lore.kernel.org), inspired by
[hackermail](https://github.com/sjp38/hackermail).

It mirrors lore's public-inbox git archives locally and provides a terminal UI
to browse and read mails. The main goal is to have a simple, fast, and
keyboard-friendly way to keep up with the latest discussions in the Linux kernel
community.


## Features (MVP)

- Pick any mailing list on lore.kernel.org (`linux-pm`, `linux-mm`,
  `linux-kernel`, `damon`, …) via `--list`.
- Filter by subsystem interactively with `f` (case-insensitive substring match
  against the subject, e.g. `sched` matches `[PATCH sched/core]`, `[sched]`,
  `[scheduler]`).
- Read the raw mail in a scrollable detail pane.

## Build & run

```sh
# 1. Build
cargo build --release

# 2. Launch the TUI — it clones the latest epoch on first run, and
#    runs `git remote update` on every subsequent start.
./target/release/lkml-reader --list lkml
```

CLI:

```
lkml-reader [--list <LIST>]
```

Defaults: `--list lkml`.

## Keys

| View   | Key                    | Action                          |
|--------|------------------------|---------------------------------|
| List   | `↑` / `↓`              | Move selection within page      |
| List   | `←` / `→`              | Previous / next page            |
| List   | `Home`/`End`           | First / last (within page)      |
| List   | `Enter`                | Open mail                       |
| List   | `/`                    | Set subject filter (eager scan across cloned epochs) |
| List   | `u`                    | Update current mirror (`git remote update`) |
| List   | `?`                    | Help                            |
| List   | `q`                    | Quit                            |
| Detail | `↑`/`↓`, `PgUp`/`PgDn` | Scroll                          |
| Detail | `g` / `G`              | Top / bottom                    |
| Detail | `Esc` / `q`            | Back to list                    |

## How it works

`lore.kernel.org` runs [public-inbox](https://public-inbox.org/), which exposes
each mailing list as one or more **bare git repositories**. Every commit is a
single email; the mail's body is stored as the blob `m` in the commit's tree.
This app reads mails by cloning those git mirrors and shelling out to `git log`
/ `git show`.

### Manifest

`https://lore.kernel.org/manifest.js.gz` is a gzipped JSON catalog (grokmirror
format) of every repo lore serves:

```jsonc
{
  "/lkml/git/0.git":  { "description": "LKML [epoch 0]",  ... },
  "/lkml/git/1.git":  { "description": "LKML [epoch 1]",  ... },
  ...
  "/lkml/git/19.git": { "description": "LKML [epoch 19]", ... }
}
```

On startup the app fetches this file, decompresses it, and extracts the epoch
numbers (`0..=19` above) for the list passed via `--list`.

### Epochs

Once a list's git repo grows large, public-inbox rolls a new repo so each one
stays cloneable in a reasonable time. Those numbered slices (`0.git`, `1.git`,
...) are called **epochs**. They're append-only and time-ordered:

- Higher epoch number = newer mail.
- The current epoch is the only one still receiving new commits.
- Older epochs never change once retired.

The app starts at the highest epoch (newest mails). When you page past the
end of the current epoch, it rolls back to `epoch - 1` to fetch older history.
Small lists (e.g. `damon`) only have `0.git`; lkml currently has 20 epochs.

### Local cache

Mirrors live under:

```
$XDG_CACHE_HOME/lkml-reader/archives/<list>/<epoch>.git
```

(falls back to `~/.cache/lkml-reader/archives/...`). The TUI manages this
directly: on startup it clones the latest epoch if missing, then runs
`git remote update` on every launch and whenever you press `u`. Reading a mail
uses `git log` / `git show` against the local mirror — no network round-trip
once the clone is in place.

Older epochs are auto-cloned on demand: paging past the current epoch's
oldest commit triggers a `git clone --mirror` for `epoch - 1` (with a loading
view while it runs), then paging continues into the freshly-cloned mirror.

### Reading a mail

When you press `Enter`, the app already knows the commit hash and epoch for the
selected row (each entry in the current `page_items` carries its epoch + commit).
Opening the mail is simply `git show <hash>:m` against that epoch's local mirror
— no network round-trip.
