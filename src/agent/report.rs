//! The result of a task, for the card the screen shows when it ends: whether
//! the plan is complete, which test covers each `## Done when` line and how
//! it did without the change, and what is left.

use regex::Regex;
use serde::{Deserialize, Serialize};

use super::proof::{Changed, NewTest, Proof, Without};
use super::tandem::TandemOutcome;
use crate::tools::todo::Todo;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Report {
    pub task: String,
    /// The plan's `## What you asked for`, when there was a plan.
    pub asked: Option<String>,
    pub planned: bool,
    /// Nothing open, nothing failing, approved, every `Done when` line tested.
    pub complete: bool,
    /// What keeps it from complete.
    pub problems: Vec<String>,
    /// Complete or not, what deserves a look.
    pub warnings: Vec<String>,
    pub done_when: Vec<Covered>,
    /// Checklist steps done, of how many.
    pub steps: (usize, usize),
    /// The steps still open.
    pub open: Vec<String>,
    pub checks: Option<bool>,
    /// `approved`, `approved after 1 fix round`, `fixes pending`, …
    pub review: String,
    pub changed: Vec<Changed>,
    /// Why the new tests were not run without the change, when they were not.
    pub without_note: Option<String>,
    pub files: Vec<String>,
    pub seconds: u64,
}

/// One `## Done when` line and the test the reviewer found for it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Covered {
    pub text: String,
    /// As the reviewer cited it: `` `path:line` "name" ``. `None`: no test.
    pub test: Option<String>,
    /// How that test did without the change, when weft could tell.
    pub without: Option<Without>,
}

impl Report {
    /// The card's headline: `plan complete`, `plan incomplete`, `done`, …
    pub fn headline(&self) -> String {
        let what = if self.planned { "plan" } else { "task" };
        match (self.complete, self.warnings.len()) {
            (false, _) => format!("{what} incomplete"),
            (true, 0) => format!("{what} complete"),
            (true, 1) => format!("{what} complete · 1 thing to look at"),
            (true, n) => format!("{what} complete · {n} things to look at"),
        }
    }

    /// One line for a notification or a listing.
    pub fn summary(&self) -> String {
        let mut s = self.headline();
        if let Some(first) = self.problems.first() {
            s.push_str(&format!(": {first}"));
            if self.problems.len() > 1 {
                s.push_str(&format!(" (+{} more)", self.problems.len() - 1));
            }
        }
        s
    }
}

/// The reviewer's `Done when:` block, taken out of what it said: one entry
/// per numbered line (`none` where it found no test), and the rest.
pub fn coverage(text: &str) -> (Vec<String>, String) {
    let lines: Vec<&str> = text.lines().collect();
    let head = lines.iter().position(|l| {
        let t = l
            .trim()
            .trim_matches(|c: char| c == '*' || c == '#' || c == ' ')
            .to_lowercase();
        t.starts_with("done when") && t.trim_end_matches(':').trim() == "done when"
    });
    let Some(head) = head else {
        return (Vec::new(), text.to_string());
    };
    let mut items: Vec<(usize, String)> = Vec::new();
    let mut end = head + 1;
    while end < lines.len() {
        let l = lines[end].trim();
        if l.is_empty() {
            if items.is_empty() {
                end += 1;
                continue;
            }
            break;
        }
        let rest = l.trim_start_matches(|c: char| c.is_ascii_digit());
        let digits = &l[..l.len() - rest.len()];
        match digits.parse::<usize>() {
            Ok(n) if rest.starts_with('.') || rest.starts_with(')') => {
                items.push((n, rest[1..].trim().to_string()));
                end += 1;
            }
            _ => break,
        }
    }
    let count = items.iter().map(|(n, _)| *n).max().unwrap_or(0);
    let mut out = vec!["none".to_string(); count];
    for (n, t) in items {
        if n >= 1 {
            out[n - 1] = t;
        }
    }
    let rest: Vec<&str> = lines[..head].iter().chain(&lines[end..]).copied().collect();
    (out, rest.join("\n").trim().to_string())
}

/// A coverage line that says there is no test.
fn says_none(line: &str) -> bool {
    let l = line.trim().trim_matches('`').to_lowercase();
    l.is_empty() || l.starts_with("none") || l.starts_with("no test") || l.starts_with("missing")
}

