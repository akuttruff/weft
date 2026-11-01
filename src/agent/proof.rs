//! What shows a task did what its plan says, beyond "the checks pass": the
//! tests it added, whether each fails with the code put back as it was (a
//! test that passes either way does not test the change), and the existing
//! test lines it changed (a check moved off its edge, a number loosened).

use std::path::{Path, PathBuf};

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::checkpoint::{Before, Checkpoint};
use crate::tools::bash::run_shell;

/// A test file by its name: `*.test.ts`, `*_test.go`, `tests/…`, `test_*.py`.
/// Rust's tests inside `src/` files are not told apart from the code.
pub fn is_test_file(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    let dirs = &path[..path.len() - name.len()];
    name.contains(".test.")
        || name.contains(".spec.")
        || name.contains("_test.")
        || (name.starts_with("test_") && name.ends_with(".py"))
        || dirs.split('/').any(|d| d == "tests" || d == "__tests__")
}

/// A test the task added: where it starts and what it is called.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewTest {
    pub file: String,
    pub line: usize,
    pub name: String,
}

/// How a new test did with the code as it was before the task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Without {
    /// It failed: it tests the change.
    Fails,
    /// It passed anyway: it tests something that was already so.
    Passes,
    /// Its file would not load without the change (an import of something new).
    FileFails,
    /// Not found in the output.
    Unknown,
}

impl Without {
    pub fn words(self) -> &'static str {
        match self {
            Without::Fails => "fails without the change",
            Without::Passes => "passes without the change",
            Without::FileFails => "its file does not load without the change",
            Without::Unknown => "not seen in the run without the change",
        }
    }
}

/// An existing test line the task changed, or took out (`after` is `None`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Changed {
    pub file: String,
    pub line: usize,
    pub before: String,
    pub after: Option<String>,
}

/// One review round's evidence.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Proof {
    pub tests: Vec<(NewTest, Without)>,
    pub changed: Vec<Changed>,
    /// Why the tests were not run without the change, when they were not.
    pub note: Option<String>,
}

impl Proof {
    /// The new tests found, without the run's verdicts.
    pub fn found(tests: Vec<NewTest>, changed: Vec<Changed>, note: Option<String>) -> Self {
        Self {
            tests: tests.into_iter().map(|t| (t, Without::Unknown)).collect(),
            changed,
            note,
        }
    }

    /// What the reviewer reads: the run without the change, then the
    /// existing test lines changed. Empty when there is nothing to say.
    pub fn brief(&self) -> String {
        let mut b = String::new();
        if !self.tests.is_empty() {
            b.push_str("## New tests, run without the change\n");
            match &self.note {
                Some(note) => b.push_str(&format!("Not run: {note}.\n")),
                None => b.push_str(
                    "weft copied the project, put the code (not the tests) back as it was before this task, and ran the new tests there. A test that passes without the change does not test it.\n",
                ),
            }
            for (t, w) in &self.tests {
                let said = if self.note.is_some() {
                    String::new()
                } else {
                    format!(": {}", w.words())
                };
                b.push_str(&format!("- `{}:{}` \"{}\"{said}\n", t.file, t.line, t.name));
            }
            b.push('\n');
        }
        if !self.changed.is_empty() {
            b.push_str("## Existing test lines changed\nLines of tests that were there before this task, changed or taken out:\n");
            for c in &self.changed {
                match &c.after {
                    Some(after) => b.push_str(&format!(
                        "- `{}:{}`\n  was: {}\n  now: {}\n",
                        c.file, c.line, c.before, after
                    )),
                    None => b.push_str(&format!(
                        "- `{}:{}` taken out: {}\n",
                        c.file, c.line, c.before
                    )),
                }
            }
            b.push('\n');
        }
        b
    }

    /// The tests that pass without the change.
    pub fn passing(&self) -> Vec<&NewTest> {
        self.tests
            .iter()
            .filter(|(_, w)| *w == Without::Passes)
            .map(|(t, _)| t)
            .collect()
    }
}

