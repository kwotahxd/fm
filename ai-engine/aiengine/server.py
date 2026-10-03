"""asyncio Unix-socket server: newline-delimited JSON, many requests in flight per connection."""
from __future__ import annotations

import asyncio
import contextlib
import json
import logging
import os
import socket
import stat
from pathlib import Path

from .backend import BackendError
from .handlers import Engine, ProtocolError

log = logging.getLogger("aiengine.server")
MAX_LINE = 16 * 1024 * 1024


def _err(id_, code: str, message: str) -> bytes:
    return (json.dumps({"id": id_, "error": {"code": code, "message": message}}, ensure_ascii=False) + "\n").encode()


async def _handle(reader: asyncio.StreamReader, writer: asyncio.StreamWriter, engine: Engine, sem: asyncio.Semaphore) -> None:
    wlock = asyncio.Lock()
    tasks: set[asyncio.Task] = set()

    async def reply(data: bytes) -> None:
        async with wlock:
            writer.write(data)
            await writer.drain()

    async def process(line: bytes) -> None:
        id_ = None
        try:
            req = json.loads(line)
            if not isinstance(req, dict):
                raise ProtocolError("bad_request", "request must be a JSON object")
            id_ = req.get("id")
            method, params = req.get("method"), req.get("params") or {}
            if not isinstance(method, str) or not isinstance(params, dict):
                raise ProtocolError("bad_request", "'method' must be a string and 'params' an object")
            if method in Engine.HEAVY:
                async with sem:  # bound the number of concurrent model calls
                    result = await asyncio.to_thread(engine.call, method, params)
            else:
                result = await asyncio.to_thread(engine.call, method, params)
            await reply((json.dumps({"id": id_, "result": result}, ensure_ascii=False) + "\n").encode())
        except json.JSONDecodeError as e:
            await reply(_err(None, "bad_request", f"invalid JSON: {e}"))
        except (ProtocolError, BackendError) as e:
            await reply(_err(id_, e.code, e.message))
        except (ConnectionError, asyncio.CancelledError):
            raise
        except Exception as e:  # noqa: BLE001 - a bug in one request must not kill the server
            log.exception("internal error")
            await reply(_err(id_, "internal_error", f"{type(e).__name__}: {e}"))

    try:
        while True:
            try:
                line = await reader.readline()
            except (asyncio.LimitOverrunError, ValueError):
                await reply(_err(None, "bad_request", "request line too long"))
                break
            if not line:
                break
            if not line.strip():
                continue
            t = asyncio.create_task(process(line))
            tasks.add(t)
            t.add_done_callback(tasks.discard)
    except ConnectionError:
        pass
    finally:
        for t in tasks:
            t.cancel()
        with contextlib.suppress(Exception):
            writer.close()
            await writer.wait_closed()


def _prepare_socket(path: str) -> None:
    p = Path(path)
    p.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    if p.exists() or p.is_symlink():
        if not stat.S_ISSOCK(p.lstat().st_mode):
            raise RuntimeError(f"{path} exists and is not a socket; refusing to remove it")
        probe = socket.socket(socket.AF_UNIX)
        try:
            probe.connect(path)
        except (ConnectionRefusedError, FileNotFoundError):
            p.unlink()  # stale socket from a crashed run
        else:
            raise RuntimeError(f"another engine is already listening on {path}")
        finally:
            probe.close()


async def serve(engine: Engine, socket_path: str, concurrency: int, ready: asyncio.Event | None = None) -> None:
    _prepare_socket(socket_path)
    sem = asyncio.Semaphore(max(1, concurrency))
    server = await asyncio.start_unix_server(lambda r, w: _handle(r, w, engine, sem), path=socket_path, limit=MAX_LINE)
    os.chmod(socket_path, 0o600)  # same-user access only: the engine reads arbitrary paths on request
    log.info("listening on %s (backend=%s, concurrency=%d)", socket_path, engine.backend.name, concurrency)
    if ready:
        ready.set()
    try:
        async with server:
            await server.serve_forever()
    finally:
        with contextlib.suppress(FileNotFoundError):
            os.unlink(socket_path)