/// How the test a coverage line cites did without the change: matched by
/// its name, else the new test in the same file that starts at or before
/// the cited line.
fn without_for<'p>(line: &str, proof: &'p Proof) -> Option<&'p (NewTest, Without)> {
    if let Some(found) = proof.tests.iter().find(|(t, _)| line.contains(&t.name)) {
        return Some(found);
    }
    let cite = Regex::new(r"([\w./-]+\.\w+):(\d+)").unwrap();
    let c = cite.captures(line)?;
    let (file, at): (&str, usize) = (c.get(1)?.as_str(), c[2].parse().ok()?);
    proof
        .tests
        .iter()
        .filter(|(t, _)| (t.file.ends_with(file) || file.ends_with(&t.file)) && t.line <= at)
        .max_by_key(|(t, _)| t.line)
}

/// The result of a task, from what it did. `cover` is the last review's
/// `Done when:` block (empty without one), `proof` the last round's evidence.
pub fn build(
    task: &str,
    plan: Option<&str>,
    out: &TandemOutcome,
    todos: &[Todo],
    cover: &[String],
    proof: Option<&Proof>,
    seconds: u64,
) -> Report {
    let empty = Proof::default();
    let proof = proof.unwrap_or(&empty);
    let done_when_lines = plan
        .map(|p| super::tandem::numbered(super::tandem::section(p, "## Done when")))
        .unwrap_or_default();
    let reviewed = !out.reviews.is_empty() && !out.review_inconclusive;
    let mut problems = Vec::new();
    let mut warnings = Vec::new();

    // The new tests the coverage lines name, so the rest can be listed apart.
    let mut matched: Vec<&NewTest> = Vec::new();
    let done_when: Vec<Covered> = done_when_lines
        .iter()
        .enumerate()
        .map(|(i, text)| {
            let cited = cover.get(i).filter(|c| !says_none(c)).cloned();
            let found = cited.as_deref().and_then(|c| without_for(c, proof));
            if let Some((t, _)) = found {
                matched.push(t);
            }
            Covered {
                text: text.clone(),
                test: cited,
                without: found.map(|(_, w)| *w).filter(|_| proof.note.is_none()),
            }
        })
        .collect();
    let open: Vec<String> = todos
        .iter()
        .filter(|t| t.is_open())
        .map(|t| t.text.clone())
        .collect();
    let steps = crate::tools::todo::progress(todos);

    if !open.is_empty() {
        problems.push(format!(
            "{} step{} still open",
            open.len(),
            if open.len() == 1 { "" } else { "s" }
        ));
    }
    if out.checks == Some(false) {
        problems.push("the checks fail".into());
    }
    let review = match out.reviews.last() {
        _ if out.review_inconclusive => {
            problems.push("the review was inconclusive".into());
            "inconclusive".to_string()
        }
        None if out.touched.is_empty() => "nothing changed to review".to_string(),
        None => "not reviewed".to_string(),
        Some(_) if out.fixes_pending => {
            problems.push("the reviewer's last asks were not made".into());
            "fixes pending".to_string()
        }
        Some((true, _)) => match out.reviews.len() {
            1 => "approved".to_string(),
            n => format!(
                "approved after {} fix round{}",
                n - 1,
                if n == 2 { "" } else { "s" }
            ),
        },
        Some((false, _)) => {
            problems.push("the reviewer did not approve".into());
            "not approved".to_string()
        }
    };
    if reviewed && !cover.is_empty() {
        for (i, c) in done_when.iter().enumerate() {
            if c.test.is_none() {
                problems.push(format!("no test for Done when {}", i + 1));
            }
        }
    }
    for (i, c) in done_when.iter().enumerate() {
        if c.without == Some(Without::Passes) {
            warnings.push(format!(
                "Done when {}: its test passes without the change",
                i + 1
            ));
        }
    }
    for t in proof.passing() {
        if !matched.contains(&t) {
            warnings.push(format!("\"{}\" passes without the change", t.name));
        }
    }
    if !proof.changed.is_empty() {
        warnings.push(format!(
            "{} existing test line{} changed",
            proof.changed.len(),
            if proof.changed.len() == 1 { "" } else { "s" }
        ));
    }

    let asked = plan.map(|p| {
        super::tandem::section(p, "## What you asked for")
            .trim()
            .to_string()
    });
    Report {
        task: task.to_string(),
        asked: asked.filter(|a| !a.is_empty()),
        planned: plan.is_some(),
        complete: problems.is_empty(),
        problems,
        warnings,
        done_when,
        steps,
        open,
        checks: out.checks,
        review,
        changed: proof.changed.clone(),
        without_note: proof.note.clone().filter(|_| !proof.tests.is_empty()),
        files: out.touched.clone(),
        seconds,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::todo::Status;

    #[test]
    fn the_done_when_block_is_taken_out_of_the_review() {
        let (cover, rest) = coverage(
            "**Done when:**\n1. `src/App.test.tsx:1910` \"shows the week\"\n3. none\n\n1. `a.ts`: fix it",
        );
        assert_eq!(
            cover,
            ["`src/App.test.tsx:1910` \"shows the week\"", "none", "none"]
        );
        assert_eq!(rest, "1. `a.ts`: fix it");
        let (cover, rest) = coverage("Nothing to add");
        assert!(cover.is_empty());
        assert_eq!(rest, "Nothing to add");
    }

    const PLAN: &str = "## What you asked for\nWeeks start on Monday.\n\n## Done when\n1. The week shows Mon–Sun.\n2. Water counts from Monday.\n3. Headings keep their order.\n\n## Steps\n1. `a.ts`: flip it";

    fn proof() -> Proof {
        let t = |name: &str, line| NewTest {
            file: "src/App.test.tsx".into(),
            line,
            name: name.into(),
        };
        Proof {
            tests: vec![
                (t("shows the week", 1900), Without::Fails),
                (t("counts water", 2080), Without::Passes),
                (t("an extra", 2200), Without::Passes),
            ],
            changed: vec![Changed {
                file: "server/store.test.ts".into(),
                line: 1009,
                before: "addDays(week, 6)".into(),
                after: Some("addDays(week, 5)".into()),
            }],
            note: None,
        }
    }

    fn approved() -> TandemOutcome {
        TandemOutcome {
            reviews: vec![(false, "1. x".into()), (true, "Nothing to add".into())],
            checks: Some(true),
            touched: vec!["src/a.ts".into()],
            ..Default::default()
        }
    }

    #[test]
    fn a_tested_approved_plan_is_complete_with_what_to_look_at() {
        let cover = [
            "`src/App.test.tsx:1910` \"shows the week\"".to_string(),
            "`src/App.test.tsx:2085`".to_string(),
            "`src/App.test.tsx:1700` (an old test)".to_string(),
        ];
        let todos = [Todo {
            text: "a".into(),
            status: Status::Done,
        }];
        let r = build(
            "t",
            Some(PLAN),
            &approved(),
            &todos,
            &cover,
            Some(&proof()),
            60,
        );
        assert!(r.complete, "{:?}", r.problems);
        assert_eq!(r.asked.as_deref(), Some("Weeks start on Monday."));
        assert_eq!(r.review, "approved after 1 fix round");
        assert_eq!(r.done_when[0].without, Some(Without::Fails));
        assert_eq!(
            r.done_when[1].without,
            Some(Without::Passes),
            "matched by line"
        );
        assert_eq!(r.done_when[2].without, None, "an older test: not run");
        assert_eq!(
            r.warnings,
            [
                "Done when 2: its test passes without the change",
                "\"an extra\" passes without the change",
                "1 existing test line changed"
            ]
        );
        assert_eq!(r.headline(), "plan complete · 3 things to look at");
    }

    #[test]
    fn open_steps_a_missing_test_and_pending_fixes_make_it_incomplete() {
        let out = TandemOutcome {
            reviews: vec![(false, "1. x".into())],
            fixes_pending: true,
            checks: Some(true),
            touched: vec!["src/a.ts".into()],
            ..Default::default()
        };
        let todos = [Todo::pending("Fix: x")];
        let cover = ["`a.test.ts:3`".to_string(), "none".into()];
        let r = build("t", Some(PLAN), &out, &todos, &cover, None, 60);
        assert!(!r.complete);
        assert_eq!(
            r.problems,
            [
                "1 step still open",
                "the reviewer's last asks were not made",
                "no test for Done when 2",
                "no test for Done when 3"
            ]
        );
        assert_eq!(r.summary(), "plan incomplete: 1 step still open (+3 more)");
        assert_eq!(r.open, ["Fix: x"]);
    }
}
