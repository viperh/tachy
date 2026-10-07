# tachy

[![CI](https://github.com/viperh/tachy/workflows/CI/badge.svg)](https://github.com/viperh/tachy/actions)

A starting point for terminal user interfaces in Rust, built on
[ratatui](https://ratatui.rs) with an async [tokio](https://tokio.rs) event
loop, layered configuration, file logging and cross-platform release builds.

## Layout

The workspace deliberately splits the UI from everything else:

```
Cargo.toml              workspace manifest — all dependency versions live here
.config/config.json     default keybindings and styles, baked into the binary
.envrc                  direnv: keep config/data/logs inside the repo
crates/
  tachy/                the binary: terminal, rendering, input, config, logging
    build.rs            vergen — stamps git/build info into the version string
    src/
      main.rs           entry point
      app.rs            event loop, mode handling, component dispatch
      action.rs         the Action enum every component speaks
      components.rs     the Component trait
      components/
        home.rs         default screen — copy this shape for new components
      cli.rs            clap argument parsing
      config.rs         layered config, keybinding and style parsing
      errors.rs         panic hooks, color-eyre, human-panic
      logging.rs        tracing subscriber writing to a log file
      tui.rs            terminal setup/teardown and the crossterm event stream
  tachy-core/           domain logic, no terminal dependencies
    src/lib.rs
```

`tachy-core` must never depend on `ratatui`, `crossterm` or `clap`. Keeping the
domain there means it can be unit tested without a TTY, and it stays reusable
if you later add a second front end (a CLI, a daemon, a web UI).

## Running

`q`, `Ctrl-c` and `Ctrl-d` quit; `Ctrl-z` suspends. Rebind in
`.config/config.json`.

```sh
cargo run -p tachy -- data.csv     # open a file (use - for stdin)
cargo run -p tachy -- --help       # every option (spec §3)
cargo run -p tachy -- --version    # prints git info and the resolved directories
```

## Limitations

tachy memory-maps files read-only (spec §5.1, §16). If another program
truncates an open file, reading past its new end raises `SIGBUS`. tachy
catches it, restores the terminal, prints
`tachy: <file> was truncated while open (SIGBUS); exiting` and exits with code 1.
This is inherent to mmap. Other changes (appends, rewrites) are detected within
about 2 s and shown as `file changed on disk — R to reload`.

## Configuration

Defaults are compiled in from `.config/config.json`. At startup the app also
looks in the per-user config directory (printed by `--version`) for
`config.json5`, `config.json`, `config.yaml`, `config.toml` or `config.ini`,
and layers whatever it finds on top. Set `TACHY_CONFIG` to override that
directory outright.

Keybindings are keyed by mode, then by key sequence: `"<Ctrl-a>"` for a single
chord, `"<g><g>"` for a sequence. Every value must name an `Action` variant.

## Logging

Logs go to `<data dir>/tachy.log`. Set `TACHY_LOG_LEVEL` (or `RUST_LOG`) to change
the filter, and `TACHY_DATA` to change the directory.

## Checks

The same four gates CI runs:

```sh
cargo test --locked --all-features --workspace
cargo fmt --all --check
cargo clippy --all-targets --all-features --workspace -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --document-private-items --all-features --workspace
```

`Cargo.lock` is committed on purpose — CI builds with `--locked`.

## Releases

Pushing a tag matching `v1.2.3` or `1.2.3` builds the binary for macOS
(x86_64/arm64), Linux (x86_64/arm64/i686) and Windows, then attaches tarballs
and SHA-256 sums to the GitHub release.

## License

MIT — see [LICENSE](LICENSE).
