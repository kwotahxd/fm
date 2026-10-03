from __future__ import annotations

import argparse
import asyncio
import logging
import os
import signal

from . import __version__
from .backend import make_backend
from .config import load
from .handlers import Engine
from .server import serve


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(prog="aiengine", description="AI engine for the modular file manager")
    ap.add_argument("--config", help="TOML config (default: $FM_CONFIG or ~/.config/fm/config.toml)")
    ap.add_argument("--socket", help="override ai.socket_path")
    ap.add_argument("--mock", action="store_true", default=os.environ.get("FM_AI_MOCK") == "1",
                    help="deterministic offline backend (tests / CI); no Ollama needed")
    ap.add_argument("--log-level", help="debug|info|warning|error (default: [general].log_level)")
    ap.add_argument("--version", action="version", version=__version__)
    a = ap.parse_args(argv)

    cfg = load(a.config)
    if a.socket:
        cfg.socket_path = a.socket
    level = (a.log_level or os.environ.get("FM_LOG") or cfg.log_level or "info").upper()
    logging.basicConfig(level=getattr(logging, level, logging.INFO), format="%(asctime)s %(levelname)-7s %(name)s: %(message)s")
    engine = Engine(cfg, make_backend(cfg, a.mock))
    if not a.mock:
        logging.getLogger("aiengine").info("ollama=%s text=%s vision=%s embed=%s", cfg.ollama_url, cfg.text_model, cfg.vision_model, cfg.embed_model)

    async def run() -> None:
        loop = asyncio.get_running_loop()
        task = asyncio.create_task(serve(engine, cfg.socket_path, cfg.concurrency))
        for sig in (signal.SIGINT, signal.SIGTERM):
            loop.add_signal_handler(sig, task.cancel)
        try:
            await task
        except asyncio.CancelledError:
            pass

    try:
        asyncio.run(run())
    except RuntimeError as e:
        logging.getLogger("aiengine").error("%s", e)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
