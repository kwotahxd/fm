# Benchmark: directory listing vs `ls -la --color=never`

## What is measured

`cargo run --release -p fm-core --example bench_ls -- <dir> [stat_threads]` opens `<dir>`,
reads it with `getdents64` + one `fstatat` per entry (see `crates/fm-core/src/listing.rs`),
sorts the entries by name, and formats an `ls -l`-style line per entry into an in-memory buffer.
That mirrors the *default* work `ls -la` does (list, stat, sort, format) minus the two things
that are not about directory reading: writing to a terminal, and colourising (`--color=never`
disables the latter; both sides skip the former by redirecting to `/dev/null`).

## Reproducing

```bash
# 1. generate a flat directory with N files (random sizes 0-200 bytes, mixed extensions)
python3 bench/gen.py /tmp/bench_dir 100000

# 2. build the release benchmark binary
cargo build --release -p fm-core --example bench_ls

# 3. compare (hyperfine gives proper warm-up + statistics; a plain loop works too, see below)
hyperfine --warmup 2 \
  'ls -la --color=never /tmp/bench_dir' \
  './target/release/examples/bench_ls /tmp/bench_dir 1'

# 4. multi-threaded stat (only helps once entries exceed ReadOpts::par_threshold, default 20_000)
./target/release/examples/bench_ls /tmp/bench_dir "$(nproc)"
```

Without `hyperfine`, a quick comparison:

```bash
python3 - <<'PY'
import subprocess, time
t = []
for _ in range(5):
    t0 = time.perf_counter()
    subprocess.run(["ls", "-la", "--color=never", "/tmp/bench_dir"], stdout=subprocess.DEVNULL)
    t.append(time.perf_counter() - t0)
print("avg ls -la:", sum(t) / len(t) * 1000, "ms")
PY
```

## Result (this environment: 100,000 files, single-CPU sandbox container, overlayfs)

| tool                         | avg wall time |
|-------------------------------|---------------|
| `ls -la --color=never`        | ~410 ms       |
| `fm-core` bench_ls (1 thread) | ~170 ms       |

fm-core comes in at roughly **40% of `ls`'s time** here, comfortably inside the ≤10-15%
*difference* target from the spec (it is faster, not just close). The gap is mostly GNU
`ls`'s per-entry `getpwuid`/`getgrgid` username/group lookups (NSS calls) and locale-aware
(`strcoll`) sorting, neither of which `fm-core` does (it sorts by raw bytes and prints numeric
uid/gid, same as `ls -n`).

This sandbox exposes a single CPU, so the parallel-`fstatat` path (`ReadOpts::stat_threads`,
used automatically by `fm index` through `fm_config::Config::walk_threads()`) could not be
benchmarked here — on a real multi-core machine, pass `nproc` as the second argument to see it
kick in above `par_threshold` (default 20,000 entries per directory). Re-run the steps above on
target hardware to confirm the multi-core numbers; the single-threaded result above is
already within spec on its own.
