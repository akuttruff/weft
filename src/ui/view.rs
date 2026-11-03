//! Turning transcript pieces into styled lines: tool calls, plans, reviews,
//! and the answer text with a little markdown.

use std::sync::LazyLock;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use regex::Regex;
use unicode_width::UnicodeWidthChar;

use super::diff::{Kind, Row};
use crate::agent::proof::Without;
use crate::agent::report::Report;
use crate::lm::Usage;
use crate::mcp::Status as McpStatus;
use crate::tools::todo::{self, Status, Todo};
use crate::transcript::Saved;

/// `> what the user typed`
pub fn user_line(text: &str) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from("")];
    for (i, l) in text.lines().enumerate() {
        let prefix = if i == 0 { "> " } else { "  " };
        lines.push(Line::from(vec![
            Span::styled(prefix, Style::new().bold()),
            Span::styled(l.to_string(), Style::new().bold()),
        ]));
    }
    lines
}

/// `/resume`: the folder's earlier sessions, numbered for picking.
pub fn session_lines(sessions: &[Saved]) -> Vec<Line<'static>> {
    if sessions.is_empty() {
        return note_line("no other sessions in this folder");
    }
    let mut lines = Vec::new();
    for (i, s) in sessions.iter().enumerate().take(10) {
        let first = s
            .tasks
            .first()
            .and_then(|t| t.lines().next())
            .unwrap_or("(no task)");
        lines.push(Line::from(vec![
            Span::styled(format!("  {:>2}  ", i + 1), Style::new().fg(Color::Cyan)),
            Span::styled(
                format!(
                    "{}  {} · {} task{}  ",
                    s.id,
                    when(&s.started),
                    s.tasks.len(),
                    if s.tasks.len() == 1 { "" } else { "s" }
                ),
                Style::new().dim(),
            ),
            Span::raw(clip(first, 60)),
        ]));
    }
    if sessions.len() > 10 {
        lines.push(Line::from(Span::styled(
            format!(
                "      … and {} more; `weft sessions` lists them all",
                sessions.len() - 10
            ),
            Style::new().dim(),
        )));
    }
    lines.push(Line::from(Span::styled(
        "  type a number to pick one up, or /resume <number or id>",
        Style::new().dim(),
    )));
    lines
}

/// The conversation a resumed session carries: each task and the first line
/// of its answer, so you can see where you were.
pub fn replay_lines(saved: &Saved) -> Vec<Line<'static>> {
    let mut lines = note_line(&format!(
        "resumed session {} · started {} · {} task{} · context {} tokens",
        saved.id,
        when(&saved.started),
        saved.tasks.len(),
        if saved.tasks.len() == 1 { "" } else { "s" },
        k(saved.context),
    ));
    // Tasks folded away by compaction have no messages left to replay; say
    // so, and name them, rather than leaving a gap between the count and
    // what is shown.
    let exchanges = saved.exchanges();
    let replayed: Vec<&str> = exchanges.iter().map(|(t, _)| t.as_str()).collect();
    let folded: Vec<&str> = saved
        .tasks
        .iter()
        .map(String::as_str)
        .filter(|t| !replayed.contains(t))
        .collect();
    if !folded.is_empty() {
        lines.extend(note_line(&format!(
            "  {} earlier task{} folded into the note the worker carries:",
            folded.len(),
            if folded.len() == 1 { "" } else { "s" }
        )));
        for t in folded {
            lines.push(Line::from(Span::styled(
                format!("    > {}", clip(t.lines().next().unwrap_or(""), 90)),
                Style::new().dim(),
            )));
        }
    }
    for (task, answer) in exchanges {
        lines.extend(user_line(&task));
        let mut rest = answer.lines().filter(|l| !l.trim().is_empty());
        if let Some(first) = rest.next() {
            lines.push(answer_line(&clip(first, 120), true));
            if rest.next().is_some() {
                lines.push(Line::from(Span::styled("  …", Style::new().dim())));
            }
        }
    }
    // The last task's plan, shown again: a task stopped at its plan has no
    // exchange above, so its line comes first.
    if let Some((task, plan)) = saved.last_plan() {
        let shown = saved.exchanges().last().is_some_and(|(t, _)| t == task);
        if !shown {
            lines.extend(user_line(task));
        }
        lines.extend(note_line("  the plan from last time:"));
        lines.extend(plan_view(plan));
    }
    if saved.todos.iter().any(Todo::is_open) {
        lines.extend(todo_lines(&saved.todos));
    }
    lines
}

