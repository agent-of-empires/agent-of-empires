use aoe_plugin_api::agent_integration::{AgentIntegrationAnswer, AgentIntegrationPrompt};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    prelude::*,
    widgets::{Clear, Paragraph, Wrap},
};
use tui_input::{backend::crossterm::EventHandler, Input};

use super::{dialog_block, render_dialog_frame, DialogResult};
use crate::tui::components::text::wrapped_line_count;
use crate::tui::styles::Theme;

/// Answer editor for a provider-native choice prompt.
pub struct AgentQuestionDialog {
    prompt: AgentIntegrationPrompt,
    question: usize,
    selected: Vec<Vec<String>>,
    option_cursor: Vec<usize>,
    custom: Vec<Input>,
    editing_custom: bool,
    scroll_offsets: Vec<usize>,
    scroll_max: Vec<usize>,
    page_rows: usize,
    follow_focus: bool,
    focus_visible: bool,
}

impl AgentQuestionDialog {
    pub fn new(prompt: AgentIntegrationPrompt) -> Self {
        let AgentIntegrationPrompt::Question { questions, .. } = &prompt else {
            unreachable!()
        };
        let selected = questions
            .iter()
            .map(|q| {
                q.recommended
                    .and_then(|i| q.options.get(i))
                    .map(|o| vec![o.label.clone()])
                    .unwrap_or_default()
            })
            .collect();
        let option_cursor = questions
            .iter()
            .map(|q| q.recommended.unwrap_or(0))
            .collect();
        let custom = questions.iter().map(|_| Input::default()).collect();
        let question_count = questions.len();
        Self {
            prompt,
            question: 0,
            selected,
            option_cursor,
            custom,
            editing_custom: false,
            scroll_offsets: vec![0; question_count],
            scroll_max: vec![0; question_count],
            page_rows: 1,
            follow_focus: true,
            focus_visible: true,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DialogResult<Vec<AgentIntegrationAnswer>> {
        match key.code {
            KeyCode::PageUp => {
                self.scroll_question(false);
                return DialogResult::Continue;
            }
            KeyCode::PageDown => {
                self.scroll_question(true);
                return DialogResult::Continue;
            }
            _ => {}
        }
        self.follow_focus = true;
        let AgentIntegrationPrompt::Question { questions, .. } = &self.prompt else {
            unreachable!()
        };
        let current = &questions[self.question];
        if self.editing_custom {
            match key.code {
                KeyCode::Esc => {
                    self.editing_custom = false;
                    return DialogResult::Continue;
                }
                KeyCode::Enter => {
                    self.editing_custom = false;
                    return DialogResult::Continue;
                }
                _ => {
                    self.custom[self.question].handle_event(&crossterm::event::Event::Key(key));
                    return DialogResult::Continue;
                }
            }
        }
        if !self.focus_visible
            && !current.options.is_empty()
            && matches!(key.code, KeyCode::Char(' ') | KeyCode::Enter)
        {
            return DialogResult::Continue;
        }
        match key.code {
            KeyCode::Esc => DialogResult::Cancel,
            KeyCode::Up | KeyCode::Char('k') => {
                if !current.options.is_empty() {
                    let cursor = &mut self.option_cursor[self.question];
                    *cursor = (*cursor + current.options.len() - 1) % current.options.len();
                    let index = *cursor;
                    if !current.multi {
                        let label = current.options[index].label.clone();
                        Self::select(
                            &mut self.selected[self.question],
                            &mut self.custom[self.question],
                            label,
                            false,
                        );
                    }
                }
                DialogResult::Continue
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if !current.options.is_empty() {
                    let cursor = &mut self.option_cursor[self.question];
                    *cursor = (*cursor + 1) % current.options.len();
                    let index = *cursor;
                    if !current.multi {
                        let label = current.options[index].label.clone();
                        Self::select(
                            &mut self.selected[self.question],
                            &mut self.custom[self.question],
                            label,
                            false,
                        );
                    }
                }
                DialogResult::Continue
            }
            KeyCode::Char(' ') => {
                if let Some(option) = current.options.get(self.option_cursor[self.question]) {
                    Self::select(
                        &mut self.selected[self.question],
                        &mut self.custom[self.question],
                        option.label.clone(),
                        current.multi,
                    );
                }
                DialogResult::Continue
            }
            KeyCode::Char('c') => {
                if !current.multi {
                    self.selected[self.question].clear();
                }
                self.editing_custom = true;
                DialogResult::Continue
            }
            KeyCode::Tab | KeyCode::Right => {
                self.question = (self.question + 1) % questions.len();
                DialogResult::Continue
            }
            KeyCode::BackTab | KeyCode::Left => {
                self.question = (self.question + questions.len() - 1) % questions.len();
                DialogResult::Continue
            }
            KeyCode::Enter => {
                if !current.multi
                    && self.selected[self.question].is_empty()
                    && self.custom[self.question].value().trim().is_empty()
                {
                    if let Some(option) = current.options.get(self.option_cursor[self.question]) {
                        Self::select(
                            &mut self.selected[self.question],
                            &mut self.custom[self.question],
                            option.label.clone(),
                            false,
                        );
                    }
                }
                if self.question + 1 < questions.len() {
                    self.question += 1;
                    return DialogResult::Continue;
                }
                if let Some(missing) = questions.iter().enumerate().position(|(i, question)| {
                    !question.multi
                        && self.selected[i].len()
                            + usize::from(!self.custom[i].value().trim().is_empty())
                            != 1
                }) {
                    self.question = missing;
                    return DialogResult::Continue;
                }
                DialogResult::Submit(
                    questions
                        .iter()
                        .enumerate()
                        .map(|(i, q)| AgentIntegrationAnswer {
                            id: q.id.clone(),
                            selected_options: self.selected[i].clone(),
                            custom_input: (!self.custom[i].value().trim().is_empty())
                                .then(|| self.custom[i].value().to_owned()),
                        })
                        .collect(),
                )
            }
            _ => DialogResult::Continue,
        }
    }
    pub fn handle_paste(&mut self, text: &str) {
        if self.editing_custom {
            self.follow_focus = true;
            for ch in text.chars().filter(|ch| *ch != '\n' && *ch != '\r') {
                self.custom[self.question].handle(tui_input::InputRequest::InsertChar(ch));
            }
        }
    }

    fn scroll_question(&mut self, down: bool) {
        let question = self.question;
        let page_rows = self.page_rows;
        let max_scroll = self.scroll_max[question];
        let offset = &mut self.scroll_offsets[question];
        if down {
            *offset = offset.saturating_add(page_rows).min(max_scroll);
        } else {
            *offset = offset.saturating_sub(page_rows);
        }
        self.follow_focus = false;
    }

    fn select(selected: &mut Vec<String>, custom: &mut Input, label: String, multi: bool) {
        if !multi {
            *custom = Input::default();
        }
        if multi {
            if let Some(index) = selected.iter().position(|current| *current == label) {
                selected.remove(index);
            } else {
                selected.push(label);
            }
        } else {
            selected.clear();
            selected.push(label);
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let AgentIntegrationPrompt::Question {
            title, questions, ..
        } = &self.prompt
        else {
            unreachable!()
        };
        let block = dialog_block(format!(" {title} "), theme);
        let (_, inner) = render_dialog_frame(frame, area, 70, 18, block);
        frame.render_widget(Clear, inner);
        let q = &questions[self.question];
        let mut lines = vec![Line::from(format!(
            "Question {}/{}: {}",
            self.question + 1,
            questions.len(),
            q.question
        ))];
        lines.push(Line::from(if q.multi {
            "Select multiple: Space toggles; c enters custom answer"
        } else {
            "Select one: Up/Down; c enters custom answer"
        }));
        for (index, option) in q.options.iter().enumerate() {
            let cursor = if index == self.option_cursor[self.question] {
                ">"
            } else {
                " "
            };
            let marker = if self.selected[self.question].contains(&option.label) {
                "[x]"
            } else {
                "[ ]"
            };
            let description = option.description.as_deref().unwrap_or("");
            lines.push(Line::from(format!(
                "{cursor} {marker} {} {description}",
                option.label
            )));
        }
        let custom_focused = self.editing_custom || q.options.is_empty();
        let custom_line = lines.len();
        lines.push(Line::from(format!(
            "{} Custom answer (press c): {}",
            if custom_focused { ">" } else { " " },
            self.custom[self.question].value()
        )));
        let focus_line = if custom_focused {
            custom_line
        } else {
            2 + self.option_cursor[self.question].min(q.options.len().saturating_sub(1))
        };

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(2)])
            .split(inner);
        let content = chunks[0];
        let footer = chunks[1];
        let visible_height = content.height as usize;
        let row_counts: Vec<usize> = lines
            .iter()
            .map(|line| wrapped_line_count(line, content.width.max(1)))
            .collect();
        let mut row_starts = Vec::with_capacity(row_counts.len() + 1);
        let mut total_rows = 0usize;
        for rows in &row_counts {
            row_starts.push(total_rows);
            total_rows += rows;
        }
        row_starts.push(total_rows);

        self.page_rows = visible_height.saturating_sub(1).max(1);
        let max_scroll = total_rows.saturating_sub(visible_height);
        self.scroll_max[self.question] = max_scroll;
        let focus_start = row_starts[focus_line];
        let focus_end = row_starts[focus_line + 1];
        let mut scroll = self.scroll_offsets[self.question].min(max_scroll);
        if visible_height > 0 && self.follow_focus {
            if focus_start < scroll {
                scroll = focus_start;
            }
            if focus_end > scroll.saturating_add(visible_height) {
                scroll = if focus_end - focus_start > visible_height {
                    focus_start
                } else {
                    focus_end.saturating_sub(visible_height)
                };
            }
        }
        self.scroll_offsets[self.question] = scroll;
        self.focus_visible = visible_height > 0
            && focus_start >= scroll
            && focus_start < scroll.saturating_add(visible_height);
        self.follow_focus = false;

        if visible_height > 0 {
            let first = (0..lines.len())
                .find(|&index| row_starts[index + 1] > scroll)
                .unwrap_or(lines.len());
            let visible_end = scroll.saturating_add(visible_height);
            let end = (first..lines.len())
                .find(|index| row_starts[*index] >= visible_end)
                .unwrap_or(lines.len());
            if first < end {
                let line_scroll = scroll
                    .saturating_sub(row_starts[first])
                    .min(u16::MAX as usize) as u16;
                frame.render_widget(
                    Paragraph::new(lines[first..end].to_vec())
                        .wrap(Wrap { trim: true })
                        .scroll((line_scroll, 0)),
                    content,
                );
            }
        }

        frame.render_widget(
            Paragraph::new(vec![
                Line::from("Up/Down: move  Space: select  c: custom  PgUp/PgDn: scroll"),
                Line::from("Tab: next question  Enter: continue/submit  Esc: cancel"),
            ])
            .style(Style::default().fg(theme.dimmed)),
            footer,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aoe_plugin_api::agent_integration::{AgentIntegrationOption, AgentIntegrationQuestion};

    #[test]
    fn single_choice_enter_submits_the_highlighted_option_or_custom_answer() {
        for recommended in [None, Some(0)] {
            let prompt = AgentIntegrationPrompt::Question {
                id: "question".into(),
                title: "Configure".into(),
                questions: vec![AgentIntegrationQuestion {
                    id: "database".into(),
                    question: "Database?".into(),
                    options: vec![
                        AgentIntegrationOption {
                            label: "SQLite".into(),
                            description: None,
                        },
                        AgentIntegrationOption {
                            label: "Postgres".into(),
                            description: None,
                        },
                    ],
                    multi: false,
                    recommended,
                }],
            };
            let mut dialog = AgentQuestionDialog::new(prompt.clone());
            assert!(matches!(
                dialog.handle_key(KeyEvent::new(
                    KeyCode::Enter,
                    crossterm::event::KeyModifiers::NONE
                )),
                DialogResult::Submit(answers) if answers[0].selected_options == vec!["SQLite".to_owned()]
            ));

            let mut dialog = AgentQuestionDialog::new(prompt.clone());
            dialog.handle_key(KeyEvent::new(
                KeyCode::Down,
                crossterm::event::KeyModifiers::NONE,
            ));
            assert!(matches!(
                dialog.handle_key(KeyEvent::new(
                    KeyCode::Enter,
                    crossterm::event::KeyModifiers::NONE
                )),
                DialogResult::Submit(answers) if answers[0].selected_options == vec!["Postgres".to_owned()]
            ));

            let mut dialog = AgentQuestionDialog::new(prompt);
            let key = |code| KeyEvent::new(code, crossterm::event::KeyModifiers::NONE);
            dialog.handle_key(key(KeyCode::Char('c')));
            dialog.handle_paste("Custom database");
            dialog.handle_key(key(KeyCode::Enter));
            assert!(matches!(
                dialog.handle_key(key(KeyCode::Enter)),
                DialogResult::Submit(answers)
                    if answers[0].selected_options.is_empty()
                        && answers[0].custom_input.as_deref() == Some("Custom database")
            ));
        }
    }

    #[test]
    fn multi_choice_can_toggle_multiple_options() {
        let prompt = AgentIntegrationPrompt::Question {
            id: "question".into(),
            title: "Configure".into(),
            questions: vec![AgentIntegrationQuestion {
                id: "database".into(),
                question: "Databases?".into(),
                options: vec![
                    AgentIntegrationOption {
                        label: "SQLite".into(),
                        description: None,
                    },
                    AgentIntegrationOption {
                        label: "Postgres".into(),
                        description: None,
                    },
                ],
                multi: true,
                recommended: None,
            }],
        };
        let key = |code| KeyEvent::new(code, crossterm::event::KeyModifiers::NONE);
        assert!(matches!(
            AgentQuestionDialog::new(prompt.clone()).handle_key(key(KeyCode::Enter)),
            DialogResult::Submit(answers) if answers[0].selected_options.is_empty()
        ));
        let mut dialog = AgentQuestionDialog::new(prompt);
        dialog.handle_key(key(KeyCode::Char(' ')));
        dialog.handle_key(key(KeyCode::Down));
        dialog.handle_key(key(KeyCode::Char(' ')));
        assert!(matches!(
            dialog.handle_key(key(KeyCode::Enter)),
            DialogResult::Submit(answers)
                if answers[0].selected_options == vec!["SQLite".to_owned(), "Postgres".to_owned()]
        ));
    }

    #[test]
    fn incomplete_single_choice_keeps_answers_and_returns_to_the_missing_question() {
        let key = |code| KeyEvent::new(code, crossterm::event::KeyModifiers::NONE);
        for cleared_custom in [false, true] {
            let question = |id: &str, multi, recommended| AgentIntegrationQuestion {
                id: id.into(),
                question: id.into(),
                options: vec![AgentIntegrationOption {
                    label: "one".into(),
                    description: None,
                }],
                multi,
                recommended,
            };
            let prompt = AgentIntegrationPrompt::Question {
                id: "prompt".into(),
                title: "Configure".into(),
                questions: vec![
                    question("first", false, cleared_custom.then_some(0)),
                    question("second", false, Some(0)),
                    question("optional", true, None),
                ],
            };
            let mut dialog = AgentQuestionDialog::new(prompt);
            if cleared_custom {
                dialog.handle_key(key(KeyCode::Char('c')));
                dialog.handle_paste("   ");
                dialog.handle_key(key(KeyCode::Esc));
            }
            dialog.handle_key(key(KeyCode::Right));
            dialog.handle_key(key(KeyCode::Right));
            assert!(matches!(
                dialog.handle_key(key(KeyCode::Enter)),
                DialogResult::Continue
            ));
            assert_eq!(dialog.question, 0);
            dialog.handle_key(key(KeyCode::Char(' ')));
            dialog.handle_key(key(KeyCode::Enter));
            dialog.handle_key(key(KeyCode::Enter));
            assert!(matches!(
                dialog.handle_key(key(KeyCode::Enter)),
                DialogResult::Submit(answers)
                    if answers[0].selected_options == ["one"]
                        && answers[0].custom_input.is_none()
                        && answers[1].selected_options == ["one"]
                        && answers[2].selected_options.is_empty()
            ));
        }
    }

    #[test]
    fn long_questions_scroll_with_option_and_custom_focus() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let key = |code| KeyEvent::new(code, crossterm::event::KeyModifiers::NONE);
        let options = (0..24)
            .map(|index| AgentIntegrationOption {
                label: format!("option-{index:02}"),
                description: Some(format!(
                    "{}tail-{index:02}",
                    "wrapped option details ".repeat(4)
                )),
            })
            .collect();
        let prompt = AgentIntegrationPrompt::Question {
            id: "prompt".into(),
            title: "Configure".into(),
            questions: vec![AgentIntegrationQuestion {
                id: "question".into(),
                question: "Choose an option".into(),
                options,
                multi: false,
                recommended: None,
            }],
        };
        let theme = crate::tui::styles::load_theme_with_mode("empire", false);
        let mut dialog = AgentQuestionDialog::new(prompt);
        let render = |dialog: &mut AgentQuestionDialog| {
            let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
            terminal
                .draw(|frame| dialog.render(frame, frame.area(), &theme))
                .unwrap();
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>()
        };

        let initial = render(&mut dialog);
        assert!(initial.contains("PgUp/PgDn: scroll"));
        for _ in 0..23 {
            dialog.handle_key(key(KeyCode::Down));
        }
        let focused = render(&mut dialog);
        assert!(focused.contains("option-23"));
        assert!(focused.contains("tail-23"));
        assert_eq!(dialog.selected[0], vec!["option-23".to_owned()]);

        let bottom = dialog.scroll_offsets[0];
        dialog.handle_key(key(KeyCode::PageUp));
        assert!(dialog.scroll_offsets[0] < bottom);
        assert_eq!(dialog.option_cursor[0], 23);
        while dialog.scroll_offsets[0] > 0 {
            dialog.handle_key(key(KeyCode::PageUp));
        }
        assert!(render(&mut dialog).contains("option-00"));
        while dialog.scroll_offsets[0] < dialog.scroll_max[0] {
            dialog.handle_key(key(KeyCode::PageDown));
        }
        assert!(render(&mut dialog).contains("option-23"));

        dialog.handle_key(key(KeyCode::Char('c')));
        let custom = format!("{}custom-answer-tail", "custom answer ".repeat(20));
        dialog.handle_paste(&custom);
        assert!(render(&mut dialog).contains("custom-answer-tail"));
    }
    #[test]
    fn selection_requires_the_focused_option_to_be_visible() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let options = (0..24)
            .map(|index| AgentIntegrationOption {
                label: format!("option-{index:02}"),
                description: None,
            })
            .collect::<Vec<_>>();
        let prompt = |multi| AgentIntegrationPrompt::Question {
            id: "prompt".into(),
            title: "Configure".into(),
            questions: vec![AgentIntegrationQuestion {
                id: "question".into(),
                question: "Choose an option".into(),
                options: options.clone(),
                multi,
                recommended: (!multi).then_some(0),
            }],
        };
        let key = |code| KeyEvent::new(code, crossterm::event::KeyModifiers::NONE);
        let theme = crate::tui::styles::load_theme_with_mode("empire", false);
        let render = |dialog: &mut AgentQuestionDialog| {
            let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
            terminal
                .draw(|frame| dialog.render(frame, frame.area(), &theme))
                .unwrap();
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>()
        };

        for multi in [false, true] {
            let mut dialog = AgentQuestionDialog::new(prompt(multi));
            render(&mut dialog);
            dialog.handle_key(key(KeyCode::PageDown));
            render(&mut dialog);
            dialog.handle_key(key(KeyCode::PageDown));
            let scrolled = render(&mut dialog);
            assert!(!scrolled.contains("option-00"));

            // Enter/Space on a hidden focus must not act on the off-screen
            // option: it snaps the view back and does nothing this press.
            let selection_key = if multi {
                KeyCode::Char(' ')
            } else {
                KeyCode::Enter
            };
            assert!(matches!(
                dialog.handle_key(key(selection_key)),
                DialogResult::Continue
            ));
            assert!(render(&mut dialog).contains("option-00"));

            if multi {
                // The focused option is on screen now; a fresh Space selects it.
                dialog.handle_key(key(KeyCode::Char(' ')));
            }
            assert!(matches!(
                dialog.handle_key(key(KeyCode::Enter)),
                DialogResult::Submit(answers)
                    if answers[0].selected_options == ["option-00"]
            ));
        }
    }
}
