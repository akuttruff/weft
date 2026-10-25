//! Checkpoints: before a tool changes a file, a copy of it is kept, so `/undo`
//! can put a whole task's edits back. New files are noted so undo removes
//! them. Only weft's own file tools are covered; what a shell command does to
//! the tree is not.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Debug)]
pub struct Checkpoint {
    dir: PathBuf,
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    /// Files copied into `dir/files/` before their first change.
    saved: BTreeSet<String>,
    /// Files that did not exist before the task.
    created: BTreeSet<String>,
}

impl Checkpoint {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            state: Mutex::new(State::default()),
        }
    }

    /// Keep `rel` (under `root`) as it is now, the first time it is about to change.
    pub fn before_write(&self, root: &Path, rel: &str) -> std::io::Result<()> {
        let mut state = self.state.lock().unwrap();
        if state.saved.contains(rel) || state.created.contains(rel) {
            return Ok(());
        }
        let source = root.join(rel);
        if source.is_file() {
            let copy = self.dir.join("files").join(rel);
            if let Some(parent) = copy.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&source, &copy)?;
            state.saved.insert(rel.to_string());
        } else {
            state.created.insert(rel.to_string());
        }
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        let state = self.state.lock().unwrap();
        state.saved.is_empty() && state.created.is_empty()
    }

    /// Every file this task changed or created.
    pub fn files(&self) -> Vec<String> {
        let state = self.state.lock().unwrap();
        state.saved.iter().chain(&state.created).cloned().collect()
    }

    /// Put every file back as it was and drop the checkpoint. Returns what was touched.
    pub fn restore(&self, root: &Path) -> std::io::Result<Vec<String>> {
        let state = self.state.lock().unwrap();
        for rel in &state.saved {
            fs::copy(self.dir.join("files").join(rel), root.join(rel))?;
        }
        for rel in &state.created {
            match fs::remove_file(root.join(rel)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        let files: Vec<String> = state.saved.iter().chain(&state.created).cloned().collect();
        drop(state);
        self.discard();
        Ok(files)
    }

    /// Forget the kept copies.
    pub fn discard(&self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restores_changed_files_and_removes_created_ones() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/a.rs"), "one").unwrap();
        let cp = Checkpoint::new(dir.path().join("cp"));
        assert!(cp.is_empty());

        cp.before_write(&root, "src/a.rs").unwrap();
        fs::write(root.join("src/a.rs"), "two").unwrap();
        cp.before_write(&root, "src/a.rs").unwrap(); // a second change keeps the first copy
        fs::write(root.join("src/a.rs"), "three").unwrap();
        cp.before_write(&root, "src/new.rs").unwrap();
        fs::write(root.join("src/new.rs"), "fresh").unwrap();
        assert_eq!(cp.files(), ["src/a.rs", "src/new.rs"]);

        let restored = cp.restore(&root).unwrap();
        assert_eq!(restored, ["src/a.rs", "src/new.rs"]);
        assert_eq!(fs::read_to_string(root.join("src/a.rs")).unwrap(), "one");
        assert!(!root.join("src/new.rs").exists());
        assert!(!dir.path().join("cp").exists(), "the copies are gone");
    }
}
