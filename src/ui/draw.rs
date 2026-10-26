//! Drawing the live area: the streaming tail, the status line, the input box,
//! the footer and the slash menu.

use super::*;

impl App {
    // ---- Drawing ----------------------------------------------------------------

    pub(super) fn render(&self, frame: &mut ratatui::Frame) {
        let area = frame.area();
        if self.menu_open() {
            self.render_menu(frame, area);
            return;
        }
        let [tail, status, input, footer] = Layout::vertical([
            Constraint::Length(TAIL_ROWS),
            Constraint::Length(1),
            Constraint::Length(BOX_ROWS),
            Constraint::Length(1),
        ])
        .areas(area);

        // The last few lines of the answer as it streams.
        if let Some(r) = &self.running
            && !r.text.is_empty()
        {
            let para = Paragraph::new(Text::from(vec![view::answer_line(&r.text, !r.answer_open)]))
                .wrap(Wrap { trim: false });
            let count = para.line_count(tail.width) as u16;
            frame.render_widget(para.scroll((count.saturating_sub(tail.height), 0)), tail);
        }

        frame.render_widget(Paragraph::new(self.status_line()), status);

        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().dim());
        let inner = block.inner(input);
        frame.render_widget(block, input);
        let shown = if self.input.is_empty() {
            Text::from(Line::from(vec![
                Span::styled("> ", Style::new().bold()),
                Span::styled("What should weft do?", Style::new().dim()),
            ]))
        } else {
            let mut lines = Vec::new();
            for (i, l) in self.input.split('\n').enumerate() {
                lines.push(Line::from(vec![
                    Span::styled(if i == 0 { "> " } else { "  " }, Style::new().bold()),
                    Span::raw(l.to_string()),
                ]));
            }
            Text::from(lines)
        };
        let (row, col) = self.input.cursor_at(inner.width);
        let scroll = row.saturating_sub(inner.height.saturating_sub(1));
        frame.render_widget(
            Paragraph::new(shown)
                .wrap(Wrap { trim: false })
                .scroll((scroll, 0)),
            inner,
        );
        frame.set_cursor_position((inner.x + col, inner.y + row - scroll));

        frame.render_widget(Paragraph::new(self.footer_line()), footer);
    }

    /// While a command is being typed, the live area shows the line and the matching commands.
    pub(super) fn render_menu(&self, frame: &mut ratatui::Frame, area: Rect) {
        let items = self.menu_items();
        let mut lines = vec![Line::from(vec![
            Span::styled("> ", Style::new().bold()),
            Span::raw(self.input.to_string()),
        ])];
        for (i, item) in items.iter().enumerate() {
            let style = if i == self.menu_at.min(items.len() - 1) {
                Style::new().bold().fg(Color::Cyan)
            } else {
                Style::new()
            };
            lines.push(Line::from(vec![
                Span::styled(format!("  {:<10}", item.name), style),
                Span::styled(item.what.clone(), Style::new().dim()),
            ]));
        }
        frame.render_widget(Paragraph::new(Text::from(lines)), area);
        frame.set_cursor_position((area.x + 2 + self.input.width() as u16, area.y));
    }

    pub(super) fn status_line(&self) -> Line<'static> {
        if self.plan_ask.is_some() {
            return Line::from(vec![
                Span::styled("? ", Style::new().fg(Color::Yellow).bold()),
                Span::styled("Follow this plan?  ", Style::new().fg(Color::Yellow)),
                Span::styled(
                    "enter to go · type notes + enter to revise · esc to stop",
                    Style::new().dim(),
                ),
            ]);
        }
        if let Some((what, _)) = &self.ask {
            return Line::from(vec![
                Span::styled("? ", Style::new().fg(Color::Yellow).bold()),
                Span::styled(
                    format!("Allow weft to {what}?  "),
                    Style::new().fg(Color::Yellow),
                ),
                Span::styled("(y) once  (a) always  (n) no", Style::new().dim()),
            ]);
        }
        if let Some(r) = &self.running {
            let verb = match r.phase {
                "planning" => "Planning",
                "working" => "Working",
                "reviewing" => "Reviewing",
                "fixing" => "Fixing",
                "compacting" => "Compacting",
                _ => "Thinking about it",
            };
            let mut s = format!("{verb}… {}", view::clock(r.began.elapsed().as_secs()));
            if let Some((Role::Planner, n)) = r.turn
                && n > 0
            {
                s.push_str(&format!(" · ~{} thinking tokens", view::k(n as u32)));
            }
            return Line::from(vec![
                Span::styled(
                    format!("{} ", SPINNER[self.frame % SPINNER.len()]),
                    Style::new().fg(Color::Magenta),
                ),
                Span::styled(s, Style::new().fg(Color::Magenta)),
                Span::styled("  esc to interrupt", Style::new().dim()),
            ]);
        }
        if self.quit_armed {
            return Line::from(Span::styled(
                "press ctrl-c again to leave",
                Style::new().dim(),
            ));
        }
        Line::from("")
    }

    pub(super) fn footer_line(&self) -> Line<'static> {
        let pct = (self.context as f32 / self.session.window.max(1) as f32 * 100.0).round() as u32;
        // Name the model that has the floor: the planner while it plans or
        // reviews, the worker otherwise.
        let (model, doing) = match &self.running {
            Some(Running {
                turn: Some((Role::Planner, _)),
                phase,
                ..
            }) => (self.session.planner_name(), *phase),
            Some(Running {
                turn: Some((Role::Scout, _)),
                phase,
                ..
            }) => (
                self.session.scout_name(),
                if *phase == "compacting" {
                    "compacting"
                } else {
                    "scouting"
                },
            ),
            Some(r) => (self.session.worker_name(), r.phase),
            None => (self.session.worker_name(), "idle"),
        };
        Line::from(Span::styled(
            format!(
                "  {model} · {doing} · think {} · {} · context {pct}% · {} in conversation · / for commands · shift-tab",
                if self.think { "on" } else { "off" },
                self.mode.label(),
                self.session.depth()
            ),
            Style::new().dim(),
        ))
    }
}