/// `2026-10-25T14:19:01-07:00` → `2026-10-25 14:19`.
pub fn when(at: &str) -> String {
    match (at.get(..10), at.get(11..16)) {
        (Some(day), Some(time)) => format!("{day} {time}"),
        _ => at.to_string(),
    }
}

pub fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

/// `✻ planning`
pub fn phase_line(name: &str) -> Vec<Line<'static>> {
    vec![Line::from(vec![
        Span::styled("✻ ", Style::new().fg(Color::Magenta)),
        Span::styled(name.to_string(), Style::new().bold()),
    ])]
}

/// The checklist, as Claude Code shows it: a heading with the count, then
/// `☒` done (struck through), `◼` under way (bold), `☐` not started.
pub fn todo_lines(items: &[Todo]) -> Vec<Line<'static>> {
    if items.is_empty() {
        return vec![Line::from(vec![
            Span::styled("⏺ ", Style::new().fg(Color::Green)),
            Span::raw("Todos cleared"),
        ])];
    }
    let (done, total) = todo::progress(items);
    let mut out = vec![Line::from(vec![
        Span::styled("⏺ ", Style::new().fg(Color::Green)),
        Span::raw("Todos "),
        Span::styled(format!("{done} of {total} done"), Style::new().dim()),
    ])];
    for (i, t) in items.iter().enumerate() {
        let style = match t.status {
            Status::Done => Style::new().dim().add_modifier(Modifier::CROSSED_OUT),
            Status::InProgress => Style::new().bold(),
            Status::Pending => Style::new(),
        };
        let mark = match t.status {
            Status::InProgress => Style::new().fg(Color::Magenta),
            Status::Done => Style::new().fg(Color::Green),
            Status::Pending => Style::new().dim(),
        };
        out.push(Line::from(vec![
            Span::styled(if i == 0 { "  ⎿ " } else { "    " }, Style::new().dim()),
            Span::styled(format!("{} ", t.mark()), mark),
            Span::styled(t.text.clone(), style),
        ]));
    }
    out
}

/// `⏺ Read src/cli.js`
pub fn tool_line(summary: &str) -> Vec<Line<'static>> {
    vec![Line::from(vec![
        Span::styled("⏺ ", Style::new().fg(Color::Green)),
        Span::raw(summary.to_string()),
    ])]
}

/// The line a result is summed up by: its first, or for a failure the last
/// line that names one (`Tests  1 failed | 631 passed`), so a run of checks
/// that failed in its tests is not shown by the lint's "0 errors" before them.
fn headline(ok: bool, output: &str) -> &str {
    static FAILURE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)\b[1-9]\d*\s+(failed|failures?|errors?)\b|^\s*(FAIL|error)\b").unwrap()
    });
    let first = output.lines().next().unwrap_or("(no output)");
    if ok {
        return first;
    }
    output
        .lines()
        .rev()
        .find(|l| FAILURE.is_match(l))
        .map(str::trim)
        .unwrap_or(first)
}

/// `  ⎿ the result's headline (+n lines)`, or `Read 18 lines` for a file.
pub fn result_line(ok: bool, output: &str, tool: &str) -> Vec<Line<'static>> {
    let n = output.lines().count();
    let summary = if ok && tool == "read_file" {
        let numbered = output
            .lines()
            .filter(|l| {
                l.trim_start().starts_with(|c: char| c.is_ascii_digit()) && l.contains('\t')
            })
            .count();
        format!(
            "Read {numbered} line{}",
            if numbered == 1 { "" } else { "s" }
        )
    } else {
        let first: String = headline(ok, output)
            .replace('\t', "  ")
            .chars()
            .take(110)
            .collect();
        let more = if n > 1 {
            format!(" (+{} lines)", n - 1)
        } else {
            String::new()
        };
        format!("{first}{more}")
    };
    let style = if ok {
        Style::new().dim()
    } else {
        Style::new().fg(Color::Red)
    };
    vec![Line::from(vec![
        Span::styled("  ⎿ ", style),
        Span::styled(summary, style),
    ])]
}

