"""Turns a file into something a model can read. Everything is lazy and bounded: only the first
`max_text_bytes` of text are read, PDFs are capped at a few pages, images are size-limited."""
from __future__ import annotations

import base64
import html
import logging
import mimetypes
import re
import shutil
import subprocess
import zipfile
from dataclasses import dataclass, field
from pathlib import Path

log = logging.getLogger("aiengine.extract")

MAX_IMAGE_BYTES = 15 * 1024 * 1024
_TEXT_MIMES = {
    "application/json", "application/xml", "application/yaml", "application/toml", "application/sql",
    "application/javascript", "application/x-sh", "application/x-yaml",
}
_TEXT_EXTS = {
    "md", "markdown", "rs", "py", "c", "h", "cpp", "hpp", "go", "java", "js", "ts", "tsx", "jsx", "sh", "nix", "toml",
    "yaml", "yml", "json", "xml", "sql", "csv", "log", "ini", "cfg", "conf", "txt", "html", "htm", "css", "rb", "php", "lua",
}


class ExtractError(Exception):
    def __init__(self, code: str, message: str):
        super().__init__(message)
        self.code, self.message = code, message


@dataclass
class Extracted:
    kind: str                     # text | pdf | docx | image | audio | binary
    mime: str
    text: str = ""
    image_b64: str | None = None
    warnings: list[str] = field(default_factory=list)


def sniff(head: bytes) -> str | None:
    checks = [
        (b"\x89PNG\r\n\x1a\n", "image/png"), (b"\xff\xd8\xff", "image/jpeg"), (b"GIF8", "image/gif"),
        (b"%PDF-", "application/pdf"), (b"\x7fELF", "application/x-elf"), (b"PK\x03\x04", "application/zip"),
        (b"\x1f\x8b", "application/gzip"), (b"fLaC", "audio/flac"), (b"OggS", "audio/ogg"), (b"ID3", "audio/mpeg"),
    ]
    for magic, mime in checks:
        if head.startswith(magic):
            return mime
    if head[:4] == b"RIFF" and head[8:12] == b"WEBP":
        return "image/webp"
    if head[4:8] == b"ftyp":
        return "video/mp4"
    if head and b"\0" not in head:
        try:
            head.decode("utf-8")
            return "text/plain"
        except UnicodeDecodeError:
            return None
    return None


def _ext(path: Path) -> str:
    return path.suffix.lower().lstrip(".")


def _read_text(path: Path, limit: int) -> str:
    with path.open("rb") as f:
        raw = f.read(limit)
    return raw.decode("utf-8", errors="replace").replace("\0", "")


def _pdf_text(path: Path, limit: int, warnings: list[str]) -> str:
    try:
        from pypdf import PdfReader  # optional
        reader = PdfReader(str(path))
        out, n = [], 0
        for page in reader.pages[:8]:
            t = page.extract_text() or ""
            out.append(t)
            n += len(t)
            if n >= limit:
                break
        return "\n".join(out)[:limit]
    except ImportError:
        pass
    except Exception as e:  # noqa: BLE001 - corrupt/encrypted PDFs must not kill the request
        warnings.append(f"pdf parse failed: {e}")
        return ""
    if shutil.which("pdftotext"):
        try:
            r = subprocess.run(["pdftotext", "-l", "8", "-q", str(path), "-"], capture_output=True, timeout=30, check=False)
            return r.stdout.decode("utf-8", errors="replace")[:limit]
        except (subprocess.SubprocessError, OSError) as e:
            warnings.append(f"pdftotext failed: {e}")
            return ""
    warnings.append("pdf text extraction unavailable (pip install pypdf, or install poppler-utils)")
    return ""


def _docx_text(path: Path, limit: int, warnings: list[str]) -> str:
    try:
        with zipfile.ZipFile(path) as z:
            xml = z.read("word/document.xml")[: limit * 20].decode("utf-8", errors="replace")
    except (zipfile.BadZipFile, KeyError, OSError) as e:
        warnings.append(f"docx parse failed: {e}")
        return ""
    return html.unescape(" ".join(re.findall(r"<w:t[^>]*>([^<]*)</w:t>", xml)))[:limit]


_whisper = None


def _transcribe(path: Path, model: str, limit: int, warnings: list[str]) -> str:
    global _whisper
    if not model:
        warnings.append("audio transcription disabled (set ai.whisper_model and pip install faster-whisper)")
        return ""
    try:
        from faster_whisper import WhisperModel  # optional, heavy
    except ImportError:
        warnings.append("faster-whisper is not installed; audio analysed by file name only")
        return ""
    try:
        if _whisper is None:
            _whisper = WhisperModel(model, device="cpu", compute_type="int8")
        segments, _info = _whisper.transcribe(str(path), vad_filter=True)
        out, n = [], 0
        for s in segments:
            out.append(s.text.strip())
            n += len(out[-1])
            if n >= limit:
                break
        return " ".join(out)[:limit]
    except Exception as e:  # noqa: BLE001
        warnings.append(f"transcription failed: {e}")
        return ""


def extract(path: str, mime_hint: str | None, max_text_bytes: int, whisper_model: str = "") -> Extracted:
    p = Path(path)
    try:
        st = p.stat()
    except FileNotFoundError:
        raise ExtractError("file_not_found", f"{path}: no such file") from None
    except OSError as e:
        raise ExtractError("file_unreadable", f"{path}: {e}") from None
    if not p.is_file():
        raise ExtractError("not_a_file", f"{path}: not a regular file")

    try:
        with p.open("rb") as f:
            head = f.read(8192)
    except OSError as e:
        raise ExtractError("file_unreadable", f"{path}: {e}") from None

    guess = mimetypes.guess_type(p.name)[0]
    sniffed = sniff(head)
    generic = (None, "application/octet-stream", "text/plain")
    mime = mime_hint if mime_hint not in generic else (sniffed if sniffed and sniffed != "text/plain" else (mime_hint or guess or sniffed or "application/octet-stream"))
    if mime in generic and sniffed:
        mime = sniffed
    ext = _ext(p)
    w: list[str] = []

    if mime.startswith("image/") and mime != "image/svg+xml":
        if st.st_size > MAX_IMAGE_BYTES:
            w.append("image too large for the vision model; analysed by name only")
            return Extracted("image", mime, warnings=w)
        return Extracted("image", mime, image_b64=base64.b64encode(p.read_bytes()).decode("ascii"), warnings=w)
    if mime == "application/pdf":
        return Extracted("pdf", mime, _pdf_text(p, max_text_bytes, w), warnings=w)
    if ext == "docx" or "wordprocessingml" in mime:
        return Extracted("docx", mime, _docx_text(p, max_text_bytes, w), warnings=w)
    if mime.startswith(("audio/", "video/")):
        return Extracted("audio", mime, _transcribe(p, whisper_model, max_text_bytes, w), warnings=w)
    if mime.startswith("text/") or mime in _TEXT_MIMES or mime.endswith(("+xml", "+json")) or ext in _TEXT_EXTS:
        return Extracted("text", mime, _read_text(p, max_text_bytes), warnings=w)
    return Extracted("binary", mime, warnings=w)