/// `@@ -a,b +c,d @@` → c, the new side's first line.
fn hunk_start(line: &str) -> Option<usize> {
    let plus = line.split_whitespace().find(|w| w.starts_with('+'))?;
    plus[1..].split(',').next()?.parse().ok()
}

/// Lines of a unified diff with the new side's line number: `(n, '+', text)`
/// for added lines, `(n, ' ', text)` for context, and `(n, '-', text)` for
/// removed ones (`n` is where they were, on the new side). A hunk starts a
/// new group.
fn diff_lines(diff: &str) -> Vec<Vec<(usize, char, String)>> {
    let mut hunks = Vec::new();
    let mut current: Vec<(usize, char, String)> = Vec::new();
    let mut n = 0;
    let mut in_hunk = false;
    for line in diff.lines() {
        if line.starts_with("@@") {
            if !current.is_empty() {
                hunks.push(std::mem::take(&mut current));
            }
            n = hunk_start(line).unwrap_or(1);
            in_hunk = true;
            continue;
        }
        if !in_hunk || line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        let (kind, text) = match line.chars().next() {
            Some(c @ ('+' | '-' | ' ')) => (c, &line[1..]),
            _ => continue,
        };
        current.push((n, kind, text.to_string()));
        if kind != '-' {
            n += 1;
        }
    }
    if !current.is_empty() {
        hunks.push(current);
    }
    hunks
}

/// A whole new file as a diff that adds every line.
pub fn as_added(text: &str) -> String {
    let mut out = format!("@@ -0,0 +1,{} @@\n", text.lines().count());
    for l in text.lines() {
        out.push('+');
        out.push_str(l);
        out.push('\n');
    }
    out
}

/// The tests a diff of `file` adds: `it('…')` and `test('…')`, Python's
/// `def test_…`, Rust's `#[test] fn …`.
pub fn new_tests(file: &str, diff: &str) -> Vec<NewTest> {
    let js = Regex::new(r#"^\s*(?:it|test)(?:\.\w+)*\s*\(\s*(['"`])(.+?)['"`]\s*,"#).unwrap();
    let py = Regex::new(r"^\s*(?:async\s+)?def\s+(test_\w+)").unwrap();
    let rs = Regex::new(r"^\s*(?:pub\s+)?(?:async\s+)?fn\s+(\w+)").unwrap();
    let mut out = Vec::new();
    let mut attr = false;
    for hunk in diff_lines(diff) {
        for (n, kind, text) in hunk {
            if kind != '+' {
                attr = false;
                continue;
            }
            if text.contains("test]") {
                attr = true;
                continue;
            }
            let name = js
                .captures(&text)
                .map(|c| c[2].to_string())
                .or_else(|| py.captures(&text).map(|c| c[1].to_string()))
                .or_else(|| {
                    attr.then(|| rs.captures(&text).map(|c| c[1].to_string()))
                        .flatten()
                });
            if !text.trim_start().starts_with("#[") {
                attr = false;
            }
            if let Some(name) = name {
                out.push(NewTest {
                    file: file.to_string(),
                    line: n,
                    name,
                });
            }
        }
    }
    out
}

/// A line that checks something: `expect(…)`, `assert…`, `.should`.
fn is_assertion(text: &str) -> bool {
    let t = text.trim();
    !t.is_empty()
        && !t.starts_with("//")
        && !t.starts_with('#')
        && ["expect(", "assert", ".should", ".to_be(", "toThrow"]
            .iter()
            .any(|k| t.contains(k))
}

#[derive(Debug, PartialEq)]
enum Tok {
    Ident(String),
    Literal(String),
    Punct(char),
}

/// A line as words, literals (strings, numbers, regexes) and punctuation.
fn tokens(text: &str) -> Vec<Tok> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let after_open = matches!(out.last(), Some(Tok::Punct('(' | ',' | '=')) | None);
        if c.is_whitespace() {
            i += 1;
        } else if matches!(c, '\'' | '"' | '`') || (c == '/' && after_open) {
            let start = i;
            i += 1;
            while i < chars.len() && chars[i] != c {
                if chars[i] == '\\' {
                    i += 1;
                }
                i += 1;
            }
            i = (i + 1).min(chars.len());
            out.push(Tok::Literal(chars[start..i].iter().collect()));
        } else if c.is_ascii_digit() {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '.') {
                i += 1;
            }
            out.push(Tok::Literal(chars[start..i].iter().collect()));
        } else if c.is_alphanumeric() || c == '_' || c == '$' {
            let start = i;
            while i < chars.len()
                && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '$')
            {
                i += 1;
            }
            out.push(Tok::Ident(chars[start..i].iter().collect()));
        } else {
            out.push(Tok::Punct(c));
            i += 1;
        }
    }
    out
}