/// One dim line at startup: `mcp: deep-research 4 tools · notes failed`.
pub fn mcp_summary(servers: &[McpStatus]) -> Vec<Line<'static>> {
    let mut spans = vec![Span::styled("mcp: ", Style::new().dim())];
    for (i, s) in servers.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", Style::new().dim()));
        }
        match &s.tools {
            Ok(tools) => spans.push(Span::styled(
                format!(
                    "{} {} tool{}",
                    s.name,
                    tools.len(),
                    if tools.len() == 1 { "" } else { "s" }
                ),
                Style::new().dim(),
            )),
            Err(_) => spans.push(Span::styled(
                format!("{} failed", s.name),
                Style::new().fg(Color::Red),
            )),
        }
    }
    vec![Line::from(spans)]
}

/// `/mcp`: each server with its tools, or why it is not there.
pub fn mcp_lines(servers: &[McpStatus]) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for s in servers {
        match &s.tools {
            Ok(tools) => {
                out.push(Line::from(vec![
                    Span::styled(format!("  {}", s.name), Style::new().bold()),
                    Span::styled(
                        format!(
                            " · {} tool{}",
                            tools.len(),
                            if tools.len() == 1 { "" } else { "s" }
                        ),
                        Style::new().dim(),
                    ),
                ]));
                for t in tools {
                    out.push(Line::from(Span::styled(
                        format!("    {}__{t}", s.name),
                        Style::new().fg(Color::Cyan),
                    )));
                }
            }
            Err(e) => out.push(Line::from(vec![
                Span::styled(format!("  {}", s.name), Style::new().bold()),
                Span::styled(format!(" · failed: {e}"), Style::new().fg(Color::Red)),
            ])),
        }
    }
    out
}

/// What a wrapped diff line's continuation starts with: past the number and the sign.
pub const DIFF_INDENT: &str = "           ";

/// The code an edit changed, under its result line: `    12 -  old` in red,
/// `    12 +  new` in green, unchanged lines plain, numbers dim.
pub fn diff_lines(rows: &[Row], hidden: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for row in rows {
        let (sign, style) = match row.kind {
            Kind::Removed => ("-", Style::new().fg(Color::Red)),
            Kind::Added => ("+", Style::new().fg(Color::Green)),
            Kind::Same => (" ", Style::new()),
        };
        out.push(Line::from(vec![
            Span::styled(format!("    {:>4} ", row.line), Style::new().dim()),
            Span::styled(format!("{sign} "), style),
            Span::styled(row.text.replace('\t', "    "), style),
        ]));
    }
    if hidden > 0 {
        out.push(Line::from(Span::styled(
            format!(
                "{DIFF_INDENT}… {hidden} more line{}",
                if hidden == 1 { "" } else { "s" }
            ),
            Style::new().dim(),
        )));
    }
    out
}

/// The plan, in a cyan gutter.
pub fn plan_lines(text: &str) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    // Code fences carry nothing on a terminal; the code between them stays.
    for l in text.lines().filter(|l| !l.trim_start().starts_with("```")) {
        let mut spans = vec![Span::styled("  │ ", Style::new().fg(Color::Cyan))];
        let style = if l.starts_with("## ") {
            Style::new().bold()
        } else if l.contains('⚠') {
            // A choice that departs from what was asked: the line to read.
            Style::new().fg(Color::Yellow)
        } else {
            Style::new()
        };
        spans.extend(inline(l, style));
        out.push(Line::from(spans));
    }
    out
}

/// A plan as the person reads it: their part whole, the worker's folded
/// to one line that `/steps` opens.
pub fn plan_view(text: &str) -> Vec<Line<'static>> {
    let (mine, worker) = crate::agent::tandem::split_plan(text);
    let mut out = plan_lines(mine);
    if let Some(worker) = worker {
        let steps = worker
            .lines()
            .skip(1)
            .take_while(|l| !l.starts_with("## "))
            .filter(|l| {
                let t = l.trim_start();
                let rest = t.trim_start_matches(|c: char| c.is_ascii_digit());
                rest.len() < t.len() && (rest.starts_with('.') || rest.starts_with(')'))
            })
            .count();
        out.push(Line::from(vec![
            Span::styled("  │ ", Style::new().fg(Color::Cyan)),
            Span::styled(
                format!(
                    "── {steps} step{} for the worker, with its checks and pitfalls · /steps shows them",
                    if steps == 1 { "" } else { "s" }
                ),
                Style::new().dim(),
            ),
        ]));
    }
    out
}

