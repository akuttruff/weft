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
    /// A commit git made of the tracked tree before the task (`git stash
    /// create`), or `HEAD` when the tree was clean. Lets undo cover what a
    /// shell command changed, not only what the tools did.
    git_base: Option<String>,
    /// Untracked files before the task, to tell new ones apart afterwards.
    untracked_before: BTreeSet<String>,
    /// Tracked files git saw change during the task that the tools did not touch.
    git_changed: BTreeSet<String>,
    /// Untracked files that appeared during the task, not made by the tools.
    git_new: BTreeSet<String>,
}

/// `git <args>` in `root`, its stdout, or `None` when git says no.
fn git(root: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn lines(s: &str) -> BTreeSet<String> {
    s.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
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

    /// Before the task, in a git repository: remember the tracked tree and
    /// the untracked files, so `note_git_after` can see what a shell command
    /// changed. Elsewhere this does nothing.
    pub fn note_git_before(&self, root: &Path) {
        if git(root, &["rev-parse", "--is-inside-work-tree"]).as_deref() != Some("true") {
            return;
        }
        // `stash create` writes a commit of the working tree and leaves the
        // tree alone; it prints nothing when there is nothing to save.
        let base = match git(root, &["stash", "create"]) {
            Some(hash) if !hash.is_empty() => hash,
            _ => "HEAD".to_string(),
        };
        let untracked = git(root, &["ls-files", "--others", "--exclude-standard"])
            .map(|s| lines(&s))
            .unwrap_or_default();
        let mut state = self.state.lock().unwrap();
        state.git_base = Some(base);
        state.untracked_before = untracked;
    }

    /// After the task: the tracked files that differ from before, and the new
    /// untracked files, beyond what the tools already noted.
    pub fn note_git_after(&self, root: &Path) {
        let mut state = self.state.lock().unwrap();
        let Some(base) = state.git_base.clone() else {
            return;
        };
        let changed = git(root, &["diff", "--name-only", "--relative", &base])
            .map(|s| lines(&s))
            .unwrap_or_default();
        let untracked = git(root, &["ls-files", "--others", "--exclude-standard"])
            .map(|s| lines(&s))
            .unwrap_or_default();
        for f in changed {
            if !state.saved.contains(&f) && !state.created.contains(&f) {
                state.git_changed.insert(f);
            }
        }
        for f in untracked {
            if !state.untracked_before.contains(&f) && !state.created.contains(&f) {
                state.git_new.insert(f);
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        let state = self.state.lock().unwrap();
        state.saved.is_empty()
            && state.created.is_empty()
            && state.git_changed.is_empty()
            && state.git_new.is_empty()
    }

    /// Every file this task changed or created.
    pub fn files(&self) -> Vec<String> {
        let state = self.state.lock().unwrap();
        state
            .saved
            .iter()
            .chain(&state.created)
            .chain(&state.git_changed)
            .chain(&state.git_new)
            .cloned()
            .collect()
    }

    /// Put every file back as it was and drop the checkpoint. Returns what was touched.
    pub fn restore(&self, root: &Path) -> std::io::Result<Vec<String>> {
        let state = self.state.lock().unwrap();
        for rel in &state.saved {
            fs::copy(self.dir.join("files").join(rel), root.join(rel))?;
        }
        for rel in state.created.iter().chain(&state.git_new) {
            match fs::remove_file(root.join(rel)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        // What a shell command changed comes back from git's copy of the
        // tree; a file git did not have then is one the task added.
        if let Some(base) = &state.git_base {
            for rel in &state.git_changed {
                if git(root, &["checkout", base, "--", rel]).is_none() {
                    let _ = git(root, &["rm", "--cached", "--quiet", "--", rel]);
                    let _ = fs::remove_file(root.join(rel));
                }
            }
        }
        let files: Vec<String> = state
            .saved
            .iter()
            .chain(&state.created)
            .chain(&state.git_changed)
            .chain(&state.git_new)
            .cloned()
            .collect();
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

    #[test]
    fn in_a_repository_shell_changes_are_undone_too() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        fs::create_dir_all(&root).unwrap();
        let sh = |args: &[&str]| {
            assert!(
                std::process::Command::new("git")
                    .args(args)
                    .current_dir(&root)
                    .output()
                    .unwrap()
                    .status
                    .success(),
                "git {args:?}"
            );
        };
        sh(&["init", "-q"]);
        sh(&["config", "user.email", "t@example.com"]);
        sh(&["config", "user.name", "t"]);
        fs::write(root.join("a.txt"), "committed").unwrap();
        fs::write(root.join("b.txt"), "committed too").unwrap();
        sh(&["add", "."]);
        sh(&["commit", "-qm", "chore: start"]);
        // The user's own uncommitted edit, made before the task, must survive an undo.
        fs::write(root.join("b.txt"), "my own edit").unwrap();

        let cp = Checkpoint::new(dir.path().join("cp"));
        cp.note_git_before(&root);
        // The task: a tool edit of a.txt, and a shell command that rewrites
        // b.txt and adds c.txt behind the tools' back.
        cp.before_write(&root, "a.txt").unwrap();
        fs::write(root.join("a.txt"), "tool edit").unwrap();
        fs::write(root.join("b.txt"), "shell edit").unwrap();
        fs::write(root.join("c.txt"), "shell made").unwrap();
        cp.note_git_after(&root);
        assert_eq!(cp.files(), ["a.txt", "b.txt", "c.txt"]);

        cp.restore(&root).unwrap();
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "committed");
        assert_eq!(
            fs::read_to_string(root.join("b.txt")).unwrap(),
            "my own edit"
        );
        assert!(!root.join("c.txt").exists());
    }
}
