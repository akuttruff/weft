//! Turning transcript pieces into styled lines: tool calls, plans, reviews,
//! and the answer text with a little markdown.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

use super::diff::{Kind, Row};
use crate::lm::Usage;
use crate::mcp::Status;

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

/// `✻ planning`
pub fn phase_line(name: &str) -> Vec<Line<'static>> {
    vec![Line::from(vec![
        Span::styled("✻ ", Style::new().fg(Color::Magenta)),
        Span::styled(name.to_string(), Style::new().bold()),
    ])]
}

/// `⏺ Read src/cli.js`
pub fn tool_line(summary: &str) -> Vec<Line<'static>> {
    vec![Line::from(vec![
        Span::styled("⏺ ", Style::new().fg(Color::Green)),
        Span::raw(summary.to_string()),
    ])]
}

/// `  ⎿ first line of the result (+n lines)`, or `Read 18 lines` for a file.
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
        let first: String = output
            .lines()
            .next()
            .unwrap_or("(no output)")
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
pub fn mcp_summary(servers: &[Status]) -> Vec<Line<'static>> {
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
pub fn mcp_lines(servers: &[Status]) -> Vec<Line<'static>> {
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
        spans.extend(inline(
            l,
            if l.starts_with("## ") {
                Style::new().bold()
            } else {
                Style::new()
            },
        ));
        out.push(Line::from(spans));
    }
    out
}

pub fn review_lines(approved: bool, text: &str) -> Vec<Line<'static>> {
    if approved {
        return vec![Line::from(vec![
            Span::styled("  ✔ approved ", Style::new().fg(Color::Green)),
            Span::styled(text.to_string(), Style::new().dim()),
        ])];
    }
    let mut out = vec![Line::from(Span::styled(
        "  ✗ fixes requested",
        Style::new().fg(Color::Yellow),
    ))];
    for l in text.lines() {
        out.push(Line::from(format!("    {l}")));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_marks_bold_and_code() {
        let spans = inline("see `a.rs` and **this**", Style::new());
        let texts: Vec<_> = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(texts, ["see ", "a.rs", " and ", "this"]);
        assert_eq!(spans[1].style.fg, Some(Color::Cyan));
        assert!(spans[3].style.add_modifier.contains(Modifier::BOLD));
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