pub fn review_lines(approved: bool, inconclusive: bool, text: &str) -> Vec<Line<'static>> {
    if inconclusive {
        let mut out = vec![Line::from(Span::styled(
            "  ? review inconclusive: not approved, and nothing sent back",
            Style::new().fg(Color::Yellow),
        ))];
        out.extend(
            text.lines()
                .map(|l| Line::from(Span::styled(format!("    {l}"), Style::new().dim()))),
        );
        return out;
    }
    // The reviewer writes lines, and a little markdown: each line stays a
    // line, with its **bold** and `code` drawn rather than spelt out.
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    if approved {
        let mut first = vec![Span::styled("  ✔ approved ", Style::new().fg(Color::Green))];
        first.extend(inline(lines.next().unwrap_or(""), Style::new().dim()));
        let mut out = vec![Line::from(first)];
        for l in lines {
            let mut spans = vec![Span::raw("    ")];
            spans.extend(inline(l, Style::new().dim()));
            out.push(Line::from(spans));
        }
        return out;
    }
    let mut out = vec![Line::from(Span::styled(
        "  ✗ fixes requested",
        Style::new().fg(Color::Yellow),
    ))];
    for l in lines {
        let mut spans = vec![Span::raw("    ")];
        spans.extend(inline(l, Style::new()));
        out.push(Line::from(spans));
    }
    out
}

/// One line of the answer: `⏺ ` on the first line of a block, two spaces after.
pub fn answer_line(l: &str, first: bool) -> Line<'static> {
    let l = if l.trim_start().starts_with("```") {
        ""
    } else {
        l
    };
    let mut spans = vec![Span::styled(
        if first { "⏺ " } else { "  " },
        Style::new().fg(Color::White),
    )];
    let base = if l.starts_with('#') {
        Style::new().bold()
    } else {
        Style::new()
    };
    let l = l.trim_start_matches('#').trim_start();
    spans.extend(inline(l, base));
    Line::from(spans)
}

pub fn note_line(text: &str) -> Vec<Line<'static>> {
    vec![Line::from(Span::styled(
        text.to_string(),
        Style::new().dim(),
    ))]
}

pub fn error_line(text: &str) -> Vec<Line<'static>> {
    vec![Line::from(Span::styled(
        text.to_string(),
        Style::new().fg(Color::Red),
    ))]
}

/// `**bold**` and `` `code` `` inside one line.
// Rust: returns owned `Span<'static>`s by copying the pieces out of `l`, so the
// lines can outlive the string they came from.
pub fn inline(l: &str, base: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut buf = String::new();
    let mut bold = false;
    let mut code = false;
    let mut chars = l.chars().peekable();
    let flush = |buf: &mut String, spans: &mut Vec<Span<'static>>, bold: bool, code: bool| {
        if buf.is_empty() {
            return;
        }
        let mut style = base;
        if bold {
            style = style.add_modifier(Modifier::BOLD);
        }
        if code {
            style = style.fg(Color::Cyan);
        }
        spans.push(Span::styled(std::mem::take(buf), style));
    };
    while let Some(c) = chars.next() {
        if c == '`' {
            flush(&mut buf, &mut spans, bold, code);
            code = !code;
        } else if c == '*' && !code && chars.peek() == Some(&'*') {
            chars.next();
            flush(&mut buf, &mut spans, bold, code);
            bold = !bold;
        } else {
            buf.push(c);
        }
    }
    flush(&mut buf, &mut spans, bold, code);
    spans
}

/// `parts` joined with ` · `, as many as fit in `width` columns, in order:
/// what does not fit is left off whole rather than cut mid-word.
pub fn fit(parts: &[String], width: usize) -> String {
    let mut out = String::new();
    for p in parts.iter().filter(|p| !p.is_empty()) {
        let next = if out.is_empty() {
            p.clone()
        } else {
            format!("{out} · {p}")
        };
        if unicode_width::UnicodeWidthStr::width(next.as_str()) > width {
            break;
        }
        out = next;
    }
    out
}

/// `12.3k` style token counts.
pub fn k(n: u32) -> String {
    if n >= 10_000 {
        format!("{:.0}k", n as f32 / 1000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f32 / 1000.0)
    } else {
        n.to_string()
    }
}

pub fn total(u: Usage) -> u32 {
    u.prompt_tokens + u.completion_tokens
}

