# fm — modular file manager with local AI sorting

[Russian/Русский](docs/README_rus.md)

A Linux file manager built as a "constructor": every module is independent, replaceable, and
talks to the others through a small, explicit interface. Directory walking is done with raw
syscalls (no shelling out to `ls`/`find`/`stat`); AI classification, embeddings and natural-language
sorting run entirely locally through [Ollama](https://ollama.com), with no other module blocked
if the AI is unavailable.

## Architecture

```
                          ┌────────────────────────────────────────┐
                          │                  CLI (fm)              │   ← crates/fm-cli
                          │   ls · index · analyze · find · search │      thin: calls fm-app only
                          │   rules · sort · undo · watch · status │
                          └───────────────────┬────────────────────┘
                                              │
                          ┌───────────────────▼──────────────────────┐
                          │                fm-app                    │   the only crate that
                          │   wires the modules below together,      │   knows about more than
                          │   decides AI-degradation, owns caches    │   one other module
                          └──┬───────┬─────────┬────────────┬────────┘
                             │       │         │            │
             ┌───────────────▼───┐ ┌─▼──────┐ ┌▼────────┐ ┌─▼─────────┐
             │     fm-core       │ │ fm-db  │ │fm-sorter│ │fm-watcher │
             │ getdents64,       │ │ SQLite │ │ plan /  │ │ inotify,  │
             │ fstatat, walk,    │ │ behind │ │ execute │ │ debounced │
             │ metadata cache,   │ │ a trait│ │ / undo  │ │ batches   │
             │ mime sniff, hash  │ │(Postgre│ │(Rust)   │ │(Rust)     │
             │ (Rust)            │ │ swap-  │ └─────────┘ └───────────┘
             └───────────────────┘ │ able)  │
                                   └────────┘
                             │
                          ┌──▼─────────────────────────┐        Unix socket / NDJSON
                          │           fm-ai            │◄─────────────────────────┐
                          │  Rust client for the AI    │                          │
                          │  engine's RPC protocol     │                          │
                          └────────────────────────────┘                          │
                                                                                  │
                          ┌───────────────────────────────────────────────────────▼────┐
                          │                    ai-engine (Python, asyncio)             │
                          │  extract → backend.classify/embed/parse_prompt → validate  │
                          │  content extraction: text/pdf/docx/image/audio(whisper)    │
                          │  backends: OllamaBackend (llama3/llava/nomic-embed-text)   │
                          │            MockBackend (deterministic, offline, for tests) │
                          └───────────────────────────┬────────────────────────────────┘
                                                      │ HTTP
                                                  ┌───▼──────┐
                                                  │  Ollama  │
                                                  └──────────┘
```

Module boundaries (also see `docs/architecture.md` for the full interface listing):

| Crate         | Responsibility                                                       | Depends on           |
|---------------|-----------------------------------------------------------------------|-----------------------|
| `fm-types`    | Shared data types + the AI wire protocol types                        | —                     |
| `fm-config`   | TOML config, shared with the Python engine                            | —                     |
| `fm-core`     | Raw-syscall directory walking, metadata cache, MIME/hash, thread pool | `fm-types`* (none really — see note) |
| `fm-db`       | `Database` trait + SQLite implementation, migrations                  | `fm-types`, `fm-config` |
| `fm-sorter`   | Templates, plan/execute/undo, conflict resolution                     | `fm-types`            |
| `fm-watcher`  | Recursive inotify watcher with debounced batches                      | `fm-core`             |
| `fm-ai`       | Blocking client for the AI engine's Unix-socket protocol              | `fm-types`            |
| `fm-app`      | Wires everything together; the only cross-module crate                | all of the above      |
| `fm-cli`      | Argument parsing + printing; calls `fm-app` only                      | `fm-app`              |
| `ai-engine`   | Python asyncio service: content extraction, Ollama bridge, prompt→rules | (stdlib only)       |

`fm-core` has no dependency on `fm-types` in practice (it defines its own `Meta`/`FileKind` since
those describe *filesystem* concepts, not index concepts) — this is deliberate: the walker must
stay usable even if the index/AI types change shape.

## Building

Requires Rust ≥ 1.75 and Python ≥ 3.11.

```bash
cargo build --release                 # builds fm-types .. fm-cli; binary at "target/release/fm"
cd ai-engine && pip install -e .      # or: pip install -e '.[pdf,audio]' for PDF/audio extraction
```

## Running

```bash
# 1. start Ollama (native install, or `docker compose -f docker/docker-compose.yml up -d ollama`)
ollama pull llama3
ollama pull llava
ollama pull nomic-embed-text

# 2. start the AI engine (reads ~/.config/fm/config.toml, or $FM_CONFIG)
cp config/config.example.toml ~/.config/fm/config.toml
python -m aiengine &

# 3. use the CLI — everything works even before step 1/2 finish; AI features degrade gracefully
fm index ~/Documents
fm analyze ~/Documents
fm find --category Finance
fm search "tax documents from last year"
fm rules
fm sort ~/Documents --rule by-type                 # dry-run, no changes done 
fm sort ~/Documents --rule by-type --apply
fm sort ~/Pictures --prompt "sort photos by year and event" --apply
fm undo
fm watch ~/Documents
fm status
```

No Ollama yet, or just want to try the plumbing? `python -m aiengine --mock` runs a deterministic,
offline backend (keyword-based categorisation, hashed bag-of-words embeddings) — used by the
integration tests, and handy for kicking the tires without downloading models.

## Testing

```bash
cargo test --workspace                                 # unit + integration tests (Rust)
cd ai-engine && python -m unittest discover -s tests    # unit + end-to-end tests (Python)
```

`crates/fm-app/tests/integration_ai.rs` is the Core+DB+AI+Sorter integration suite: it spawns the
*real* Python engine (`--mock` backend, no network) as a child process and drives it over the
actual Unix-socket protocol — indexing, incremental re-analysis, semantic search, prompt-based
sorting, apply, and undo, end to end.

## Performance

See `bench/README.md` for the directory-listing benchmark methodology and reproduction steps
against `ls -la --color=never` (target: within 10-15% of `ls`'s time on a 100,000-file directory;
measured result in the sandboxed CI environment here is roughly 2.4× *faster*, mostly because
GNU `ls -l` resolves every uid/gid to a name via NSS and fm-core does not).

## Graceful degradation

`fm-app::App::ai_available()` pings the engine (cached briefly) before any AI-dependent call.
When it's down: `fm index`, `fm ls`, `fm find`, built-in `fm sort --rule by-type/by-date/by-ext`,
`fm undo`, and `fm watch` (without `auto_analyze`) all keep working. `fm analyze`, `fm search`,
and `fm sort --prompt` (or `--rule by-category` before anything has been analysed) return a
clear `AiUnavailable` error with a hint instead of hanging or crashing.

## Extending

- **New rule sets**: add a JSON `RuleSet` via `fm sort --prompt "..." --save-as my-rule` (the AI
  parses and validates it), or write one directly and `INSERT` it into `sort_rules` — `fm-sorter`
  only ever sees the validated `RuleSet` struct.
- **PostgreSQL backend**: implement `fm_db::Database` for it and add a match arm in `fm_db::open`;
  every other crate already talks to `dyn Database`, not SQLite directly.
- **New AI backend / model**: implement `aiengine.backend.Backend`; `handlers.Engine` and the RPC
  layer are backend-agnostic (see `MockBackend` for the minimal shape).
- **GUI**: call `fm_app::App` directly, the same way `fm-cli` does — no CLI parsing required.
