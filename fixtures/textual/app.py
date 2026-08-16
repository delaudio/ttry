from textual.app import App, ComposeResult
from textual.widgets import Static


class FixtureApp(App[None]):
    def compose(self) -> ComposeResult:
        yield Static("Textual loading", id="status")

    def on_mount(self) -> None:
        self.set_timer(0.05, lambda: self.query_one("#status", Static).update("Textual async ready"))

    def on_key(self, event) -> None:
        if event.key == "q":
            self.exit()
        else:
            self.query_one("#status", Static).update(f"Textual key: {event.key}")


if __name__ == "__main__":
    FixtureApp().run()