/// Only names differ (a variable renamed): no value, string or step changed.
fn only_renamed(before: &str, after: &str) -> bool {
    let (a, b) = (tokens(before), tokens(after));
    a.len() == b.len()
        && a.iter()
            .zip(&b)
            .all(|(x, y)| x == y || matches!((x, y), (Tok::Ident(_), Tok::Ident(_))))
}

/// The assertions of a diff of an existing test file that were changed or
/// taken out, renames aside. Within each run of removed and added lines
/// (between unchanged ones), the removed assertions pair with the added
/// ones in order; a removed one left over was taken out. Added assertions
/// past the pairs are new checks: nothing was loosened by them.
pub fn changed_lines(file: &str, diff: &str) -> Vec<Changed> {
    let mut out = Vec::new();
    let runs = diff_lines(diff).into_iter().flat_map(|hunk| {
        hunk.split(|(_, k, _)| *k == ' ')
            .map(<[_]>::to_vec)
            .collect::<Vec<_>>()
    });
    for run in runs {
        let removed: Vec<(usize, &String)> = run
            .iter()
            .filter(|(_, k, t)| *k == '-' && is_assertion(t))
            .map(|(n, _, t)| (*n, t))
            .collect();
        let added: Vec<(usize, &String)> = run
            .iter()
            .filter(|(_, k, t)| *k == '+' && is_assertion(t))
            .map(|(n, _, t)| (*n, t))
            .collect();
        for (i, (n, before)) in removed.iter().enumerate() {
            match added.get(i) {
                Some((m, after)) => {
                    if !only_renamed(before, after) && before.trim() != after.trim() {
                        out.push(Changed {
                            file: file.to_string(),
                            line: *m,
                            before: before.trim().to_string(),
                            after: Some(after.trim().to_string()),
                        });
                    }
                }
                None => out.push(Changed {
                    file: file.to_string(),
                    line: *n,
                    before: before.trim().to_string(),
                    after: None,
                }),
            }
        }
    }
    out
}

/// The command that runs some test files, `{files}` standing for them: the
/// project's `test`, or one guessed from its files.
pub fn test_command(root: &Path, configured: Option<&str>) -> Option<String> {
    if let Some(c) = configured {
        return Some(c.to_string());
    }
    let package = std::fs::read_to_string(root.join("package.json")).unwrap_or_default();
    if package.contains("\"vitest\"") {
        return Some("npx vitest run --reporter=verbose {files}".into());
    }
    if package.contains("\"jest\"") {
        return Some("npx jest --verbose {files}".into());
    }
    if root.join("pyproject.toml").exists() || root.join("pytest.ini").exists() {
        return Some("python -m pytest -v {files}".into());
    }
    if root.join("Cargo.toml").exists() {
        return Some("cargo test".into());
    }
    None
}

/// `command` with `{files}` filled in, each quoted for the shell.
pub fn with_files(command: &str, files: &[String]) -> String {
    let quoted: Vec<String> = files
        .iter()
        .map(|f| format!("'{}'", f.replace('\'', r"'\''")))
        .collect();
    command.replace("{files}", &quoted.join(" "))
}

