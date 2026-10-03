//! Tiny MIME detection: extension table (free) + magic-byte sniffing (needs the first bytes).

pub fn from_ext(name: &str) -> Option<&'static str> {
    let ext = name.rsplit_once('.').map(|(_, e)| e)?;
    if ext.is_empty() || name.starts_with('.') && !name[1..].contains('.') {
        return None;
    }
    let e = ext.to_ascii_lowercase();
    Some(match e.as_str() {
        "txt" | "log" | "cfg" | "ini" | "conf" => "text/plain",
        "md" | "markdown" => "text/markdown",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "json" => "application/json",
        "xml" => "application/xml",
        "yaml" | "yml" => "application/yaml",
        "toml" => "application/toml",
        "pdf" => "application/pdf",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "odt" => "application/vnd.oasis.opendocument.text",
        "epub" => "application/epub+zip",
        "zip" => "application/zip",
        "tar" => "application/x-tar",
        "gz" | "tgz" => "application/gzip",
        "xz" => "application/x-xz",
        "bz2" => "application/x-bzip2",
        "zst" => "application/zstd",
        "7z" => "application/x-7z-compressed",
        "rar" => "application/vnd.rar",
        "iso" => "application/x-iso9660-image",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        "tif" | "tiff" => "image/tiff",
        "heic" => "image/heic",
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "wav" => "audio/wav",
        "ogg" | "oga" | "opus" => "audio/ogg",
        "m4a" => "audio/mp4",
        "mp4" | "m4v" => "video/mp4",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "avi" => "video/x-msvideo",
        "mov" => "video/quicktime",
        "rs" => "text/x-rust",
        "py" => "text/x-python",
        "c" | "h" => "text/x-c",
        "cpp" | "cc" | "cxx" | "hpp" => "text/x-c++",
        "go" => "text/x-go",
        "java" => "text/x-java",
        "js" | "mjs" => "text/javascript",
        "ts" | "tsx" | "jsx" => "text/x-typescript",
        "sh" | "bash" | "zsh" => "text/x-shellscript",
        "nix" => "text/x-nix",
        "sql" => "application/sql",
        "so" | "o" | "a" | "bin" | "exe" | "dll" => "application/octet-stream",
        _ => return None,
    })
}

/// Magic-byte sniffing over the first bytes of a file.
pub fn sniff(head: &[u8]) -> Option<&'static str> {
    let starts = |m: &[u8]| head.starts_with(m);
    if starts(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if starts(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if starts(b"GIF87a") || starts(b"GIF89a") {
        Some("image/gif")
    } else if starts(b"RIFF") && head.len() >= 12 && &head[8..12] == b"WEBP" {
        Some("image/webp")
    } else if starts(b"%PDF-") {
        Some("application/pdf")
    } else if starts(b"\x7fELF") {
        Some("application/x-elf")
    } else if starts(b"PK\x03\x04") {
        Some("application/zip")
    } else if starts(b"\x1f\x8b") {
        Some("application/gzip")
    } else if starts(b"ID3") || (head.len() > 1 && head[0] == 0xff && head[1] & 0xe0 == 0xe0) {
        Some("audio/mpeg")
    } else if starts(b"fLaC") {
        Some("audio/flac")
    } else if starts(b"OggS") {
        Some("audio/ogg")
    } else if head.len() >= 8 && &head[4..8] == b"ftyp" {
        Some("video/mp4")
    } else if starts(b"#!") {
        Some("text/x-shellscript")
    } else if !head.is_empty() && !head.contains(&0) && std::str::from_utf8(head).is_ok() {
        Some("text/plain")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ext_table() {
        assert_eq!(from_ext("a.PNG"), Some("image/png"));
        assert_eq!(from_ext("main.rs"), Some("text/x-rust"));
        assert_eq!(from_ext("Makefile"), None);
        assert_eq!(from_ext(".bashrc"), None);
        assert_eq!(from_ext("archive.tar.gz"), Some("application/gzip"));
    }

    #[test]
    fn magic() {
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\nxxxx"), Some("image/png"));
        assert_eq!(sniff(b"%PDF-1.7"), Some("application/pdf"));
        assert_eq!(sniff(b"hello world\n"), Some("text/plain"));
        assert_eq!(sniff(b"\x00\x01\x02"), None);
        assert_eq!(sniff(b"\x7fELF\x02"), Some("application/x-elf"));
    }
}
