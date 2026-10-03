"""Reads the same TOML file as the Rust side (sections [general] and [ai])."""
from __future__ import annotations

import os
import tomllib
from dataclasses import dataclass
from pathlib import Path


def default_socket_path() -> str:
    rt = os.environ.get("XDG_RUNTIME_DIR")
    if rt:
        return str(Path(rt) / "fm" / "ai.sock")
    return f"/tmp/fm-{os.getuid()}/ai.sock"


def _expand(p: str) -> str:
    return str(Path(p).expanduser()) if p else p


@dataclass
class AiConfig:
    socket_path: str = ""
    ollama_url: str = "http://127.0.0.1:11434"
    text_model: str = "llama3"
    vision_model: str = "llava"
    embed_model: str = "nomic-embed-text"
    request_timeout_s: int = 180
    concurrency: int = 2
    max_text_bytes: int = 6000
    whisper_model: str = ""
    log_level: str = "info"

    def __post_init__(self) -> None:
        self.socket_path = _expand(self.socket_path) or default_socket_path()


def load(path: str | None = None) -> AiConfig:
    """explicit path → $FM_CONFIG → ~/.config/fm/config.toml → defaults. `FM_OLLAMA_URL` overrides the URL."""
    candidates = [path, os.environ.get("FM_CONFIG"), str(Path("~/.config/fm/config.toml").expanduser())]
    data: dict = {}
    for i, c in enumerate(candidates):
        if not c:
            continue
        p = Path(c)
        if p.is_file():
            data = tomllib.loads(p.read_text(encoding="utf-8"))
            break
        if i < 2:  # explicitly requested but missing
            raise FileNotFoundError(f"config file not found: {c}")
    ai = dict(data.get("ai", {}))
    ai.pop("enabled", None)
    known = {f for f in AiConfig.__dataclass_fields__}
    kwargs = {k: v for k, v in ai.items() if k in known}
    kwargs["log_level"] = data.get("general", {}).get("log_level", "info").split(",")[-1] if "general" in data else "info"
    cfg = AiConfig(**kwargs)
    if os.environ.get("FM_OLLAMA_URL"):
        cfg.ollama_url = os.environ["FM_OLLAMA_URL"]
    return cfg
