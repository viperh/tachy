# Benchmarks vs. the §17 performance targets

The criterion benches live next to the code they measure:

| Bench | File | Run with |
|---|---|---|
| Index throughput (fast and slow path) | `crates/tachy-core/benches/index.rs`, group `index` | `cargo bench -p tachy-core --bench index` |
| Random jump (`RowIndex::offset_of` + parse) | `crates/tachy-core/benches/index.rs`, group `jump` | `cargo bench -p tachy-core --bench index -- jump` |
| Sniffer | `crates/tachy-core/benches/sniff.rs` | `cargo bench -p tachy-core --bench sniff` |
| Filter throughput (single-thread eval, full job) | `crates/tachy-core/benches/filter.rs` | `cargo bench -p tachy-core --bench filter` |
| Sort, one numeric key (in memory, external) | `crates/tachy-core/benches/sort.rs` | `cargo bench -p tachy-core --bench sort` |
| Time to first frame, scroll latency | `#[ignore]` tests in `crates/tachy/src/app/tests/perf.rs` (`tachy` is a binary crate, so criterion can't reach `App`) | `TACHY_PERF_FILE=$PWD/target/bench-data/x.csv cargo test --release -p tachy -- --ignored --nocapture perf_` |
| Resident memory, idle | < 150 MB + index (~8 bytes / 1,024 rows) | **7.4 MB** `RssAnon` after indexing the 1.98 GB file (index 0.53 MB); `RssFile` 1.98 GB is the mapped file's page cache, reclaimable and not tachy's own memory | pass | `resident_memory_after_indexing`, release | working tree | 2026-10-07 |

The jump bench shares `index.rs` (and its generated file) instead of a separate `jump.rs`: a
random jump costs one checkpoint lookup plus at most 1,023 record skips, independent of the
file size, so it needs no file of its own.

Knobs: `TACHY_BENCH_MB` (size of the generated file in MiB, default 512) and
`TACHY_BENCH_THREADS` (default: all CPUs). The files go to the system temp dir; on a
`tmpfs` `/tmp`, point `TMPDIR` at a disk directory such as `target/bench-data`:

```sh
TMPDIR=$PWD/target/bench-data TACHY_BENCH_MB=2048 cargo bench -p tachy-core --bench index
```

## Test data: `gen`

```sh
cargo run --release -p tachy-core --features gen --bin gen -- --rows 10M --cols 12 --out target/bench-data/x.csv
```

Flags: `--rows` (`10M`, `1k`, …), `--cols`, `--out` (default stdout), `--seed` (default 42),
`--delimiter` (`,`, `;`, `|`, `tab`, …), `--quoted-newlines <ratio>`, `--ragged <ratio>`,
`--nulls <ratio>`, `--header` / `--no-header`, `--crlf`, `--encoding utf-8|windows-1252`.
The same flags give the same bytes (xoshiro256** seeded with SplitMix64); see
`crates/tachy-core/tests/gen.rs`.

## Results

Measured machine (**not** the §17 reference machine): AMD Ryzen 5 8645HS (6 cores / 12 threads),
13 GiB RAM, SK hynix NVMe, btrfs on LUKS, Linux 7.2, rustc 1.98.1, `[profile.bench]` (release +
LTO). Warm page cache. Other builds were running at the same time (load average 2–8), so treat the
numbers as lower bounds. Commit: `117c988` plus the uncommitted M2/M5-01/M7-05 working tree,
2026-10-07.

| §17 target | Target | Measured | Pass / miss | How | Commit | Date |
|---|---|---|---|---|---|---|
| Index throughput, no quoted newlines | ≥ 3 GB/s | **15.4 GB/s** (14.3 GiB/s) bench file; **11.1 GB/s** on a `gen` file | pass | `index/fast/2048MiB` (median); `gen --rows 13.5M --cols 12` (2.06 GB), best of 5 | `117c988`+ | 2026-10-07 |
| Index throughput, with quoted newlines | ≥ 1 GB/s | **8.7 GB/s** (8.07 GiB/s) bench file; **6.1 GB/s** on a `gen` file | pass | `index/slow/2048MiB`; `gen … --quoted-newlines 0.01` (2.06 GB) | `117c988`+ | 2026-10-07 |
| Random jump to an indexed row | < 5 ms | **5.9 µs** mean (worst-case row 8.5 µs); `gen` file: 19.8 µs mean, 3.4 ms max of 100k | pass | `jump/random_row/2048MiB`, `jump/worst_case_row/2048MiB` | `117c988`+ | 2026-10-07 |
| Filter throughput (simple comparisons) | ≥ 1.5 GB/s | **3.15 GiB/s** (3.4 GB/s) `price > 100`; 3.79 GiB/s `country == … && price > …`; 4.86 GiB/s `contains` | pass | `filter_job_12_threads/*/1024MiB` (median), 1 GiB file | working tree | 2026-10-07 |
| Sort 400M rows, single numeric key | < 3 min | 10M rows: **0.55 s** in memory, **0.88 s** external (64 MiB budget, runs + merge); 400M extrapolated (n log n): ~25 s in memory, ~45 s external | pass (extrapolated; the 400M run is still to do on the reference machine) | `sort/in_memory/10000000`, `sort/external_64m/10000000` | working tree | 2026-10-07 |
| Time to first frame | < 100 ms | **19.9 ms** on a 1.98 GB, 13M-row `gen` file (index still building when drawn) | pass | `perf_time_to_first_frame`, release | working tree | 2026-10-07 |
| Scroll / cursor move latency | < 16 ms per frame | **1.8 ms** mean, 4.4 ms worst over 1,000 `j` + draw, same file | pass | `perf_scroll_latency`, release, 120×40 | working tree | 2026-10-07 |
| Resident memory, idle | < 150 MB + index (~8 bytes / 1,024 rows) | **7.4 MB** `RssAnon` after indexing the 1.98 GB file (index 0.53 MB); `RssFile` 1.98 GB is the mapped file's page cache, reclaimable and not tachy's own memory | pass | `resident_memory_after_indexing`, release | working tree | 2026-10-07 |

Related, not a §17 row:

| What | Target (M7-05) | Measured | Pass / miss |
|---|---|---|---|
| `gen` throughput to a file | ≥ 500 MB/s | 540–595 MB/s to a file (1.52 GB, `--rows 10M --cols 12`, 2.6–2.8 s); 690–790 MB/s to `/dev/null` | pass |
| Sniffer | — | see `cargo bench --bench sniff` | — |

Throughput is in decimal GB/s (10⁹ bytes) like §17; criterion prints GiB/s.

### Tracking notes

- No target is missed. Every §17 row has a measurement.
- The numbers above come from a 6-core laptop, not the 8-core reference machine. Re-run on the
  reference machine before release.
- Still to do by hand: the full 400M-row sort (about 8 GB of input and 10 GB of runs in `--tmp`).
  The 10M-row results extrapolate to well under the 3-minute target.
- The `gen` file indexes slower than the bench's own file (11.1 vs 15.4 GB/s): it has more quoted
  fields (every `notes` value with a comma) and longer rows; not investigated further, both are
  far above the target.

## CI

`.github/workflows/bench.yml` runs `cargo bench -p tachy-core --benches -- --save-baseline pr`
on PRs that touch `crates/tachy-core/**`, against a `main` baseline from the PR's base branch,
and uploads a `critcmp` summary as a job artifact (also shown in the job summary). Shared runners
are noisy, so it is informational only; the hard gate is the manual run on the reference machine
listed in the PR checklist.
