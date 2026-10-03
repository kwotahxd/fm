"""RPC methods. Each is synchronous (blocking HTTP / file IO); the server runs them in worker threads."""
from __future__ import annotations

import logging
import os
import re

from . import __version__
from .backend import AnalysisContext, Backend, BackendError, OllamaUnavailable
from .config import AiConfig
from .extract import ExtractError, extract
from .rules import validate_ruleset

log = logging.getLogger("aiengine.handlers")


class ProtocolError(Exception):
    def __init__(self, code: str, message: str):
        super().__init__(message)
        self.code, self.message = code, message


def normalize_analysis(raw: dict, requested: list[dict], known: list[str]) -> dict:
    """Model output is untrusted: clamp lengths, strip path separators, keep only requested attributes."""
    cat = re.sub(r"\s+", " ", str(raw.get("category", "")).replace("/", " ").replace("\\", " ")).strip()[:40]
    if not cat or cat in {".", ".."}:
        cat = "Uncategorized"
    for k in known:  # reuse the spelling of an existing category
        if k.lower() == cat.lower():
            cat = k
            break
    tags: list[str] = []
    raw_tags = raw.get("tags")
    for t in raw_tags if isinstance(raw_tags, list) else []:
        t = re.sub(r"\s+", " ", str(t)).strip().lower()[:32]
        if t and t not in tags:
            tags.append(t)
    wanted = {a["name"] for a in requested}
    attrs: dict[str, str] = {}
    raw_attrs = raw.get("attributes")
    if isinstance(raw_attrs, dict):
        for k, v in raw_attrs.items():
            v = re.sub(r"\s+", " ", str(v)).strip()[:80]
            if k in wanted and v:
                attrs[k] = v
    return {"category": cat, "tags": tags[:8], "summary": str(raw.get("summary", "")).strip()[:300], "attributes": attrs}


class Engine:
    def __init__(self, cfg: AiConfig, backend: Backend):
        self.cfg, self.backend = cfg, backend

    def ping(self, _p: dict) -> dict:
        return {"pong": True, "version": __version__}

    def status(self, _p: dict) -> dict:
        return {"version": __version__, "backend": self.backend.name, **self.backend.health()}

    def analyze_file(self, p: dict) -> dict:
        path = p.get("path")
        if not isinstance(path, str) or not path:
            raise ProtocolError("bad_request", "'path' is required")
        attributes = [a for a in (p.get("attributes") or []) if isinstance(a, dict) and a.get("name")]
        categories = [str(c) for c in (p.get("categories") or [])]
        try:
            ex = extract(path, p.get("mime"), self.cfg.max_text_bytes, self.cfg.whisper_model)
        except ExtractError as e:
            raise BackendError(e.code, e.message) from None
        ctx = AnalysisContext(
            name=os.path.basename(path), mime=ex.mime, kind=ex.kind, size=os.path.getsize(path), text=ex.text,
            image_b64=ex.image_b64, categories=categories, attributes=attributes,
        )
        res = normalize_analysis(self.backend.classify(ctx), attributes, categories)
        warnings = list(ex.warnings)
        res.update({
            "model": self.backend.vision_model if ex.image_b64 else self.backend.text_model,
            "mime": ex.mime if ex.mime != p.get("mime") else None,
            "embedding": None, "embedding_model": None,
        })
        if p.get("want_embedding"):
            doc = "\n".join([ctx.name, res["category"], ", ".join(res["tags"]), res["summary"], ex.text[:1500]]).strip()
            try:
                res["embedding"] = self.backend.embed([doc])[0]
                res["embedding_model"] = self.backend.embed_model
            except OllamaUnavailable:
                raise
            except BackendError as e:  # classification succeeded: keep it, just without a vector
                warnings.append(f"embedding skipped: {e.message}")
        res["warnings"] = warnings
        return res

    def embed(self, p: dict) -> dict:
        texts = p.get("texts")
        if not isinstance(texts, list) or not texts or not all(isinstance(t, str) for t in texts):
            raise ProtocolError("bad_request", "'texts' must be a non-empty list of strings")
        return {"model": self.backend.embed_model, "vectors": self.backend.embed(texts)}

    def parse_prompt(self, p: dict) -> dict:
        prompt = p.get("prompt")
        if not isinstance(prompt, str) or not prompt.strip():
            raise ProtocolError("bad_request", "'prompt' is required")
        known = [str(c) for c in (p.get("known_categories") or [])]
        feedback = None
        for attempt in range(2):
            raw = self.backend.parse_prompt(prompt, known, feedback)
            try:
                return validate_ruleset(raw)
            except ValueError as e:
                feedback = str(e)
                log.warning("rule set rejected (attempt %d): %s", attempt + 1, e)
        raise BackendError("invalid_rule", f"the model could not produce a valid rule set: {feedback}")

    METHODS = {"ping", "status", "analyze_file", "embed", "parse_prompt"}
    # methods that hit the model and must be rate-limited by the server
    HEAVY = {"analyze_file", "embed", "parse_prompt"}

    def call(self, method: str, params: dict) -> dict:
        if method not in self.METHODS:
            raise ProtocolError("unknown_method", f"unknown method '{method}'")
        return getattr(self, method)(params)
