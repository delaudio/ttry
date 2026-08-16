use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

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
        let slug = sanitize_name(name);
        let slug = if slug.is_empty() { "snapshot" } else { &slug };
        self.root.join(format!(
            "{slug}--{:016x}.snap",
            stable_hash(name.as_bytes())
        ))
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
    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("snapshot");
    let mut temporary = None;
    for _ in 0..100 {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(".{filename}.tmp-{}-{id}", std::process::id()));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => {
                temporary = Some((candidate, file));
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    let Some((temporary_path, mut file)) = temporary else {
        return Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!(
                "could not allocate a temporary snapshot beside {}",
                path.display()
            ),
        )));
    };
    let result = (|| -> std::io::Result<()> {
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary_path, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    result.map_err(Error::Io)
}

fn sanitize_name(name: &str) -> String {
    const MAX_SLUG_LEN: usize = 96;
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
    value
        .trim_matches('-')
        .chars()
        .take(MAX_SLUG_LEN)
        .collect::<String>()
        .trim_end_matches('-')
        .to_string()
}

fn stable_hash(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
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
        assert!(matches!(
            update.assert_screen("case", &terminal.screen()).unwrap(),
            SnapshotResult::Updated(_)
        ));
        let entries: Vec<_> = fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].to_string_lossy().ends_with(".snap"));
    }

    #[test]
    fn snapshot_paths_are_nonempty_and_collision_safe() {
        let store = SnapshotStore::new("snapshots", SnapshotOptions::default());
        assert_ne!(store.path_for("a/b"), store.path_for("a:b"));
        assert_ne!(store.path_for("!!!"), PathBuf::from("snapshots/.snap"));
        assert!(store
            .path_for("!!!")
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("snapshot--"));

        let long_name = "very-long-name-".repeat(1_000);
        let filename = store.path_for(&long_name).file_name().unwrap().to_owned();
        assert!(filename.as_encoded_bytes().len() < 128);
        assert!(filename.to_string_lossy().ends_with(".snap"));
    }
}