/// Where `line` names the test `name` itself, not a longer name that starts
/// with it: what comes before and after it, the runner's marks and timing.
fn naming<'l>(line: &'l str, name: &str) -> Option<(&'l str, &'l str)> {
    let ok_after = |rest: &str| {
        let r = rest.trim_start();
        r.is_empty()
            || r.starts_with(|c: char| c.is_ascii_digit())
            || ["...", "PASSED", "FAILED", "SKIPPED", "[", "(", "✓", "×"]
                .iter()
                .any(|m| r.starts_with(m))
    };
    line.match_indices(name).find_map(|(at, _)| {
        let (before, rest) = (&line[..at], &line[at + name.len()..]);
        let clean_start = before
            .chars()
            .last()
            .is_none_or(|c| !c.is_alphanumeric() && c != '_');
        (clean_start && ok_after(rest)).then_some((before, rest))
    })
}

/// Each test's fate in the output of a run: found by its name on a line
/// with a pass or fail mark, else its file failing as a whole.
pub fn read_results(output: &str, tests: &[NewTest]) -> Vec<Without> {
    tests
        .iter()
        .map(|t| {
            for (before, after) in output.lines().filter_map(|l| naming(l, &t.name)) {
                let marks = format!("{before} {after}");
                if marks.contains('✓') || marks.contains('✔') {
                    return Without::Passes;
                }
                if marks.contains('×') || marks.contains('✗') || marks.contains('✘') {
                    return Without::Fails;
                }
                let after = after.trim();
                if after.starts_with("... ok") || after.starts_with("PASSED") {
                    return Without::Passes;
                }
                if after.starts_with("... FAILED")
                    || after.starts_with("FAILED")
                    || before.contains("FAIL")
                {
                    return Without::Fails;
                }
            }
            let base = t.file.rsplit('/').next().unwrap_or(&t.file);
            let file_failed = output
                .lines()
                .any(|l| l.contains(base) && (l.contains("FAIL") || l.contains("Error")));
            if file_failed {
                Without::FileFails
            } else {
                Without::Unknown
            }
        })
        .collect()
}

/// Run `command` in a copy of `root` where `code` (the task's changed files
/// that are not tests) is as it was before the task. The copy is removed
/// after. Returns what the run printed.
pub async fn run_without(
    root: &Path,
    checkpoint: Option<&Checkpoint>,
    code: &[String],
    command: &str,
    timeout: u64,
) -> Result<String, String> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let copy = std::env::temp_dir().join(format!("weft-without-{}-{stamp}", std::process::id()));
    let result = async {
        copy_tree(root, &copy).await?;
        if let Some(cp) = checkpoint {
            cp.note_git_after(root);
        }
        for rel in code {
            put_back(root, &copy, checkpoint, rel)
                .await
                .map_err(|e| format!("putting back {rel}: {e}"))?;
        }
        run_shell(&copy, command, timeout)
            .await
            .map(|r| r.full)
            .map_err(|e| e.to_string())
    }
    .await;
    let gone = copy.clone();
    let _ = tokio::task::spawn_blocking(move || std::fs::remove_dir_all(gone)).await;
    result
}

/// A copy of the folder, cheap where the disk can clone files.
async fn copy_tree(from: &Path, to: &Path) -> Result<(), String> {
    let attempts: &[&[&str]] = if cfg!(target_os = "macos") {
        &[&["-c", "-R"], &["-R"]]
    } else {
        &[&["-R", "--reflink=auto"], &["-R"]]
    };
    let mut last = String::new();
    for flags in attempts {
        let _ = tokio::fs::remove_dir_all(to).await;
        let out = tokio::process::Command::new("cp")
            .args(*flags)
            .arg(from)
            .arg(to)
            .output()
            .await
            .map_err(|e| e.to_string())?;
        if out.status.success() {
            return Ok(());
        }
        last = String::from_utf8_lossy(&out.stderr).trim().to_string();
    }
    Err(format!("could not copy the project: {last}"))
}

