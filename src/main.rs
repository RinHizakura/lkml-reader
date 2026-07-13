// SPDX-License-Identifier: GPL-2.0

mod app;
mod patch;
mod reply;
mod source;
mod ui;

use anyhow::Result;
use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "lkml-reader",
    version,
    about = "An interactive TUI reader for kernel.org public-inbox mailing lists."
)]
struct Args {
    #[arg(
        short,
        long,
        default_value = "lkml",
        help = "Mailing list on lore.kernel.org (e.g. lkml, linux-pm, linux-mm)\n"
    )]
    list: String, // See https://lore.kernel.org/ for the full set.
}

fn main() -> Result<()> {
    let args = Args::parse();
    app::App::new(args.list).run()
}