/// `2m10s`, `48s`
pub fn clock(secs: u64) -> String {
    if secs >= 60 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

/// Green when the task is complete with nothing to look at, yellow when
/// there is something to look at, red when it is not complete.
fn report_color(r: &Report) -> Color {
    match (r.complete, r.warnings.is_empty()) {
        (false, _) => Color::Red,
        (true, true) => Color::Green,
        (true, false) => Color::Yellow,
    }
}

/// The card's gutter, for its wrapped lines.
pub fn report_gutter(r: &Report) -> Vec<Span<'static>> {
    vec![Span::styled("  │ ", Style::new().fg(report_color(r)))]
}

/// The result card a task ends with: the verdict, each `Done when` line
/// with the test that covers it and how it did without the change, the
/// checks and the review, what to look at, and what is still open.
pub fn report_lines(r: &Report) -> Vec<Line<'static>> {
    let color = report_color(r);
    let gutter = || Span::styled("  │ ", Style::new().fg(color));
    let row = |spans: Vec<Span<'static>>| {
        let mut l = vec![gutter()];
        l.extend(spans);
        Line::from(l)
    };
    let dim = Style::new().dim();
    let mark = if r.complete { "✓" } else { "✗" };
    let mut out = vec![Line::from(vec![
        Span::styled("  ╭ ", Style::new().fg(color)),
        Span::styled(
            format!("Result · {} {mark}", r.headline()),
            Style::new().fg(color).bold(),
        ),
    ])];
    if let Some(asked) = &r.asked {
        let first = asked.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
        out.push(row(vec![Span::styled(clip(first, 200), dim)]));
    }

    if !r.done_when.is_empty() {
        out.push(row(vec![Span::styled("Done when", Style::new().bold())]));
        let reviewed = r.done_when.iter().any(|c| c.test.is_some())
            || r.problems.iter().any(|p| p.starts_with("no test for"));
        for (i, c) in r.done_when.iter().enumerate() {
            let (m, style) = match (&c.test, c.without) {
                (None, _) if reviewed => ("✗", Style::new().fg(Color::Red)),
                (None, _) => ("·", dim),
                (Some(_), Some(Without::Fails | Without::FileFails)) => {
                    ("✓", Style::new().fg(Color::Green))
                }
                (Some(_), Some(Without::Passes)) => ("⚠", Style::new().fg(Color::Yellow)),
                (Some(_), _) => ("✓", Style::new().fg(Color::Green)),
            };
            let mut spans = vec![
                Span::raw("  "),
                Span::styled(format!("{m} "), style),
                Span::raw(format!("{}. ", i + 1)),
            ];
            spans.extend(inline(&clip(&c.text, 200), Style::new()));
            out.push(row(spans));
            let under = match (&c.test, c.without) {
                (None, _) if reviewed => "no test found".to_string(),
                (None, _) => "not reviewed".to_string(),
                (Some(t), Some(w)) if w != Without::Unknown => format!("{t} · {}", w.words()),
                (Some(t), _) => t.clone(),
            };
            let mut spans = vec![Span::raw("       ")];
            spans.extend(inline(&clip(&under, 200), dim));
            out.push(row(spans));
        }
    }

    let (done, total) = r.steps;
    let mut facts = Vec::new();
    if total > 0 {
        facts.push(format!("steps {done}/{total}"));
    }
    facts.push(match r.checks {
        Some(true) => "checks passed".to_string(),
        Some(false) => "checks FAILED".to_string(),
        None => "no checks run".to_string(),
    });
    facts.push(format!("review {}", r.review));
    facts.push(clock(r.seconds));
    out.push(row(vec![Span::styled(facts.join(" · "), dim)]));
    if let Some(note) = &r.without_note {
        out.push(row(vec![Span::styled(
            format!("new tests not run without the change: {note}"),
            dim,
        )]));
    }

    if !r.problems.is_empty() {
        out.push(row(vec![Span::styled("Not complete", Style::new().bold())]));
        for p in &r.problems {
            out.push(row(vec![
                Span::raw("  "),
                Span::styled("✗ ", Style::new().fg(Color::Red)),
                Span::raw(p.clone()),
            ]));
        }
    }
    // What the Done-when lines above do not already say.
    let looks: Vec<&String> = r
        .warnings
        .iter()
        .filter(|w| !w.starts_with("Done when"))
        .collect();
    if !looks.is_empty() {
        out.push(row(vec![Span::styled("Look at", Style::new().bold())]));
        for w in looks {
            out.push(row(vec![
                Span::raw("  "),
                Span::styled("⚠ ", Style::new().fg(Color::Yellow)),
                Span::raw(w.clone()),
            ]));
            if w.contains("existing test line") {
                for c in r.changed.iter().take(5) {
                    let was = match &c.after {
                        Some(after) => format!("{} → {}", clip(&c.before, 70), clip(after, 70)),
                        None => format!("taken out: {}", clip(&c.before, 120)),
                    };
                    out.push(row(vec![Span::styled(
                        format!("     {}:{} {was}", c.file, c.line),
                        dim,
                    )]));
                }
                if r.changed.len() > 5 {
                    out.push(row(vec![Span::styled(
                        format!("     (+{} more)", r.changed.len() - 5),
                        dim,
                    )]));
                }
            }
        }
    }
    if !r.open.is_empty() {
        out.push(row(vec![
            Span::styled("Still open", Style::new().bold()),
            Span::styled(" · say \"continue\" to carry on", dim),
        ]));
        for t in &r.open {
            out.push(row(vec![Span::raw(format!("  ☐ {}", clip(t, 200)))]));
        }
    }
    if !r.files.is_empty() {
        out.push(row(vec![Span::styled(
            format!("files {}", r.files.join(", ")),
            dim,
        )]));
    }
    out.push(Line::from(Span::styled("  ╰", Style::new().fg(color))));
    out
}

