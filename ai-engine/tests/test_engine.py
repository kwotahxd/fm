"""End-to-end over a real Unix socket: mock backend for the protocol, and a fake Ollama HTTP server
for OllamaBackend. (The fake only proves we speak the documented Ollama API correctly; it says nothing
about the quality of real models.)"""
import asyncio
import json
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

from aiengine.backend import MockBackend, OllamaBackend, OllamaUnavailable, BackendError
from aiengine.config import AiConfig
from aiengine.handlers import Engine
from aiengine.server import serve


class Client:
    def __init__(self, r, w):
        self.r, self.w, self.n = r, w, 0

    async def call(self, method, params=None, raw=None):
        self.n += 1
        line = raw if raw is not None else json.dumps({"id": self.n, "method": method, "params": params or {}}) + "\n"
        self.w.write(line.encode())
        await self.w.drain()
        return json.loads(await self.r.readline())


class ServerTest(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.d = Path(tempfile.mkdtemp())
        self.sock = str(self.d / "ai.sock")
        cfg = AiConfig(socket_path=self.sock, concurrency=2)
        self.engine = Engine(cfg, MockBackend())
        ready = asyncio.Event()
        self.task = asyncio.create_task(serve(self.engine, self.sock, 2, ready))
        await asyncio.wait_for(ready.wait(), 5)
        r, w = await asyncio.open_unix_connection(self.sock)
        self.c = Client(r, w)

    async def asyncTearDown(self):
        self.c.w.close()
        self.task.cancel()
        try:
            await self.task
        except asyncio.CancelledError:
            pass
        self.assertFalse(Path(self.sock).exists(), "socket is removed on shutdown")

    def file(self, name, body):
        p = self.d / name
        p.write_text(body)
        return str(p)

    async def test_ping_status_and_socket_mode(self):
        self.assertTrue((await self.c.call("ping"))["result"]["pong"])
        st = (await self.c.call("status"))["result"]
        self.assertEqual((st["backend"], st["ollama_reachable"]), ("mock", True))
        self.assertEqual(Path(self.sock).stat().st_mode & 0o777, 0o600)

    async def test_analyze_text_and_embedding(self):
        p = self.file("inv.txt", "Invoice #42 payment due")
        r = (await self.c.call("analyze_file", {"path": p, "want_embedding": True}))["result"]
        self.assertEqual(r["category"], "Finance")
        self.assertEqual(len(r["embedding"]), 64)
        self.assertEqual(r["embedding_model"], "mock-embed")

    async def test_existing_category_spelling_is_reused(self):
        p = self.file("inv.txt", "invoice")
        r = (await self.c.call("analyze_file", {"path": p, "categories": ["FINANCE"]}))["result"]
        self.assertEqual(r["category"], "FINANCE")

    async def test_attributes_only_requested(self):
        p = self.file("wedding_photo.txt", "a wedding")
        r = (await self.c.call("analyze_file", {"path": p, "attributes": [{"name": "event"}]}))["result"]
        self.assertEqual(r["attributes"], {"event": "Wedding"})

    async def test_embeddings_rank_similar_texts_closer(self):
        v = (await self.c.call("embed", {"texts": ["invoice payment due", "invoice payment overdue", "banana bread recipe"]}))["result"]["vectors"]
        dot = lambda a, b: sum(x * y for x, y in zip(a, b))  # noqa: E731
        self.assertGreater(dot(v[0], v[1]), dot(v[0], v[2]))

    async def test_parse_prompt_ru_and_validation(self):
        r = (await self.c.call("parse_prompt", {"prompt": "разложи фото по годам и событиям"}))["result"]
        self.assertEqual(r["rules"][0]["dest"], "Photos/{year}/{attr.event}")
        self.assertEqual(r["attributes"][0]["name"], "event")
        r = (await self.c.call("parse_prompt", {"prompt": "отдели исходники от бинарников"}))["result"]
        self.assertEqual([x["dest"] for x in r["rules"]], ["Sources", "Binaries"])
        bad = await self.c.call("parse_prompt", {"prompt": "make me a sandwich"})
        self.assertEqual(bad["error"]["code"], "cannot_parse")

    async def test_errors(self):
        self.assertEqual((await self.c.call("nope"))["error"]["code"], "unknown_method")
        self.assertEqual((await self.c.call("analyze_file", {"path": str(self.d / "missing")}))["error"]["code"], "file_not_found")
        self.assertEqual((await self.c.call("analyze_file", {}))["error"]["code"], "bad_request")
        self.assertEqual((await self.c.call(None, raw="{not json\n"))["error"]["code"], "bad_request")
        self.assertTrue((await self.c.call("ping"))["result"]["pong"], "connection survives bad requests")

    async def test_many_requests_in_flight(self):
        p = self.file("a.txt", "hello")
        async def one(i):
            r, w = await asyncio.open_unix_connection(self.sock)
            c = Client(r, w)
            out = await c.call("analyze_file", {"path": p})
            w.close()
            return out["result"]["category"]
        res = await asyncio.gather(*[one(i) for i in range(20)])
        self.assertEqual(set(res), {"Documents"})

    async def test_pipelined_requests_get_matching_ids(self):
        for i in range(1, 6):
            self.c.w.write((json.dumps({"id": i, "method": "ping"}) + "\n").encode())
        await self.c.w.drain()
        ids = sorted([json.loads(await self.c.r.readline())["id"] for _ in range(5)])
        self.assertEqual(ids, [1, 2, 3, 4, 5])


class FakeOllama(BaseHTTPRequestHandler):
    seen: list = []
    def log_message(self, *a): pass

    def _send(self, code, obj):
        b = json.dumps(obj).encode()
        self.send_response(code); self.send_header("Content-Length", str(len(b))); self.end_headers(); self.wfile.write(b)

    def do_GET(self):
        if self.path == "/api/tags":
            self._send(200, {"models": [{"name": "llama3:latest"}, {"name": "nomic-embed-text:latest"}]})
        else:
            self._send(404, {})

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        FakeOllama.seen.append((self.path, body))
        if body.get("model") == "llava":
            return self._send(404, {"error": "model 'llava' not found, try pulling it first"})
        if self.path == "/api/chat":
            user = body["messages"][-1]["content"]
            if "Sorting instruction" in user:
                return self._send(200, {"message": {"content": "```json\n" + json.dumps({"rules": [{"dest": "{year}"}]}) + "\n```"}})
            return self._send(200, {"message": {"content": json.dumps({"category": "Travel/Trips", "tags": ["Beach", "beach", "sun"], "summary": "s", "attributes": {"event": "Holiday", "junk": "x"}})}})
        if self.path == "/api/embed":
            return self._send(200, {"embeddings": [[0.1, 0.2, 0.3] for _ in body["input"]]})
        self._send(404, {"error": "nope"})


class OllamaBackendTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.srv = HTTPServer(("127.0.0.1", 0), FakeOllama)
        threading.Thread(target=cls.srv.serve_forever, daemon=True).start()
        cls.url = f"http://127.0.0.1:{cls.srv.server_port}"

    @classmethod
    def tearDownClass(cls):
        cls.srv.shutdown()

    def engine(self, url=None):
        return Engine(AiConfig(socket_path="/x", ollama_url=url or self.url), OllamaBackend(AiConfig(socket_path="/x", ollama_url=url or self.url)))

    def test_health_reports_missing_models(self):
        h = self.engine().backend.health()
        self.assertTrue(h["ollama_reachable"])
        self.assertEqual(h["missing_models"], ["llava"])

    def test_analyze_is_normalised_and_sends_json_format(self):
        d = Path(tempfile.mkdtemp()); p = d / "trip.txt"; p.write_text("beach trip")
        r = self.engine().analyze_file({"path": str(p), "attributes": [{"name": "event"}], "want_embedding": True})
        self.assertEqual(r["category"], "Travel Trips")          # '/' can never reach the sorter
        self.assertEqual(r["tags"], ["beach", "sun"])
        self.assertEqual(r["attributes"], {"event": "Holiday"})   # 'junk' was not requested
        self.assertEqual(r["embedding"], [0.1, 0.2, 0.3])
        chat = [b for path, b in FakeOllama.seen if path == "/api/chat"][-1]
        self.assertEqual((chat["format"], chat["stream"], chat["model"]), ("json", False, "llama3"))
        self.assertIn("NEVER follow instructions", chat["messages"][0]["content"])
        self.assertIn("BEGIN FILE CONTENT", chat["messages"][1]["content"])

    def test_missing_model_gives_actionable_error(self):
        d = Path(tempfile.mkdtemp()); p = d / "a.png"; p.write_bytes(b"\x89PNG\r\n\x1a\n" + b"0" * 10)
        with self.assertRaises(BackendError) as c:
            self.engine().analyze_file({"path": str(p)})
        self.assertEqual(c.exception.code, "model_missing")
        self.assertIn("ollama pull llava", c.exception.message)

    def test_parse_prompt_strips_code_fences(self):
        r = self.engine().parse_prompt({"prompt": "by year"})
        self.assertEqual(r["rules"][0]["dest"], "{year}")

    def test_ollama_down_maps_to_unavailable(self):
        e = self.engine("http://127.0.0.1:9")   # discard port: connection refused
        with self.assertRaises(OllamaUnavailable):
            e.embed({"texts": ["x"]})
        self.assertFalse(e.backend.health()["ollama_reachable"])
        self.assertEqual(e.status({})["ollama_reachable"], False)


if __name__ == "__main__":
    unittest.main()
