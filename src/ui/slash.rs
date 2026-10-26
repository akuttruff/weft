//! What happens when enter is pressed: slash commands, and handing a task
//! or a compaction to the session on its own tokio task.

use super::*;

impl App {
    // ---- Submitting -------------------------------------------------------------

    pub(super) fn submit(&mut self, tx: &mpsc::Sender<UiMsg>) {
        let line = self.input.trim().to_string();
        if line.is_empty() {
            return;
        }
        self.input.clear();
        self.history_at = None;
        if self.history.last() != Some(&line) {
            self.history.push(line.clone());
        }
        // Right after a `/resume` listing, a bare number is a pick, not a task.
        let line = if self.picking_session && line.parse::<usize>().is_ok() {
            format!("/resume {line}")
        } else {
            line
        };
        self.picking_session = false;
        let (command, rest) = match line.split_once(char::is_whitespace) {
            Some((c, r)) if c.starts_with('/') => (c, r.trim().to_string()),
            _ => (line.as_str(), String::new()),
        };
        match command {
            "/quit" | "/exit" | "/q" => self.quit = true,
            "/restart" => {
                if self.running.is_some() {
                    self.push(view::note_line("a task is running; esc stops it"));
                } else {
                    self.restart = true;
                    self.quit = true;
                }
            }
            "/think" => {
                self.think = !self.think;
                self.push(view::note_line(&format!(
                    "thinking {}",
                    if self.think { "on" } else { "off" }
                )));
            }
            "/accept" => self.set_mode(if self.mode == Permission::Edits {
                Permission::Ask
            } else {
                Permission::Edits
            }),
            "/clear" => {
                self.session.clear();
                self.push(view::note_line("fresh conversation"));
            }
            "/compact" => self.compact(tx),
            "/undo" => {
                if self.running.is_some() {
                    self.push(view::note_line("a task is running; esc stops it"));
                } else {
                    match self.session.undo() {
                        Ok(Some(files)) => self.push(view::note_line(&format!(
                            "put back {}",
                            files.join(", ")
                        ))),
                        Ok(None) => self.push(view::note_line("nothing to undo")),
                        Err(e) => self.push(view::error_line(&format!("  undo failed: {e:#}"))),
                    }
                }
            }
            "/resume" if rest.is_empty() => {
                let me = self.session.transcript_id();
                match self.sessions_dir().and_then(|d| transcript::list(&d)) {
                    Ok(all) => {
                        self.sessions = all.into_iter().filter(|s| Some(&s.id) != me.as_ref()).collect();
                        let lines = view::session_lines(&self.sessions);
                        self.push(lines);
                        self.picking_session = !self.sessions.is_empty();
                    }
                    Err(e) => self.push(view::error_line(&format!("  {e:#}"))),
                }
            }
            "/resume" => {
                // A number picks from the last listing; anything else is an id.
                let picked = match rest.parse::<usize>() {
                    Ok(n) => self
                        .sessions
                        .get(n.wrapping_sub(1))
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("no session {n} in the list; /resume shows it")),
                    Err(_) => self.sessions_dir().and_then(|d| transcript::find(&d, &rest)),
                };
                match picked {
                    Ok(saved) => self.resume(saved),
                    Err(e) => self.push(view::error_line(&format!("  {e:#}"))),
                }
            }
            "/cost" => self.push(view::note_line(&format!(
                "{} tasks · planner {} tokens ({} thinking) · worker {} tokens · scout {} tokens · context {} of {}",
                self.tasks,
                view::k(view::total(self.planner_usage)),
                view::k(self.planner_usage.reasoning_tokens),
                view::k(view::total(self.worker_usage)),
                view::k(view::total(self.scout_usage)),
                view::k(self.context),
                view::k(self.session.window),
            ))),
            "/memory" => {
                let file = self.session.memory_file();
                let text = file
                    .as_ref()
                    .and_then(|f| std::fs::read_to_string(f).ok())
                    .unwrap_or_default();
                match file {
                    Some(f) if !text.trim().is_empty() => {
                        let mut lines = view::note_line(&format!("kept in {}", f.display()));
                        for l in text.lines() {
                            lines.push(Line::from(Span::raw(format!("  {l}"))));
                        }
                        self.push(lines);
                    }
                    Some(f) => self.push(view::note_line(&format!(
                        "nothing kept yet; `remember` will write to {}",
                        f.display()
                    ))),
                    None => self.push(view::note_line("no home folder, so nothing is kept")),
                }
            }
            "/mcp" => {
                if self.session.mcp.is_empty() {
                    self.push(view::note_line(&format!(
                        "no MCP servers; list them in {}",
                        crate::mcp::McpConfig::path()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|_| "~/.config/weft/mcp.json".into())
                    )));
                } else {
                    self.push(view::mcp_lines(&self.session.mcp));
                }
            }
            "/help" => {
                let mut lines = Vec::new();
                for (name, what, _) in COMMANDS {
                    lines.push(Line::from(vec![
                        Span::styled(format!("  {name:<10}"), Style::new().fg(Color::Cyan)),
                        Span::styled(what.to_string(), Style::new().dim()),
                    ]));
                }
                for c in commands::load(&self.session.ctx.root) {
                    lines.push(Line::from(vec![
                        Span::styled(format!("  /{:<9}", c.name), Style::new().fg(Color::Cyan)),
                        Span::styled(
                            format!("{} · {}", c.description, c.from.display()),
                            Style::new().dim(),
                        ),
                    ]));
                }
                lines.push(Line::from(Span::styled("  enter sends · shift-tab cycles ask / edits auto / all auto / plan first · ctrl-j or \\ + enter for a newline · Emacs-style editing keys as in your shell · ↑↓ history · esc stops a task · y/a/n answer approvals · ctrl-c twice leaves", Style::new().dim())));
                self.push(lines);
            }
            "/plan" | "/go" if rest.is_empty() => {
                self.push(view::note_line(&format!("usage: {command} <task>")))
            }
            "/plan" => self.start(rest, Mode::Plan, tx),
            "/go" => self.start(rest, Mode::Go, tx),
            c if c.starts_with('/') => {
                // A custom command: the file is the task, shown as what was typed.
                let custom = commands::load(&self.session.ctx.root)
                    .into_iter()
                    .find(|cmd| format!("/{}", cmd.name) == c);
                match custom {
                    Some(cmd) => {
                        let task = commands::expand(&cmd.body, &rest);
                        self.start_shown(line.clone(), task, cmd.mode, tx);
                    }
                    None => self.push(view::note_line(&format!(
                        "unknown command {c}; /help lists them"
                    ))),
                }
            }
            _ => {
                let mode = if self.mode == Permission::Plan {
                    Mode::Plan
                } else {
                    Mode::Auto
                };
                self.start(line, mode, tx)
            }
        }
    }

    /// Fold the conversation now, on its own tokio task like a task.
    pub(super) fn compact(&mut self, tx: &mpsc::Sender<UiMsg>) {
        if self.running.is_some() {
            self.push(view::note_line("a task is running; esc stops it"));
            return;
        }
        if self.session.depth() == 0 {
            self.push(view::note_line("nothing to compact yet"));
            return;
        }
        let cancel = CancellationToken::new();
        self.running = Some(Running {
            began: Instant::now(),
            phase: "compacting",
            turn: None,
            text: String::new(),
            answer_open: false,
            cancel: cancel.clone(),
        });
        let session = self.session.clone();
        let before = self.context;
        let tx = tx.clone();
        tokio::spawn(async move {
            let (events, mut agent_rx) = mpsc::channel::<AgentEvent>(64);
            let forward = {
                let tx = tx.clone();
                tokio::spawn(async move {
                    while let Some(evt) = agent_rx.recv().await {
                        if tx.send(UiMsg::Agent(evt)).await.is_err() {
                            break;
                        }
                    }
                })
            };
            let result = session.compact(events, cancel, before).await;
            let _ = forward.await;
            let _ = tx.send(UiMsg::Compacted(result)).await;
        });
    }

    pub(super) fn sessions_dir(&self) -> Result<std::path::PathBuf> {
        transcript::dir_for(&self.session.ctx.root)
    }

    /// Make an earlier session the current one and show what it holds.
    pub(super) fn resume(&mut self, saved: Saved) {
        if self.running.is_some() {
            self.push(view::note_line("a task is running; esc stops it"));
            return;
        }
        if let Err(e) = self.session.resume(&saved) {
            self.push(view::error_line(&format!("  could not resume: {e:#}")));
            return;
        }
        self.planner_usage = saved.planner_usage;
        self.worker_usage = saved.worker_usage;
        self.scout_usage = saved.scout_usage;
        self.context = saved.context;
        self.tasks = saved.tasks.len();
        self.push(view::replay_lines(&saved));
    }

    /// Switch the permission mode, keeping the session's edit approvals in step.
    pub(super) fn set_mode(&mut self, mode: Permission) {
        self.mode = mode;
        self.session
            .allow(&["edit_file", "write_file"], mode == Permission::Edits);
        self.push(view::note_line(&format!(
            "{} · {}",
            mode.label(),
            mode.note()
        )));
    }

    /// Hand a task to the session on its own tokio task; its events come back through `tx`.
    pub(super) fn start(&mut self, task: String, mode: Mode, tx: &mpsc::Sender<UiMsg>) {
        self.start_shown(task.clone(), task, mode, tx);
    }

    /// The same, showing `shown` in the transcript rather than the task text.
    pub(super) fn start_shown(
        &mut self,
        shown: String,
        task: String,
        mode: Mode,
        tx: &mpsc::Sender<UiMsg>,
    ) {
        if self.running.is_some() {
            self.push(view::note_line("a task is running; esc stops it"));
            return;
        }
        self.tasks += 1;
        self.push(view::user_line(&shown));
        let cancel = CancellationToken::new();
        self.running = Some(Running {
            began: Instant::now(),
            phase: "starting",
            turn: None,
            text: String::new(),
            answer_open: false,
            cancel: cancel.clone(),
        });

        let session = self.session.clone();
        let think = self.think;
        let yes = self.mode == Permission::All;
        let tx = tx.clone();
        tokio::spawn(async move {
            let (events, mut agent_rx) = mpsc::channel::<AgentEvent>(256);
            let forward = {
                let tx = tx.clone();
                tokio::spawn(async move {
                    while let Some(evt) = agent_rx.recv().await {
                        if tx.send(UiMsg::Agent(evt)).await.is_err() {
                            break;
                        }
                    }
                })
            };
            let approver: Arc<dyn Approver> = if yes {
                Arc::new(Yes)
            } else {
                Arc::new(UiApprover { tx: tx.clone() })
            };
            let result = session
                .run_task(&task, mode, think, approver, events, cancel)
                .await;
            let _ = forward.await;
            let _ = tx.send(UiMsg::Done(result)).await;
        });
    }
}
