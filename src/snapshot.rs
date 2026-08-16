use std::fs;
use std::path::{Path, PathBuf};

use similar::{ChangeTag, TextDiff};

use crate::{Error, Result, Screen};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SnapshotOptions {
    pub update: bool,
    pub preserve_width: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SnapshotResult {
    Matched,
    Created(PathBuf),
    Updated(PathBuf),
}

#[derive(Clone, Debug)]
pub struct SnapshotStore {
    root: PathBuf,
    options: SnapshotOptions,
}

impl SnapshotStore {
    pub fn new(root: impl Into<PathBuf>, options: SnapshotOptions) -> Self {
        Self {
            root: root.into(),
            options,
        }
    }
    pub fn path_for(&self, name: &str) -> PathBuf {
        self.root.join(format!("{}.snap", sanitize_name(name)))
    }
    pub fn assert_screen(&self, name: &str, screen: &Screen) -> Result<SnapshotResult> {
        let content = serialize_screen(screen, self.options.preserve_width);
        self.assert_content(name, &content)
    }
    pub fn assert_content(&self, name: &str, content: &str) -> Result<SnapshotResult> {
        let path = self.path_for(name);
        match fs::read_to_string(&path) {
            Ok(expected) if expected == content => Ok(SnapshotResult::Matched),
            Ok(_expected) if self.options.update => {
                write_snapshot(&path, content)?;
                Ok(SnapshotResult::Updated(path))
            }
            Ok(expected) => Err(Error::SnapshotMismatch {
                name: name.into(),
                diff: unified_diff(&expected, content),
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && self.options.update => {
                write_snapshot(&path, content)?;
                Ok(SnapshotResult::Created(path))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Err(Error::SnapshotMissing {
                    name: name.into(),
                    path,
                })
            }
            Err(error) => Err(error.into()),
        }
    }
}

pub fn serialize_screen(screen: &Screen, preserve_width: bool) -> String {
    let mut content = if preserve_width {
        screen.fixed_text()
    } else {
        screen.text()
    };
    content.push('\n');
    content
}

fn write_snapshot(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, content)?;
    Ok(())
}

fn sanitize_name(name: &str) -> String {
    let value: String = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '-'
            }
        })
        .collect();
    value.trim_matches('-').to_string()
}

fn unified_diff(expected: &str, received: &str) -> String {
    let diff = TextDiff::from_lines(expected, received);
    let mut output = String::from("--- expected\n+++ received\n");
    for change in diff.iter_all_changes() {
        let prefix = match change.tag() {
            ChangeTag::Delete => '-',
            ChangeTag::Insert => '+',
            ChangeTag::Equal => ' ',
        };
        output.push(prefix);
        output.push_str(change.value());
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Terminal;
    #[test]
    fn creation_match_and_mismatch() {
        let directory = tempfile::tempdir().unwrap();
        let mut terminal = Terminal::new(4, 1).unwrap();
        terminal.advance(b"ok");
        let update = SnapshotStore::new(
            directory.path(),
            SnapshotOptions {
                update: true,
                preserve_width: false,
            },
        );
        assert!(matches!(
            update.assert_screen("case", &terminal.screen()).unwrap(),
            SnapshotResult::Created(_)
        ));
        let verify = SnapshotStore::new(directory.path(), SnapshotOptions::default());
        assert_eq!(
            verify.assert_screen("case", &terminal.screen()).unwrap(),
            SnapshotResult::Matched
        );
        terminal.advance(b"x");
        assert!(matches!(
            verify.assert_screen("case", &terminal.screen()),
            Err(Error::SnapshotMismatch { .. })
        ));
    }
}
