//! End-to-end testing primitives for terminal user interfaces.
//!
//! The crate deliberately separates terminal parsing, process management and
//! assertions, so each layer can be used and tested on its own.
//!
//! ```no_run
//! use std::time::Duration;
//! use ttry::{LaunchOptions, TuiSession};
//!
//! # fn main() -> ttry::Result<()> {
//! let session = TuiSession::launch(LaunchOptions::new("./my-tui").size(80, 24))?;
//! session.wait_for_text("Ready", Duration::from_secs(5))?;
//! session.keyboard().press("enter")?;
//! session.expect(session.get_by_text("Done")).to_be_visible()?;
//! session.close()?;
//! # Ok(())
//! # }
//! ```

mod error;
pub mod expect;
pub mod keyboard;
pub mod locator;
pub mod process;
pub mod runner;
pub mod screen;
pub mod session;
pub mod snapshot;

pub use error::{Error, Result};
pub use expect::{expect, Expect, ExpectOptions, ProcessExpect, ScreenExpect};
pub use keyboard::{Key, Keyboard};
pub use locator::{BoundingBox, Locator, TextMatcher};
pub use process::{ExitStatus, ProcessState, PtyOptions};
pub use screen::{Cell, Color, Rect, Screen, Style, Terminal, MAX_SCREEN_CELLS};
pub use session::{launch, LaunchOptions, TuiSession};
pub use snapshot::{SnapshotOptions, SnapshotResult, SnapshotStore};
