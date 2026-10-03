# Architecture reference

## Data flow: indexing

1. `fm index <root>` → `App::index` canonicalises the root, starts a single DB-writer thread
   reading from an `mpsc::sync_channel`, and calls `fm_core::walk` with a thread pool sized by
   `[core].threads` (0 = `nproc`).
2. Each directory visited by the walker becomes one batch of `NewFile` sent to the writer thread
   (never one row at a time — see "Minimising syscalls / DB round-trips" below).
3. The writer performs one `INSERT ... ON CONFLICT DO UPDATE` transaction per batch
   (`SqliteDb::upsert_files`), tagging every row with a `run_id` (`now_nanos()`), and preserving
   any existing AI analysis (`ON CONFLICT` only touches path/size/mtime/mode/mime/indexed_at).
4. After the walk, `prune_unseen(root, run_id)` deletes rows under `root` not touched by this run
   (i.e. files deleted since the last index) — skipped if the walk hit read errors, since a
   partially-unreadable tree must never look like "everything in it was deleted".

## Data flow: AI analysis

1. `fm analyze <root>` queries `files` for rows where `needs_analysis()` is true: no `analyzed_at`,
   or `analyzed_mtime_ns != mtime_ns` (the file changed since it was last analysed).
2. Jobs run through a small worker pool (`fm_core::spawn_pool`, sized by `[ai].concurrency`) —
   analysis is CPU/IO-light on the Rust side (hash + one RPC), so the concurrency knob mainly
   bounds how many requests Ollama sees at once.
3. Each job: `blake3` hash the file (skip if over `[index].max_analyze_mb`) → if the hash matches
   what's stored, the mtime bump was a no-op touch, so only `touch_analysis` runs (no model call)
   → otherwise call `fm-ai`'s `analyze_file`, which round-trips to the Python engine.
4. The engine (`aiengine.handlers.Engine.analyze_file`) extracts content (`aiengine.extract`),
   calls the backend's `classify` (and `embed` if requested), and **normalises** the result
   (`normalize_analysis`): category/tags/summary are length-capped and path-separator-stripped,
   and only attributes that were actually requested survive — model output is never trusted
   verbatim.
5. `save_analysis` merges the result into `files` (category upserted into `categories`,
   tags into `tags`/`file_tags`, embedding normalised and stored as a little-endian `f32` BLOB).

If the engine is unreachable mid-run, the pool stops issuing new requests (`AtomicBool` abort
flag) and `AnalyzeReport.aborted = true` is returned — files already processed keep their results,
the rest are simply left `needs_analysis()` for the next run.

## Data flow: prompt-based sorting

1. `fm sort <root> --prompt "..."` → `fm-ai::parse_prompt` sends the instruction plus known
   category names to the engine.
2. `aiengine.handlers.Engine.parse_prompt` calls the backend, then **validates** the JSON against
   `aiengine.rules.validate_ruleset` — relative destinations only, no `..`, only known placeholders
   (`{year}`, `{attr.<name>}`, …). On failure, the model gets one retry with the validation error
   as feedback; after two failures the call returns `invalid_rule`.
3. The validated `RuleSet` crosses the socket into Rust as the same struct `fm-sorter` consumes —
   there is exactly one definition of what a rule set looks like (`fm_types::RuleSet`), mirrored
   (not re-derived) by the Python-side JSON schema in `aiengine/rules.py`.
4. `fm_sorter::plan` compiles each rule's `dest`/`rename` templates once, then for every file:
   first matching rule wins; missing AI data (category/tags/attribute not yet analysed) is
   reported as a *skip reason*, never a hard error, so a partially-analysed tree still produces a
   usable plan for the files that are ready.
5. Conflict handling (`Conflict::Rename|Skip|Fail`) is resolved deterministically (files sorted by
   path first) and templated destinations are sanitised (`template::sanitize`) so a hostile or
   garbled AI attribute value can never escape the sort root or contain a path separator.
6. `fm_sorter::execute` moves files with `renameat2(RENAME_NOREPLACE)` (falling back to a checked
   `rename`/copy+delete on old kernels or cross-device moves) — a file can never be silently
   overwritten, even if something raced with the plan between dry-run and apply.
7. Every move is journalled to `sort_history` **as it happens** (`on_event` callback), not after
   the batch finishes, so a crash mid-apply still leaves a complete-enough journal for `fm undo`.

## Minimising syscalls / DB round-trips

- `fm-core::listing::read_dir`: one `open(O_DIRECTORY)`, a `getdents64` loop (buffer size from
  `[core].read_buffer_kb`), and **one `fstatat(AT_SYMLINK_NOFOLLOW)` per entry** — batched across
  threads (`ReadOpts::stat_threads`) once a directory has more than `par_threshold` entries, since
  spinning up threads for a 20-entry directory would cost more than it saves.
- Entry names live in one contiguous arena per directory (`Vec<u8>`), not one `String` allocation
  per entry.
- The walker streams whole-directory batches to the DB writer, which only flushes a transaction
  every ~2000 files (`App::index`) or at end-of-walk — not one transaction per file.
- `fm-core::cache::MetaCache` avoids re-walking a directory the CLI just listed: a cached listing
  is reused while both (a) it's younger than `[core].cache_ttl_ms` and (b) the directory's own
  mtime hasn't changed (one cheap `stat` to check) — catching adds/removes/renames without needing
  a full re-`getdents64`. `fm-watcher` also calls `MetaCache::invalidate` directly on every
  create/delete/move, so a shell open next to `fm` sees changes immediately rather than waiting
  out the TTL.

## Graceful degradation, precisely

`App::ai_available()` (`crates/fm-app/src/lib.rs`) pings the engine and caches the result for 10s
(3s on failure, so recovery is noticed quickly). `App::require_ai()` is called at the start of
every AI-dependent method (`analyze`, `search_semantic`, prompt-based `plan_sort`) and returns
`AppError::AiUnavailable` — a distinct variant from other errors — with a message that includes
the exact socket path and the command to start the engine. The CLI (`fmt::friendly`) appends a
one-line hint. Nothing upstream of `require_ai()` blocks or retries indefinitely: a single failed
`ping()` (2s timeout) is the entire cost of finding out the engine is down.

## Why SQLite is behind a trait

`fm_db::Database` has no SQLite-specific types in its signature (paths and `fm_types` structs
only). `search_similar` is documented as "brute-force cosine over stored vectors on SQLite";
a PostgreSQL implementation would delegate the same method to `pgvector`'s `<=>` operator instead
— callers (`fm-app`) would not change at all. See `fm_db::open` for the extension point.
