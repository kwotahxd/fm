use anyhow::Result;
use fm_app::App;
use fm_core::listing::{FileKind, SortKey};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

#[derive(Clone, Copy, clap::ValueEnum)]
pub enum SortBy {
    Name,
    Size,
    Mtime,
    None,
}
impl From<SortBy> for SortKey {
    fn from(s: SortBy) -> Self {
        match s {
            SortBy::Name => SortKey::Name,
            SortBy::Size => SortKey::Size,
            SortBy::Mtime => SortKey::Mtime,
            SortBy::None => SortKey::None,
        }
    }
}

fn perm_string(mode: u32, kind: FileKind) -> String {
    let bit = |m: u32, set: char| if mode & m != 0 { set } else { '-' };
    format!(
        "{}{}{}{}{}{}{}{}{}{}",
        kind.type_char() as char,
        bit(0o400, 'r'),
        bit(0o200, 'w'),
        bit(0o100, 'x'),
        bit(0o040, 'r'),
        bit(0o020, 'w'),
        bit(0o010, 'x'),
        bit(0o004, 'r'),
        bit(0o002, 'w'),
        bit(0o001, 'x'),
    )
}

fn human_size(n: u64) -> String {
    const U: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n}")
    } else {
        format!("{v:.1}{}", U[i])
    }
}

pub fn run(app: &App, path: &Path, all: bool, long: bool, sort: SortBy, reverse: bool) -> Result<()> {
    let listing = app.listing(path)?;
    let order = listing.order(sort.into(), reverse);
    for i in order {
        let e = listing.get(i);
        let name = e.name.to_string_lossy();
        if !all && name.starts_with('.') {
            continue;
        }
        if !long {
            println!("{name}");
            continue;
        }
        match e.meta {
            Some(m) => {
                let mtime = UNIX_EPOCH + Duration::from_nanos(m.mtime_ns.max(0) as u64);
                let secs = mtime.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
                println!(
                    "{} {:>4} {:>8} {:>8} {}  {}",
                    perm_string(std::fs::Permissions::from_mode(m.mode).mode(), e.kind),
                    m.nlink,
                    m.uid,
                    human_size(m.size),
                    fmt_epoch(secs),
                    name
                );
            }
            None => println!("{} {name} (metadata unavailable)", perm_string(0, e.kind)),
        }
    }
    Ok(())
}

/// `YYYY-MM-DD HH:MM` in UTC, no chrono: reuses the sorter's civil-date algorithm plus a little arithmetic.
fn fmt_epoch(secs: u64) -> String {
    let (y, m, d) = fm_sorter::template::civil_from_secs(secs as i64);
    let (hh, mm) = ((secs / 3600) % 24, (secs / 60) % 60);
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}")
}
