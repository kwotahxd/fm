use fm_app::AppError;
use fm_types::FileRecord;

pub fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

pub fn print_file_line(f: &FileRecord, json: bool) {
    if json {
        println!(
            r#"{{"path":{},"size":{},"mime":{},"category":{},"tags":[{}]}}"#,
            json_str(&f.path),
            f.size,
            f.mime.as_deref().map(json_str).unwrap_or_else(|| "null".into()),
            f.category.as_deref().map(json_str).unwrap_or_else(|| "null".into()),
            f.tags.iter().map(|t| json_str(t)).collect::<Vec<_>>().join(",")
        );
    } else {
        let cat = f.category.as_deref().unwrap_or("-");
        println!("{:>10}  {:<15} {}", f.size, cat, f.path);
    }
}

/// Turn an `AppError` into a message with an actionable hint, for the CLI's top-level error printer.
pub fn friendly(e: AppError) -> anyhow::Error {
    match &e {
        AppError::AiUnavailable(_) => anyhow::anyhow!("{e}\nhint: start the engine with `python -m aiengine` (or `--mock` for testing), or check [ai] in your config"),
        _ => e.into(),
    }
}
