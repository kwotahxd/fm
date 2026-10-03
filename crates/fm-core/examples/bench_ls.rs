//! `cargo run --release -p fm-core --example bench_ls -- <dir> [threads]`
//! Walks one directory (non-recursive, full stat) via fm-core and reports wall time + entry count,
//! in the same shape `hyperfine`/scripts can diff against `ls -la --color=never`.

use fm_core::listing::SortKey;
use fm_core::{read_dir, ReadOpts};
use std::fmt::Write as _;
use std::time::Instant;

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().unwrap_or_else(|| ".".into());
    let threads: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(1);
    let opts = ReadOpts { stat_threads: threads, par_threshold: 1, ..Default::default() };
    let start = Instant::now();
    let listing = read_dir(std::path::Path::new(&dir), &opts).expect("read_dir failed");
    // match `ls -la`'s default work: sort by name and format an `-l`-style line per entry
    // (built into a buffer, not printed, so terminal I/O doesn't dominate either side's number)
    let order = listing.order(SortKey::Name, false);
    let mut out = String::with_capacity(listing.len() * 48);
    for i in order {
        let e = listing.get(i);
        if let Some(m) = e.meta {
            let _ = writeln!(out, "{:o} {:>4} {:>10} {:>12} {}", m.mode, m.nlink, m.uid, m.size, e.name.to_string_lossy());
        }
    }
    let elapsed = start.elapsed();
    println!("fm-core: {} entries, {} bytes formatted, in {:?} (threads={threads})", listing.len(), out.len(), elapsed);
}
