# ttry

Playwright-style end-to-end testing for terminal user interfaces.

`ttry` launches a real terminal application inside a Unix pseudo-terminal,
sends keyboard input, reconstructs the rendered screen, and provides live text
locators, retrying assertions, process assertions, and plain-text snapshots.
The test framework is Rust; the application under test can use any language or
TUI framework.

```text
launch → press/type/paste → query → wait → assert → snapshot → close
```

## Requirements and supported platforms

- Rust 1.85 or newer;
- macOS or Linux;
- a Unix PTY environment.

Windows/ConPTY is not supported in the first release. Node.js and TypeScript
are not required.

## First test from a clean checkout

Install Rust with [rustup](https://rustup.rs/), clone the repository, and run:

```bash
cargo test
```

This builds the library and deterministic PTY fixture, then runs unit and
integration tests. The integration tests launch a real child in a PTY and
cover terminal output, keyboard input, asynchronous redraws, resize, exit
status, and bounded cleanup.

The standard development gate is:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
```

CI runs this gate on both macOS and Linux.

## Library API

Commands and arguments are always passed separately; ttry does not insert a
shell.

```rust,no_run
use std::time::Duration;
use ttry::{LaunchOptions, TuiSession};

fn main() -> ttry::Result<()> {
    let session = TuiSession::launch(
        LaunchOptions::new("./target/debug/my-app")
            .arg("--demo")
            .size(80, 24),
    )?;

    session.wait_for_text("Projects", Duration::from_secs(5))?;
    session.keyboard().press("p")?;
    session.keyboard().type_text("hello", None)?;
    session.expect(session.get_by_text("Projects"))
        .to_be_visible()?;
    session.expect_screen().to_contain_text("Projects")?;
    session.close()?;
    Ok(())
}
```

`Screen` supports text/line/cell extraction and clipped regions. Locators are
live: a locator created before a redraw queries the current screen whenever
`count`, `text`, `is_visible`, or `bounding_box` is called. Single-match
operations report every matching coordinate when strictness fails.

Assertions default to five seconds, wake on screen changes, and use a small
fallback interval so idle waits do not busy-loop. A session-bound assertion
also stops promptly if its process exits.

## Snapshots

`SnapshotStore` serializes either a full screen or a region. By default it
trims trailing spaces on each line; `preserve_width` keeps the requested cell
width. Files use UTF-8, `\n` line endings, and exactly one final newline.
Snapshot filenames combine a readable slug with a stable hash, so punctuation
normalization cannot make two test names overwrite the same file.

Missing or mismatching snapshots fail in normal mode. Set
`SnapshotOptions.update` explicitly to create or update them. Mismatches show
an expected/received line diff.

## Native CLI and configuration

Copy the example configuration and build the fixture:

```bash
cp ttry.example.toml ttry.toml
cargo build --bin ttry-fixture
cargo run -- test
```

The configuration contains deterministic `[[tests]]` entries. Tests run
serially and support grouping, skip/focus, initial `cols`/`rows`, input,
expected text, expected exit, expected exit code, and per-test timeouts.
Configured commands must exit successfully by default, including after a
screen assertion. Set `allow_running = true` for an interactive TUI that
should pass while cleanup stops it; an immediately observable non-zero exit
still fails, using a best-effort nonblocking check. Set `expect_exit = true` to accept any exit status, or
`expect_exit_code` to require an exact status. CLI options override
configuration values:

```bash
ttry test \
  --config path/to/ttry.toml \
  --grep "projects" \
  --timeout 10000 \
  --reporter dot \
  --update-snapshots
```

Invalid TOML, unknown fields, zero timeouts, empty names/commands, and unknown
reporters produce readable errors. Any failed test makes the CLI exit nonzero;
the final report always contains deterministic pass/fail/skip counts.

Rust-authored suites can use `Runner`, `TestCase`, and `TestContext::tui` to
register tests and groups directly. The context owns every session and runs
cleanup even when the test body returns an error. On timeout, registered
sessions are closed by the runner to unblock pending PTY operations.
Custom long-running work must poll `TestContext::is_cancelled()` to stop
cooperatively. Rust closures execute in isolated worker threads and cannot be
terminated forcibly; after bounded cancellation and cleanup, an uncooperative
worker is detached so it cannot block the remaining cases. Configured CLI tests
use bounded PTY startup, assertion, and process-shutdown operations.
`Runner::update_snapshots` propagates the CLI update flag without mutating the
process environment; test bodies can derive a `SnapshotOptions` value through
`TestContext::snapshot_options`.

## Keyboard behavior

`press` accepts one printable character and these normalized expressions:

- Enter, Escape, Tab, Backspace, Delete;
- arrows, Home, End, Page Up, and Page Down;
- F1 through F12;
- portable combinations such as `ctrl+c`, `alt+x`, `shift+tab`, and modified
  navigation keys.

Ctrl letters use ASCII control bytes. Alt prefixes the base encoding with ESC.
Shift+Tab uses CSI Z. Other modified navigation keys use xterm CSI modifier
parameters. Combinations without a portable terminal encoding fail with an
actionable error. `type_text` preserves character order and can add a delay;
`paste` performs exactly one write and does not add bracketed-paste markers.

## Framework fixtures

Isolated fixtures live under `fixtures/` for Ratatui, Bubble Tea, and Textual.
Their smoke tests are marked ignored in the default Rust run, so the core does
not depend on Cargo, Go, or Python framework setup beyond its own dependencies.
CI installs each toolchain and runs:

```bash
cargo test --test frameworks -- --ignored --test-threads=1
```

Each fixture covers either keyboard input or an asynchronous update. If a
toolchain is unavailable locally, the ignored test and its required environment
variable make the skip explicit rather than failing the core test suite.

## Process lifecycle

Session close is idempotent and bounded. It first requests graceful EOF, then
sends SIGTERM, then SIGKILL to the isolated Unix process group if any process
still runs. This also cleans up descendants spawned by shells and CLIs. Process
state distinguishes running, normal exit codes, and signal exits.
Recent lifecycle events are included in timeout diagnostics. Dropping the last
session handle also performs bounded termination and reaps the child.
`startup_timeout` rejects PTY creation that completes after its budget; spawn
remains synchronous so a timeout cannot orphan a launcher thread.
`shutdown_timeout` bounds the cleanup sequence. Both must be greater than zero.

## Security

Every command configured in Rust code or `ttry.toml` is user-provided code and
runs with the same account, filesystem access, environment inheritance, and
network permissions as the `ttry` process. Treat test configurations and
fixture repositories like executable source code. Do not run untrusted tests,
and use CI sandboxing or containers when the application under test is not
trusted. ttry does not invoke a shell unless the configured executable is a
shell.

## Known VT limitations

The parser intentionally implements the common MVP subset: printable UTF-8,
basic controls, absolute/relative cursor movement, erase display/line, common
SGR colors and styles, save/restore cursor, alternate screen, wide characters,
and combining marks. Unsupported or malformed escape sequences are ignored
without panicking.

The first release does not fully emulate every xterm behavior. In particular:

- DEC private modes other than alternate screen are not modeled;
- scroll regions, insert/delete line and character commands, OSC/DCS payloads,
  hyperlinks, mouse tracking, synchronized updates, and sixel graphics are not
  represented;
- grapheme shaping is limited to terminal display width plus combining marks;
- resize preserves the overlapping rectangle rather than reproducing every
  terminal's historical reflow policy;
- style data is retained in cells, but the default snapshot format is plain
  text only.

These boundaries keep output deterministic for common TUIs. Add a focused
parser fixture before relying on an escape sequence outside this list.

## Design principles

- Test observable terminal behavior, not framework internals.
- Prefer event-driven waiting over arbitrary sleeps.
- Keep terminal dimensions and process environments deterministic.
- Treat process cleanup as part of every test.
- Keep the core independent from any specific TUI framework.
- Make failures useful by showing the screen, expected state, and process events.

## License

Apache License 2.0. See [LICENSE](LICENSE).