/// One file in the copy as it was before the task.
async fn put_back(
    root: &Path,
    copy: &Path,
    checkpoint: Option<&Checkpoint>,
    rel: &str,
) -> std::io::Result<()> {
    let dest = copy.join(rel);
    let before = checkpoint
        .map(|cp| cp.before(rel))
        .unwrap_or(Before::Git("HEAD".into()));
    let from_git = |base: String| async move {
        tokio::process::Command::new("git")
            .args(["show", &format!("{base}:{rel}")])
            .current_dir(root)
            .output()
            .await
            .ok()
            .filter(|o| o.status.success())
            .map(|o| o.stdout)
    };
    let content: Option<Vec<u8>> = match before {
        Before::Kept(path) => Some(tokio::fs::read(path).await?),
        Before::Absent => None,
        Before::Git(base) => from_git(base).await,
    };
    match content {
        Some(bytes) => {
            if let Some(parent) = dest.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::write(&dest, bytes).await
        }
        None => match tokio::fs::remove_file(&dest).await {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        },
    }
}

/// The diff of one file against git's HEAD, or the whole file as added when
/// git has nothing to compare it with. `None` when it is gone.
pub async fn file_diff(root: &Path, rel: &str) -> Option<(String, bool)> {
    let path: PathBuf = root.join(rel);
    if !path.is_file() {
        return None;
    }
    let git = tokio::process::Command::new("git")
        .args(["diff", "HEAD", "--", rel])
        .current_dir(root)
        .output()
        .await;
    match git {
        Ok(o) if o.status.success() && !o.stdout.is_empty() => {
            let diff = String::from_utf8_lossy(&o.stdout).into_owned();
            let new = diff.contains("\nnew file mode");
            Some((diff, new))
        }
        _ => {
            let text = tokio::fs::read_to_string(&path).await.ok()?;
            Some((as_added(&text), true))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_files_are_known_by_their_names() {
        for yes in [
            "src/App.test.tsx",
            "server/store.spec.ts",
            "pkg/a_test.go",
            "tests/cli.rs",
            "app/test_models.py",
            "src/__tests__/x.js",
        ] {
            assert!(is_test_file(yes), "{yes}");
        }
        for no in ["src/App.tsx", "src/testing.ts", "src/lib/contest.ts"] {
            assert!(!is_test_file(no), "{no}");
        }
    }

    const DIFF: &str = "\
diff --git a/server/store.test.ts b/server/store.test.ts
--- a/server/store.test.ts
+++ b/server/store.test.ts
@@ -1005,7 +1005,7 @@ describe('store', () => {
     expect(w.weekByKind).toMatchObject({ yoga: 1, run: 1 })
-    expect(store.getDay(addDays(week, 6)).workouts.planIsCustom).toBe(true)
+    expect(store.getDay(addDays(week, 5)).workouts.planIsCustom).toBe(true)
     expect(store.getGoals().workoutPlan.yoga).toBe(4)
@@ -1190,4 +1190,4 @@
-    expect(store.getRange(sunday, addDays(sunday, 6)).workouts.goalByKind).toEqual({ yoga: 3 })
+    expect(store.getRange(monday, addDays(monday, 6)).workouts.goalByKind).toEqual({ yoga: 3 })
-    expect(() => store.setWeekPlan(x)).toThrow(/must be a Sunday/)
+    expect(() => store.setWeekPlan(x)).toThrow(/must be a Monday/)
-    expect(day.alcohol.weekTotal).toBe(1)
@@ -1300,0 +1300,6 @@
+  it('shows the week as Monday–Sunday, matching the Training tab', async () => {
+    expect(1).toBe(1)
+  })
+  test.each([1, 2])(\"counts %s\", () => {})
+  it(\"keeps a custom plan\", async () => {
+  })
";

    #[test]
    fn a_loosened_check_is_found_and_a_rename_is_not() {
        let changed = changed_lines("server/store.test.ts", DIFF);
        assert_eq!(changed.len(), 3, "{changed:#?}");
        assert_eq!(changed[0].line, 1006);
        assert!(changed[0].before.contains("addDays(week, 6)"));
        assert!(
            changed[0]
                .after
                .as_deref()
                .unwrap()
                .contains("addDays(week, 5)")
        );
        assert!(
            changed[1].before.contains("must be a Sunday"),
            "a regex is a value"
        );
        assert_eq!(changed[2].after, None, "taken out");
        assert_eq!(changed[2].before, "expect(day.alcohol.weekTotal).toBe(1)");
    }

    #[test]
    fn added_tests_are_found_with_their_lines() {
        let tests = new_tests("server/store.test.ts", DIFF);
        let names: Vec<(&str, usize)> = tests.iter().map(|t| (t.name.as_str(), t.line)).collect();
        assert_eq!(
            names,
            [
                (
                    "shows the week as Monday–Sunday, matching the Training tab",
                    1300
                ),
                ("keeps a custom plan", 1304)
            ]
        );
        let rust = as_added("#[tokio::test]\nasync fn sums_the_week() {}\nfn helper() {}\n");
        let found = new_tests("tests/a.rs", &rust);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "sums_the_week");
        assert_eq!(found[0].line, 2);
        let py = new_tests("test_a.py", &as_added("def test_sums():\n    pass\n"));
        assert_eq!(py[0].name, "test_sums");
    }

    #[test]
    fn results_are_read_from_vitest_and_cargo_output() {
        let t = |name: &str, file: &str| NewTest {
            file: file.into(),
            line: 1,
            name: name.into(),
        };
        let tests = [
            t("shows the week", "src/App.test.tsx"),
            t("counts water", "src/App.test.tsx"),
            t("sums_the_week", "tests/a.rs"),
            t("loads", "src/New.test.tsx"),
            t("never ran", "src/Other.test.tsx"),
        ];
        let output = "\
 ✓ src/App.test.tsx > App > Week > shows the week as Monday–Sunday 4ms
 ✓ src/App.test.tsx > App > Week > counts water 12ms
 × src/App.test.tsx > App > Week > shows the week 30ms
test sums_the_week ... FAILED
 FAIL  src/New.test.tsx [ src/New.test.tsx ]
Error: Failed to resolve import \"./New\"
";
        assert_eq!(
            read_results(output, &tests),
            [
                Without::Fails,
                Without::Passes,
                Without::Fails,
                Without::FileFails,
                Without::Unknown
            ]
        );
    }

    #[test]
    fn files_are_quoted_into_the_command() {
        assert_eq!(
            with_files(
                "npx vitest run {files}",
                &["a b.test.ts".into(), "it's.test.ts".into()]
            ),
            r"npx vitest run 'a b.test.ts' 'it'\''s.test.ts'"
        );
        assert_eq!(
            with_files("cargo test", &["tests/a.rs".into()]),
            "cargo test"
        );
    }

    #[tokio::test]
    async fn the_code_is_put_back_in_a_copy_and_the_tree_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("p");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.txt"), "before").unwrap();
        let cp_dir = dir.path().join("cp");
        let cp = Checkpoint::new(cp_dir);
        cp.before_write(&root, "src/a.txt").unwrap();
        cp.before_write(&root, "src/new.txt").unwrap();
        std::fs::write(root.join("src/a.txt"), "after").unwrap();
        std::fs::write(root.join("src/new.txt"), "new").unwrap();
        let out = run_without(
            &root,
            Some(&cp),
            &["src/a.txt".into(), "src/new.txt".into()],
            "cat src/a.txt; echo; test -e src/new.txt && echo still-there || echo gone",
            30,
        )
        .await
        .unwrap();
        assert_eq!(
            out.split_whitespace().collect::<Vec<_>>(),
            ["before", "gone"]
        );
        assert_eq!(
            std::fs::read_to_string(root.join("src/a.txt")).unwrap(),
            "after"
        );
        assert!(root.join("src/new.txt").exists());
    }
}
