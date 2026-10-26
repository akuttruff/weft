//! What the screen does with each message from the agent: transcript lines,
//! status, the approval and plan prompts.

use super::*;

impl App {
    // ---- Messages from the agent ----------------------------------------------

    pub(super) fn on_msg(&mut self, msg: UiMsg) {
        match msg {
            UiMsg::Ask { what, reply } => self.ask = Some((what, reply)),
            UiMsg::AskPlan { reply } => {
                self.flush_text();
                self.plan_ask = Some(reply);
            }
            UiMsg::Done(result) => self.on_done(result),
            UiMsg::Compacted(result) => {
                self.running = None;
                match result {
                    Ok(done) => self.on_agent(done.event()),
                    Err(WorkerError::Cancelled) => self.push(view::error_line("  ⎿ Interrupted")),
                    Err(e) => self.push(view::error_line(&format!("  ⎿ could not compact: {e:#}"))),
                }
            }
            UiMsg::Agent(evt) => self.on_agent(evt),
        }
    }

    pub(super) fn on_agent(&mut self, evt: AgentEvent) {
        match evt {
            AgentEvent::Phase { name } => {
                self.flush_text();
                let name: &'static str = match name.as_str() {
                    "planning" => "planning",
                    "working" => "working",
                    "reviewing" => "reviewing",
                    "fixing" => "fixing",
                    "compacting" => "compacting",
                    _ => "working",
                };
                if let Some(r) = &mut self.running {
                    r.phase = name;
                }
                self.push(view::phase_line(name));
            }
            AgentEvent::TurnStart { role } => {
                if let Some(r) = &mut self.running {
                    r.turn = Some((role, 0));
                }
            }
            AgentEvent::Reasoning { text, .. } => {
                if let Some(Running {
                    turn: Some((_, n)), ..
                }) = &mut self.running
                {
                    *n += text.len() / 4;
                }
            }
            AgentEvent::Text {
                role: Role::Worker,
                text,
            } => {
                if let Some(r) = &mut self.running {
                    r.text.push_str(&text);
                    // Whole lines go straight to the transcript; the partial last line stays live.
                    while let Some(at) = r.text.find('\n') {
                        let line: String = r.text.drain(..=at).collect();
                        let line = line.trim_end_matches('\n');
                        if line.trim().is_empty() && !r.answer_open {
                            continue;
                        }
                        let first = !r.answer_open;
                        r.answer_open = true;
                        self.out
                            .push((vec![view::answer_line(line, first)], vec![Span::raw("  ")]));
                    }
                }
            }
            AgentEvent::Text { .. } => {}
            AgentEvent::ToolStart {
                summary,
                name,
                args,
                ..
            } => {
                self.last_tool = (name, args);
                self.flush_text();
                self.push(view::tool_line(&summary));
            }
            AgentEvent::ToolDone { ok, output, .. } => {
                let (tool, args) = &self.last_tool;
                let line = view::result_line(ok, &output, tool);
                // An edit or a new file is shown as the code it changed.
                let shown = if ok {
                    diff::rows(tool, args, &output)
                } else {
                    None
                };
                self.push_with(line, vec![Span::raw("    ")]);
                if let Some((rows, hidden)) = shown {
                    self.push_with(
                        view::diff_lines(&rows, hidden),
                        vec![Span::raw(view::DIFF_INDENT)],
                    );
                }
            }
            AgentEvent::Plan { text } => self.push_with(
                view::plan_lines(&text),
                vec![Span::styled("  │ ", Style::new().fg(Color::Cyan))],
            ),
            AgentEvent::Review { approved, text } => self.push(view::review_lines(approved, &text)),
            AgentEvent::Hook { ok, text } => {
                self.flush_text();
                if text.is_empty() {
                    return;
                }
                if ok {
                    self.push(view::note_line(&format!("  {text}")));
                } else {
                    self.push(view::error_line(&format!("  {text}")));
                }
            }
            AgentEvent::Trimmed { dropped, tokens } => {
                self.flush_text();
                self.push(view::note_line(&format!(
                    "  dropped {dropped} earlier tool result{} to make room · context {}",
                    if dropped == 1 { "" } else { "s" },
                    view::k(tokens),
                )));
            }
            AgentEvent::Compacted {
                before,
                after,
                tasks,
            } => {
                self.context = after;
                self.push(view::note_line(&format!(
                    "  compacted: {tasks} earlier task{} folded into a note · context {} → ~{} tokens",
                    if tasks == 1 { "" } else { "s" },
                    view::k(before),
                    view::k(after),
                )));
            }
            AgentEvent::TurnEnd { role, usage, .. } => {
                match role {
                    Role::Planner => self.planner_usage = add(self.planner_usage, usage),
                    Role::Scout => self.scout_usage = add(self.scout_usage, usage),
                    Role::Worker => {
                        self.worker_usage = add(self.worker_usage, usage);
                        self.context = usage.prompt_tokens + usage.completion_tokens;
                    }
                }
                if role == Role::Worker {
                    self.flush_text();
                }
                if let Some(r) = &mut self.running {
                    r.turn = None;
                }
            }
        }
    }

    pub(super) fn on_done(&mut self, result: Result<TandemOutcome, WorkerError>) {
        self.flush_text();
        let began = self.running.take().map(|r| r.began);
        let secs = began.map(|b| b.elapsed().as_secs()).unwrap_or(0);
        self.ask = None;
        match result {
            Ok(out) => {
                let review = match (out.reviews.last(), out.fixes_pending) {
                    (None, _) => "",
                    (Some(_), true) => " · fixes still pending",
                    (Some((true, _)), false) => " · reviewed",
                    (Some((false, _)), false) => " · not approved",
                };
                let touched = if out.touched.is_empty() {
                    String::new()
                } else {
                    format!(" · {}", out.touched.join(", "))
                };
                self.push(view::note_line(&format!(
                    "  {} · planner {} · worker {} tokens{touched}{review}",
                    view::clock(secs),
                    view::k(view::total(out.planner_usage)),
                    view::k(view::total(out.worker_usage)),
                )));
            }
            Err(WorkerError::Cancelled) => self.push(view::error_line("  ⎿ Interrupted")),
            Err(e) => self.push(view::error_line(&format!("  ⎿ {e:#}"))),
        }
    }
}
