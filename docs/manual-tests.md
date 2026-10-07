# Edge-case audit (M7-04)

Every row of spec §16, plus the edge cases it implies, with the test that
covers it. Rows that need a real terminal, a full disk or a tool that isn't
always installed have a manual procedure. Each procedure was last run on
2026-10-07, with the result shown.

## §16 table

| # | Situation | Expected | Covered by |
|---|---|---|---|
| 1 | File not found / no permission | Coral toast; no tab; other files still open; all failing → empty TUI (D4) | `crates/tachy/src/app/tests.rs` (missing file next to a valid one; all files missing then `Ctrl-o`), `tests/pty.rs::no_file_then_ctrl_o`, `source.rs` error-kind tests |
| 2 | Empty file | `empty file` placeholder; no indexer or dialog crash | `crates/tachy-core/tests/edge_cases.rs::empty_file_opens_with_zero_rows`, table snapshot `empty_file` |
| 3 | Header only | 0 rows; columns shown | `edge_cases.rs::header_only_has_columns_and_no_rows`, table snapshot `header_only` |
| 4 | Ragged rows | Padded / `_extraN`; coral gutter; count in the status line | `edge_cases.rs::ragged_rows_are_counted`, `cache.rs` ragged tests, `tests/indexer.rs` (exact counts on both paths), table snapshot `ragged` |
| 5 | Unterminated quote at EOF | Last record ends at EOF; warning | `edge_cases.rs::unterminated_quote_ends_at_eof_with_a_warning`, `parse.rs` tests |
| 6 | Invalid UTF-8 | `�` on display; raw bytes kept for export | `edge_cases.rs::invalid_utf8_is_kept_as_raw_bytes`, `tests/export.rs` (bytes unchanged in output) |
| 7 | File modified while open | Sticky `file changed on disk — R to reload` within 2 s | `watch.rs` tests (paused time), app test for the sticky warning and `R` |
| 8 | File truncated while mmapped | SIGBUS → clean exit, message, exit code 1 | `sigbus.rs` fork test (real truncated mapping), **manual: live terminal** below |
| 9 | Disk full during sort / export | Job `✗`; temp files removed; view unaffected | `tests/export.rs` (failing writer, `StorageFull`), `tests/sort.rs` (unusable temp dir → `JobError::Io`, cancel leaves no files), **manual: tmpfs** below |
| 10 | Terminal resize | Re-layout next frame; cursor visible | `app/tests.rs` resize tests, `layout.rs` tests |
| 11 | Panic | Terminal restored before the panic message | `tui.rs::restore_terminal` idempotence test, **manual** below (`TACHY_DEBUG_PANIC`) |
| 12 | Terminal < 80×24 | `terminal too small` only | `layout.rs` tests, snapshot `too_small_79x24` |

## Implied edge cases

| Case | Covered by |
|---|---|
| No trailing newline | `edge_cases.rs::no_trailing_newline_keeps_the_last_row` |
| One field per row (no delimiter) | `edge_cases.rs::single_column_without_delimiters` |
| 1-byte file, only `\n`, only `\r\n\r\n` | `edge_cases.rs::tiny_files` |
| UTF-8 BOM + header | `edge_cases.rs::utf8_bom_is_not_part_of_the_first_column_name` |
| Very long line (8 MiB here; the 1 MiB sniff cap and the 1 MiB popup cap are tested in their modules) | `edge_cases.rs::very_long_line_is_parsed_whole` |
| More than 10,000 columns | `edge_cases.rs::many_columns` (12,000) |
| Mixed CRLF / LF | `edge_cases.rs::crlf_and_lf_mixed` |
| Filter / sort / profile while indexing | `tests/filter.rs` (follows a growing index), `tests/sort.rs` and `tests/profile.rs` (wait for the parent / index) |
| Closing a tab with running jobs | `jobs.rs` tests (tab cancel; RAII temp dirs), confirm dialog test |
| Quitting with jobs running | confirm dialog app test; **manual** run below left no `tachy-*` files |
| Source never opened for writing (§1) | `tests/source_readonly.rs` (no write-mode open in `source.rs`), **manual** `/proc` check below |

## Manual procedures

### SIGBUS in a live terminal (row 8)

1. Generate a file: `cargo run --release -p tachy-core --features gen --bin gen -- --rows 600k --out /tmp/t.csv`.
2. Run `tachy -y /tmp/t.csv`, wait for the table.
3. From another shell run `truncate -s 0 /tmp/t.csv`, then press `G` in tachy.

Expected: exit code 1, `tachy: /tmp/t.csv was truncated while open (SIGBUS); exiting`
on stderr, the alternate screen left and the cursor shown again.

**Result (2026-10-07):** pass. It was run under a pseudo-terminal by a Python driver that
checks the exit code, the message and the `\e[?1049l` / `\e[?25h` restore sequences.

### Source opened read-only (§1)

`strace` is not installed on the test machine. The equivalent check reads `/proc/<pid>/fd`,
`/proc/<pid>/fdinfo` and `/proc/<pid>/maps` while tachy opens, filters, sorts and exports a file.

**Result (2026-10-07):** pass. The file is only ever mapped `r--s` (no writable mapping), and no
descriptor stays open on it after mapping. With `strace` available:
`strace -f -e trace=openat tachy -y f.csv`. Every `openat` of `f.csv` must have `O_RDONLY`.

### Disk full during sort / export (row 9)

As root, or in `unshare -rm`: `mount -t tmpfs -o size=1M tmpfs /mnt/small`. Then run
`tachy --tmp /mnt/small big.csv`, press `s` on a column, and export to `/mnt/small/out.csv`.

Expected: both jobs end with `✗ … no space left on device`, `/mnt/small` is empty
afterwards, and the table is unchanged. The automated tests cover the same paths through an
injected failing writer.

### Panic restores the terminal (row 11)

Debug build: `TACHY_DEBUG_PANIC=main tachy -y f.csv`, and again with
`TACHY_DEBUG_PANIC=blocking`. Expected: the normal screen comes back, the panic text is
readable, and the exit code is 1.

### Terminal closed or `kill` during a session (not in §16)

Run tachy, start a sort, then either close the terminal (`SIGHUP`) or `kill <pid>`
(`SIGTERM`). Expected: tachy quits without asking, cancels its jobs and deletes every
`tachy-*` file in `--tmp` (sort runs, permutation files, the stdin spool).

**Result (2026-10-07):** pass, 3 out of 3 runs for each signal. Before the fix, a hangup left a
`tachy-perm-*.bin` behind. Drawing failed first, then ratatui's `Terminal::drop` panicked
writing to the closed stderr, so the tabs' destructors never ran.

### End-to-end jobs and saved views

A Python pseudo-terminal driver ran these steps on a 200k-row `gen` file:
1. `f`, then `country == "DE" && price > 1000`.
2. `Ctrl-S`, name `DE big`, glob `orders*.csv`.
3. `Enter` to apply the filter.
4. `l`, then `S` to sort by price, descending.
5. `e`, then `Enter` to export.
6. `q`, then `y` to quit.

**Result (2026-10-07):** pass.
- `views.json` holds the view.
- `orders.sorted.csv` has the 5,919 matching rows (checked against a Python reference), sorted
  by price descending, with ties in file order.
- The process exits 0, and no `tachy-*` temp files are left.