/// `/tasks`: each finished task, how it ended, and how long it took.
pub fn task_list(reports: &[Report]) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for (i, r) in reports.iter().enumerate() {
        let color = report_color(r);
        out.push(Line::from(vec![
            Span::styled(format!("  {:>2}. ", i + 1), Style::new().dim()),
            Span::styled(
                format!("{} ", if r.complete { "✓" } else { "✗" }),
                Style::new().fg(color),
            ),
            Span::styled(r.summary(), Style::new().fg(color)),
            Span::styled(format!(" · {}", clock(r.seconds)), Style::new().dim()),
        ]));
        out.push(Line::from(Span::styled(
            format!("      > {}", clip(r.task.lines().next().unwrap_or(""), 100)),
            Style::new().dim(),
        )));
    }
    out.push(Line::from(Span::styled(
        "  /report <number> shows one again",
        Style::new().dim(),
    )));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::proof::Changed;
    use crate::agent::report::Covered;

    fn text(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_result_card_shows_each_done_when_line_and_what_to_look_at() {
        let r = Report {
            task: "weeks start Monday".into(),
            asked: Some("Weeks start on Monday.".into()),
            planned: true,
            complete: true,
            warnings: vec![
                "Done when 2: its test passes without the change".into(),
                "1 existing test line changed".into(),
            ],
            done_when: vec![
                Covered {
                    text: "The week shows Mon–Sun.".into(),
                    test: Some("`src/App.test.tsx:1910` \"shows the week\"".into()),
                    without: Some(Without::Fails),
                },
                Covered {
                    text: "Water counts from Monday.".into(),
                    test: Some("`src/App.test.tsx:2081`".into()),
                    without: Some(Without::Passes),
                },
            ],
            steps: (5, 5),
            checks: Some(true),
            review: "approved after 1 fix round".into(),
            changed: vec![Changed {
                file: "server/store.test.ts".into(),
                line: 1009,
                before: "addDays(week, 6)".into(),
                after: Some("addDays(week, 5)".into()),
            }],
            files: vec!["src/lib/nutrition.ts".into()],
            seconds: 3546,
            ..Default::default()
        };
        let shown = text(&report_lines(&r));
        for want in [
            "╭ Result · plan complete · 2 things to look at ✓",
            "│ Weeks start on Monday.",
            "│   ✓ 1. The week shows Mon–Sun.",
            "src/App.test.tsx:1910 \"shows the week\" · fails without the change",
            "│   ⚠ 2. Water counts from Monday.",
            "src/App.test.tsx:2081 · passes without the change",
            "steps 5/5 · checks passed · review approved after 1 fix round · 59m06s",
            "⚠ 1 existing test line changed",
            "server/store.test.ts:1009 addDays(week, 6) → addDays(week, 5)",
            "╰",
        ] {
            assert!(shown.contains(want), "{want:?} missing from\n{shown}");
        }
        assert!(!shown.contains("Not complete") && !shown.contains("Still open"));
        assert_eq!(report_lines(&r)[1].spans[0].style.fg, Some(Color::Yellow));
    }

    #[test]
    fn inline_marks_bold_and_code() {
        let spans = inline("see `a.rs` and **this**", Style::new());
        let texts: Vec<_> = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(texts, ["see ", "a.rs", " and ", "this"]);
        assert_eq!(spans[1].style.fg, Some(Color::Cyan));
        assert!(spans[3].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn the_checklist_marks_each_step() {
        let items = [
            Todo {
                text: "Add the op".into(),
                status: Status::Done,
            },
            Todo {
                text: "Teach the coach".into(),
                status: Status::InProgress,
            },
            Todo::pending("Run the tests"),
        ];
        let lines = todo_lines(&items);
        let text: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(
            text,
            [
                "⏺ Todos 1 of 3 done",
                "  ⎿ ☒ Add the op",
                "    ◼ Teach the coach",
                "    ☐ Run the tests"
            ]
        );
        assert!(
            lines[1].spans[2]
                .style
                .add_modifier
                .contains(Modifier::CROSSED_OUT)
        );
        assert!(
            lines[2].spans[2]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
    }

    #[test]
    fn a_plan_shows_the_persons_part_and_folds_the_workers() {
        let plan = "## What you asked for\nAdd Pilates to the plan.\n\n## Choices to confirm\n- ⚠ Logging it on the Workouts card, not adding a session.\n\n## Steps\n1. `a.rs`: x\n2. `b.rs`: y\n\n## Verify\ncargo test";
        let lines = plan_view(plan);
        let text: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(
            !text.iter().any(|t| t.contains("a.rs")),
            "the worker's steps are folded: {text:?}"
        );
        assert_eq!(
            text.last().unwrap(),
            "  │ ── 2 steps for the worker, with its checks and pitfalls · /steps shows them"
        );
        let warn = lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.contains('⚠')))
            .unwrap();
        assert_eq!(warn.spans[1].style.fg, Some(Color::Yellow));
        // A plan in the old shape shows whole, with nothing folded.
        assert_eq!(plan_view("## Plan\n1. x").len(), 2);
    }

    #[test]
    fn an_approval_keeps_the_reviewers_lines_and_draws_its_bold() {
        let lines = review_lines(
            true,
            false,
            "Looks right.\n**Checks:** all pass.\n\n**Done when:** covered.",
        );
        let text: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(
            text,
            [
                "  ✔ approved Looks right.",
                "    Checks: all pass.",
                "    Done when: covered."
            ]
        );
        assert!(
            lines[1].spans[1]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
    }

    #[test]
    fn fit_leaves_off_whole_pieces() {
        let parts = [
            "qwen".to_string(),
            "working".into(),
            "think off".into(),
            "".into(),
        ];
        assert_eq!(fit(&parts, 80), "qwen · working · think off");
        assert_eq!(fit(&parts, 20), "qwen · working");
        assert_eq!(fit(&parts, 3), "");
    }

    #[test]
    fn counts_and_clocks() {
        assert_eq!(k(950), "950");
        assert_eq!(k(1234), "1.2k");
        assert_eq!(k(22785), "23k");
        assert_eq!(clock(130), "2m10s");
        assert_eq!(clock(48), "48s");
    }

    #[test]
    fn result_line_summarises() {
        let l = result_line(true, "a\nb\nc", "bash");
        assert_eq!(l[0].spans[1].content, "a (+2 lines)");
        let l = result_line(
            true,
            "1\tfn a() {}\n2\t}\n[lines 3-9 follow]\n",
            "read_file",
        );
        assert_eq!(l[0].spans[1].content, "Read 2 lines");
    }

    #[test]
    fn a_failure_is_summed_up_by_the_line_that_names_it() {
        let checks = "Found 0 warnings and 0 errors.\n FAIL  src/a.test.tsx > month\n Test Files  1 failed | 21 passed (22)\n      Tests  1 failed | 631 passed (632)\n   Duration  9s";
        let l = result_line(false, checks, "checks");
        assert_eq!(
            l[0].spans[1].content,
            "Tests  1 failed | 631 passed (632) (+4 lines)"
        );
        // Passing, or a failure that names nothing: the first line, as before.
        let l = result_line(true, checks, "checks");
        assert!(l[0].spans[1].content.starts_with("Found 0 warnings"));
        let l = result_line(false, "exit 2\nno match", "bash");
        assert_eq!(l[0].spans[1].content, "exit 2 (+1 lines)");
    }

    #[test]
    fn diff_lines_carry_number_sign_and_colour() {
        let rows = [
            Row {
                line: 12,
                kind: Kind::Removed,
                text: "\tlet a = 1;".into(),
            },
            Row {
                line: 12,
                kind: Kind::Added,
                text: "let a = 2;".into(),
            },
        ];
        let lines = diff_lines(&rows, 3);
        let text: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(
            text,
            [
                "      12 -     let a = 1;",
                "      12 + let a = 2;",
                "           … 3 more lines"
            ]
        );
        assert_eq!(lines[0].spans[2].style.fg, Some(Color::Red));
        assert_eq!(lines[1].spans[2].style.fg, Some(Color::Green));
    }
}

/// Wrap one styled line to `width` columns with a hanging indent: the first
/// visual line is kept as is, each continuation starts with `cont`. Breaks at
/// the last space that fits, or mid-word when a word is wider than the line.
pub fn wrap_hanging(
    line: &Line<'static>,
    width: usize,
    cont: &[Span<'static>],
) -> Vec<Line<'static>> {
    let width = width.max(8);
    let cont_width: usize = cont
        .iter()
        .map(|s| {
            s.content
                .chars()
                .map(|c| c.width().unwrap_or(0))
                .sum::<usize>()
        })
        .sum();
    // Flatten to (char, style) so breaks can fall anywhere.
    let cells: Vec<(char, Style)> = line
        .spans
        .iter()
        .flat_map(|s| s.content.chars().map(move |c| (c, s.style)))
        .collect();
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut start = 0;
    let mut first = true;
    while start < cells.len() || first {
        let avail = if first {
            width
        } else {
            width.saturating_sub(cont_width).max(4)
        };
        // Walk until the width is used up, remembering the last space.
        let mut used = 0;
        let mut end = start;
        let mut last_space = None;
        while end < cells.len() {
            let w = cells[end].0.width().unwrap_or(0);
            if used + w > avail {
                break;
            }
            if cells[end].0 == ' ' {
                last_space = Some(end);
            }
            used += w;
            end += 1;
        }
        let mut cut = end;
        if end < cells.len()
            && let Some(sp) = last_space
            && sp > start
        {
            cut = sp;
        }
        let mut spans: Vec<Span<'static>> = if first { Vec::new() } else { cont.to_vec() };
        let mut buf = String::new();
        let mut style: Option<Style> = None;
        for &(c, st) in &cells[start..cut] {
            if style != Some(st) {
                if !buf.is_empty() {
                    spans.push(Span::styled(
                        std::mem::take(&mut buf),
                        style.unwrap_or_default(),
                    ));
                }
                style = Some(st);
            }
            buf.push(c);
        }
        if !buf.is_empty() {
            spans.push(Span::styled(buf, style.unwrap_or_default()));
        }
        out.push(Line::from(spans));
        // Skip the space the line broke on. A cut that made no progress (one
        // cell wider than the line) still moves on, so this never loops forever.
        let next = if cut < cells.len() && cells[cut].0 == ' ' {
            cut + 1
        } else {
            cut
        };
        start = if next > start { next } else { start + 1 };
        first = false;
    }
    out
}

#[cfg(test)]
mod wrap_tests {
    use super::*;

    fn text(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn continuations_carry_the_gutter() {
        let line = Line::from(vec![
            Span::styled("  │ ", Style::new()),
            Span::raw("one two three four five six"),
        ]);
        let cont = vec![Span::styled("  │ ", Style::new())];
        assert_eq!(
            text(&wrap_hanging(&line, 16, &cont)),
            ["  │ one two", "  │ three four", "  │ five six"]
        );
    }

    #[test]
    fn long_words_break_and_short_lines_pass_through() {
        let line = Line::from("abcdefghijklmnop");
        assert_eq!(
            text(&wrap_hanging(&line, 8, &[Span::raw("  ")])),
            ["abcdefgh", "  ijklmn", "  op"]
        );
        let line = Line::from("short");
        assert_eq!(text(&wrap_hanging(&line, 80, &[])), ["short"]);
        let line = Line::from("");
        assert_eq!(text(&wrap_hanging(&line, 80, &[])), [""]);
    }
}
