"""Validation of LLM-produced sorting rule sets. Never trust model output: this mirrors the checks in the
Rust sorter (relative destinations, known placeholders) so a bad rule is rejected *before* it reaches the disk layer."""
from __future__ import annotations

import re

PLACEHOLDERS = ("name", "stem", "ext", "year", "month", "day", "mime", "mime_major", "kind", "category", "tag", "size_kb")
_ATTR_NAME = re.compile(r"^[A-Za-z][A-Za-z0-9_]{0,31}$")
_VAR = re.compile(r"\{([^{}]*)\}")
_FILTER_LISTS = ("mime_prefix", "ext", "category_in", "tag_any")

SCHEMA_DOC = """\
Return ONE JSON object (no prose) describing a file-sorting rule set:
{
  "name": "short-kebab-name",
  "description": "one sentence",
  "attributes": [{"name": "event", "description": "what to extract per file"}],   // only if the rule needs per-file semantic info
  "rules": [                                                                        // evaluated in order, first match wins
    {
      "name": "optional",
      "filter": {                                                                  // every field optional, all given fields must match
        "mime_prefix": ["image/"], "ext": ["rs","py"], "min_size": 0, "max_size": 1000000,
        "category_in": ["Finance"], "tag_any": ["invoice"], "attr_equals": {"kind": "source"}
      },
      "dest": "Photos/{year}/{attr.event}",                                        // destination DIRECTORY, relative, never '..'
      "rename": "{year}-{month}-{day}_{stem}.{ext}"                                // optional new file name
    }
  ]
}
Placeholders: {name} {stem} {ext} {year} {month} {day} {mime} {mime_major} {kind} {category} {tag} {size_kb} {attr.<attribute>}.
{year}/{month}/{day} come from the file's modification time. {kind} is one of Images, Video, Audio, Documents, Code, Archives, Binaries, Other.
Prefer cheap metadata (ext, mime_prefix, {year}, {kind}) over AI attributes; declare an attribute only when the request needs semantic per-file information (e.g. an event, a topic, a person).
Files that match no rule are left where they are.
"""


def _check_template(t: str, declared: set[str], used: set[str], what: str) -> None:
    depth = 0
    for ch in t:
        depth += (ch == "{") - (ch == "}")
        if depth not in (0, 1):
            raise ValueError(f"{what}: unbalanced braces in '{t}'")
    if depth:
        raise ValueError(f"{what}: unbalanced braces in '{t}'")
    for v in _VAR.findall(t):
        if v in PLACEHOLDERS:
            continue
        if v.startswith("attr.") and _ATTR_NAME.match(v[5:]):
            used.add(v[5:])
            continue
        raise ValueError(f"{what}: unknown placeholder {{{v}}}")


def validate_ruleset(raw: object) -> dict:
    """Return a normalised rule set dict or raise ValueError with a message suitable for feeding back to the model."""
    if not isinstance(raw, dict):
        raise ValueError("top level must be a JSON object")
    rules_in = raw.get("rules")
    if not isinstance(rules_in, list) or not rules_in:
        raise ValueError("'rules' must be a non-empty list")
    if len(rules_in) > 20:
        raise ValueError("too many rules (max 20)")

    attrs: dict[str, str] = {}
    for a in raw.get("attributes") or []:
        if not isinstance(a, dict) or not _ATTR_NAME.match(str(a.get("name", ""))):
            raise ValueError("attribute names must match [A-Za-z][A-Za-z0-9_]* (max 32 chars)")
        attrs[a["name"]] = str(a.get("description", ""))[:200]

    used: set[str] = set()
    rules = []
    for i, r in enumerate(rules_in):
        w = f"rules[{i}]"
        if not isinstance(r, dict):
            raise ValueError(f"{w} must be an object")
        dest = r.get("dest")
        if not isinstance(dest, str) or not dest.strip():
            raise ValueError(f"{w}.dest must be a non-empty string")
        if dest.startswith("/") or "\0" in dest or any(c == ".." for c in dest.split("/")):
            raise ValueError(f"{w}.dest must be relative and must not contain '..'")
        _check_template(dest, set(attrs), used, f"{w}.dest")
        rename = r.get("rename")
        if rename is not None:
            if not isinstance(rename, str):
                raise ValueError(f"{w}.rename must be a string")
            _check_template(rename, set(attrs), used, f"{w}.rename")

        f_in = r.get("filter") or {}
        if not isinstance(f_in, dict):
            raise ValueError(f"{w}.filter must be an object")
        f: dict = {}
        for k in _FILTER_LISTS:
            if k in f_in and f_in[k] is not None:
                if not isinstance(f_in[k], list) or not all(isinstance(x, str) for x in f_in[k]):
                    raise ValueError(f"{w}.filter.{k} must be a list of strings")
                f[k] = f_in[k]
        for k in ("min_size", "max_size"):
            if f_in.get(k) is not None:
                if not isinstance(f_in[k], int) or isinstance(f_in[k], bool) or f_in[k] < 0:
                    raise ValueError(f"{w}.filter.{k} must be a non-negative integer")
                f[k] = f_in[k]
        ae = f_in.get("attr_equals")
        if ae:
            if not isinstance(ae, dict) or not all(isinstance(k, str) and isinstance(v, str) for k, v in ae.items()):
                raise ValueError(f"{w}.filter.attr_equals must be an object of strings")
            for k in ae:
                if not _ATTR_NAME.match(k):
                    raise ValueError(f"{w}.filter.attr_equals: bad attribute name '{k}'")
                used.add(k)
            f["attr_equals"] = ae
        rule = {"name": str(r.get("name", ""))[:60], "filter": f, "dest": dest.strip()}
        if rename:
            rule["rename"] = rename
        rules.append(rule)

    for u in sorted(used - set(attrs)):  # the model used an attribute without declaring it: declare it
        attrs[u] = ""
    return {
        "name": re.sub(r"[^A-Za-z0-9_-]+", "-", str(raw.get("name") or "prompt-rule")).strip("-")[:40] or "prompt-rule",
        "description": str(raw.get("description", ""))[:300],
        "attributes": [{"name": k, "description": v} for k, v in attrs.items()],
        "rules": rules,
    }
