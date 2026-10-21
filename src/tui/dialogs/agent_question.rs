use aoe_plugin_api::agent_integration::{AgentIntegrationAnswer, AgentIntegrationPrompt};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    prelude::*,
    widgets::{Clear, Paragraph, Wrap},
};
use tui_input::{backend::crossterm::EventHandler, Input};

use super::{dialog_block, render_dialog_frame, DialogResult};
use crate::tui::styles::Theme;

/// Answer editor for a provider-native choice prompt.
pub struct AgentQuestionDialog {
    prompt: AgentIntegrationPrompt,
    question: usize,
    selected: Vec<Vec<String>>,
    option_cursor: Vec<usize>,
    custom: Vec<Input>,
    editing_custom: bool,
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
        Self {
            prompt,
            question: 0,
            selected,
            option_cursor,
            custom,
            editing_custom: false,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DialogResult<Vec<AgentIntegrationAnswer>> {
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
            for ch in text.chars().filter(|ch| *ch != '\n' && *ch != '\r') {
                self.custom[self.question].handle(tui_input::InputRequest::InsertChar(ch));
            }
        }
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
        lines.push(Line::from(format!(
            "Custom answer (press c): {}",
            self.custom[self.question].value()
        )));
        lines.push(Line::from(
            "Tab/Enter: next question • Enter: submit • Esc: cancel",
        ));
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
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
}
