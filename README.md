# ttry

Playwright-style end-to-end testing for terminal user interfaces.

`ttry` launches a real terminal application inside a Unix pseudo-terminal,
sends keyboard input, reconstructs the rendered terminal screen, and provides
assertions and snapshots over the observable result.

The project is written in Rust and initially targets macOS and Linux.

## Why ttry?

Terminal applications are usually tested with a mixture of unit tests, raw
ANSI snapshots, shell scripts, or ad-hoc PTY harnesses. These approaches often
miss the behavior users actually see: terminal initialization, keyboard input,
alternate-screen rendering, asynchronous updates, resizing, and process
lifecycle.

`ttry` is intended to make that workflow testable through one framework:

```text
launch → press/type → query → wait → assert → snapshot → close
```

## Project status

The repository is at the initial implementation stage. The current work is
focused on the MVP foundations:

- Unix PTY process execution;
- virtual terminal screen reconstruction;
- keyboard input encoding;
- text locators and retrying assertions;
- plain-text snapshots;
- a native Rust test runner and CLI.

The API and crate layout may change while these foundations are being built.

## Intended test experience

The public API is still being designed. The target experience is a native Rust
workflow with the same core ideas as Playwright:

```rust
#[ttry::test]
async fn opens_projects_panel(tui: ttry::Tui) -> ttry::Result<()> {
    tui.launch("./target/debug/my-app").await?;
    tui.keyboard().press("p").await?;

    tui.expect(tui.screen().get_by_text("Projects"))
        .to_be_visible()
        .await?;

    Ok(())
}
```

The exact syntax will be finalized as the runner and session API are
implemented.

## Supported applications

The core is framework-independent. It is intended to test applications built
with, among others:

- [Ratatui](https://ratatui.rs/);
- [Bubble Tea](https://github.com/charmbracelet/bubbletea);
- [Textual](https://textual.textualize.io/);
- Ink, Blessed, ncurses, and custom ANSI terminal applications.

The application under test may be written in any language. Only the test
framework itself is implemented in Rust.

## Development

Requirements:

- Rust stable toolchain;
- macOS or Linux;
- a terminal environment suitable for Unix PTY tests.

Once the initial crate is present, the expected development commands are:

```bash
cargo test
cargo fmt --check
cargo clippy --all-targets --all-features
```

## Design principles

- Test observable terminal behavior, not framework internals.
- Prefer event-driven waiting over arbitrary sleeps.
- Keep terminal dimensions and process environments deterministic.
- Treat process cleanup as part of every test.
- Keep the core independent from any specific TUI framework.
- Make failures useful by showing the current screen and recent actions.

## Scope

The first release targets macOS and Linux and includes keyboard input, text
queries, automatic waiting, process assertions, and plain-text snapshots.

Windows/ConPTY, mouse input, semantic locators, session recording, trace
replay, and parallel execution are planned for later phases.

## License

Apache License 2.0. See [LICENSE](LICENSE).
