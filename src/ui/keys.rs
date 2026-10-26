//! Keys: editing the input, answering prompts, walking the history, the slash menu.

use super::*;

impl App {
    // ---- Keys -----------------------------------------------------------------

    pub(super) fn on_terminal(&mut self, event: Event, tx: &mpsc::Sender<UiMsg>) {
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => self.on_key(key, tx),
            Event::Paste(text) => {
                self.input
                    .insert_str(&text.replace("\r\n", "\n").replace('\r', "\n"));
                self.history_at = None;
            }
            _ => {}
        }
    }

    pub(super) fn on_key(&mut self, key: KeyEvent, tx: &mpsc::Sender<UiMsg>) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        // A plan waiting for a word: enter goes, esc stops, typed notes + enter
        // send it back. Everything else edits the notes as usual.
        if self.plan_ask.is_some() {
            let alt = key.modifiers.contains(KeyModifiers::ALT);
            match (key.code, ctrl, alt) {
                (KeyCode::Enter, false, false) => {
                    let notes = self.input.trim().to_string();
                    self.input.clear();
                    self.answer_plan(if notes.is_empty() {
                        PlanDecision::Go
                    } else {
                        PlanDecision::Revise(notes)
                    });
                    return;
                }
                (KeyCode::Esc, _, _) | (KeyCode::Char('c'), true, _) => {
                    self.input.clear();
                    self.answer_plan(PlanDecision::Stop);
                    return;
                }
                _ => {}
            }
        }

        // An approval question takes the keyboard until it is answered.
        if let Some((_, _)) = &self.ask {
            let decision = match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => Some(Decision::Once),
                KeyCode::Char('a') | KeyCode::Char('A') => Some(Decision::Always),
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Some(Decision::No),
                KeyCode::Char('c') if ctrl => Some(Decision::No),
                _ => None,
            };
            if let Some(d) = decision
                && let Some((what, reply)) = self.ask.take()
            {
                let word = match d {
                    Decision::Once => "yes",
                    Decision::Always => "always",
                    Decision::No => "no",
                };
                self.push(view::note_line(&format!("  {what}? {word}")));
                let _ = reply.send(d);
                if d == Decision::No && key.code == KeyCode::Char('c') && ctrl {
                    self.stop();
                }
            }
            return;
        }

        if key.code != KeyCode::Char('c') || !ctrl {
            self.quit_armed = false;
        }
        match (key.code, ctrl) {
            (KeyCode::Char('c'), true) => {
                if self.running.is_some() {
                    self.stop();
                } else if !self.input.is_empty() {
                    self.input.clear();
                } else if self.quit_armed {
                    self.quit = true;
                } else {
                    self.quit_armed = true;
                }
            }
            (KeyCode::Char('d'), true) if self.input.is_empty() => self.quit = true,
            (KeyCode::Esc, _) => {
                if self.running.is_some() {
                    self.stop();
                } else {
                    self.input.clear();
                    self.history_at = None;
                }
            }
            (KeyCode::Enter, _) | (KeyCode::Char('j'), true)
                if ctrl || key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.input.insert('\n');
            }
            (KeyCode::Enter, _) => {
                if let Some(stripped) = self.input.strip_suffix('\\') {
                    self.input.set(format!("{stripped}\n"));
                } else if self.menu_open() {
                    self.complete_menu(tx);
                } else {
                    self.submit(tx);
                }
            }
            (KeyCode::Tab, _) if self.menu_open() => self.complete_menu(tx),
            (KeyCode::Backspace, _) => {
                self.input.backspace();
                self.history_at = None;
            }
            (KeyCode::Char('u'), true) => self.input.clear(),
            (KeyCode::Char('w'), true) => self.input.delete_word(),
            (KeyCode::Left, _) => self.input.left(),
            (KeyCode::Right, _) => self.input.right(),
            (KeyCode::Home, _) | (KeyCode::Char('a'), true) => self.input.home(),
            (KeyCode::End, _) | (KeyCode::Char('e'), true) => self.input.end(),
            (KeyCode::Delete, _) => self.input.delete(),
            (KeyCode::Up, _) => {
                if self.menu_open() {
                    self.menu_at = self.menu_at.saturating_sub(1);
                } else if !self.input.contains('\n') {
                    self.recall(-1);
                }
            }
            (KeyCode::Down, _) => {
                if self.menu_open() {
                    self.menu_at =
                        (self.menu_at + 1).min(self.menu_items().len().saturating_sub(1));
                } else if !self.input.contains('\n') {
                    self.recall(1);
                }
            }
            (KeyCode::Char(c), false) => {
                self.input.insert(c);
                self.history_at = None;
                self.menu_at = 0;
            }
            _ => {}
        }
    }

    pub(super) fn stop(&mut self) {
        if let Some(r) = &self.running {
            r.cancel.cancel();
        }
    }

    pub(super) fn answer_plan(&mut self, decision: PlanDecision) {
        let Some(reply) = self.plan_ask.take() else {
            return;
        };
        let note = match &decision {
            PlanDecision::Go => "  plan: go".to_string(),
            PlanDecision::Stop => "  plan: stop".to_string(),
            PlanDecision::Revise(notes) => format!("  plan: revise · {notes}"),
        };
        self.push(view::note_line(&note));
        let _ = reply.send(decision);
    }

    /// Walk the history with the arrow keys.
    pub(super) fn recall(&mut self, step: i32) {
        if self.history.is_empty() {
            return;
        }
        let at = match (self.history_at, step) {
            (None, s) if s < 0 => Some(self.history.len() - 1),
            (None, _) => None,
            (Some(0), s) if s < 0 => Some(0),
            (Some(i), s) if s < 0 => Some(i - 1),
            (Some(i), _) if i + 1 >= self.history.len() => None,
            (Some(i), _) => Some(i + 1),
        };
        self.history_at = at;
        self.input
            .set(at.map(|i| self.history[i].clone()).unwrap_or_default());
    }

    // ---- The slash menu ---------------------------------------------------------

    pub(super) fn menu_open(&self) -> bool {
        self.input.starts_with('/')
            && !self.input.contains(char::is_whitespace)
            && !self.menu_items().is_empty()
    }

    /// Built-in commands, then the custom ones from `.weft/commands`, that
    /// start with what has been typed.
    pub(super) fn menu_items(&self) -> Vec<MenuItem> {
        let typed = &*self.input;
        let mut items: Vec<MenuItem> = COMMANDS
            .iter()
            .filter(|(name, _, _)| name.starts_with(typed))
            .map(|(name, what, takes_task)| MenuItem {
                name: name.to_string(),
                what: what.to_string(),
                takes_task: *takes_task,
            })
            .collect();
        for c in commands::load(&self.session.ctx.root) {
            let name = format!("/{}", c.name);
            if name.starts_with(typed) {
                items.push(MenuItem {
                    name,
                    what: c.description.clone(),
                    takes_task: c.body.contains("$ARGUMENTS"),
                });
            }
        }
        items
    }

    pub(super) fn complete_menu(&mut self, tx: &mpsc::Sender<UiMsg>) {
        let items = self.menu_items();
        let Some(item) = items.get(self.menu_at.min(items.len().saturating_sub(1))) else {
            return;
        };
        if item.takes_task {
            self.input.set(format!("{} ", item.name));
        } else {
            self.input.set(item.name.clone());
            self.submit(tx);
        }
    }
}
