//! Dialog for answering a provider-native agent question prompt.

use aoe_plugin_api::agent_integration::{AgentIntegrationAnswer, AgentIntegrationQuestion};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::*;
use ratatui_textarea::TextArea;

use super::DialogResult;
use crate::tui::styles::Theme;

#[derive(Debug, Clone, PartialEq, Eq)]
struct QuestionAnswer {
    selected_options: Vec<String>,
    custom_input: String,
}

pub struct AgentQuestionDialog {
    title: String,
    questions: Vec<AgentIntegrationQuestion>,
    answers: Vec<QuestionAnswer>,
    question_index: usize,
    option_index: usize,
    editing_custom: bool,
    custom_input: TextArea<'static>,
}

impl AgentQuestionDialog {
    pub fn new(title: impl Into<String>, questions: Vec<AgentIntegrationQuestion>) -> Self {
        let answers = questions
            .iter()
            .map(|_| QuestionAnswer {
                selected_options: Vec::new(),
                custom_input: String::new(),
            })
            .collect();
        Self {
            title: title.into(),
            questions,
            answers,
            question_index: 0,
            option_index: 0,
            editing_custom: false,
            custom_input: TextArea::default(),
        }
    }

    fn current_question(&self) -> Option<&AgentIntegrationQuestion> {
        self.questions.get(self.question_index)
    }

    fn current_answer(&self) -> Option<&QuestionAnswer> {
        self.answers.get(self.question_index)
    }

    fn current_answer_mut(&mut self) -> Option<&mut QuestionAnswer> {
        self.answers.get_mut(self.question_index)
    }

    fn set_question(&mut self, index: usize) {
        if index >= self.questions.len() {
            return;
        }
        if let Some(answer) = self.answers.get_mut(self.question_index) {
            answer.custom_input = self.custom_input.lines().join("\n");
        }
        self.question_index = index;
        self.option_index = 0;
        self.custom_input = TextArea::new(vec![self.answers[index].custom_input.clone()]);
    }

    fn toggle_current_option(&mut self) {
        let Some(option) = self
            .current_question()
            .and_then(|question| question.options.get(self.option_index))
        else {
            return;
        };
        let id = option.label.clone();
        let selected = self
            .current_answer()
            .is_some_and(|answer| answer.selected_options.contains(&id));
        let multi = self
            .current_question()
            .is_some_and(|question| question.multi);
        if !selected && !multi {
            self.custom_input = TextArea::default();
        }
        let Some(answer) = self.current_answer_mut() else {
            return;
        };
        if selected {
            answer.selected_options.retain(|item| item != &id);
        } else if multi {
            answer.selected_options.push(id);
        } else {
            answer.selected_options.clear();
            answer.custom_input.clear();
            answer.selected_options.push(id);
        }
    }

    fn submit(&mut self) -> Vec<AgentIntegrationAnswer> {
        if let Some(answer) = self.answers.get_mut(self.question_index) {
            answer.custom_input = self.custom_input.lines().join("\n");
        }
        self.questions
            .iter()
            .zip(&self.answers)
            .map(|(question, answer)| AgentIntegrationAnswer {
                id: question.id.clone(),
                selected_options: answer.selected_options.clone(),
                custom_input: (!answer.custom_input.trim().is_empty())
                    .then(|| answer.custom_input.clone()),
            })
            .collect()
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DialogResult<Vec<AgentIntegrationAnswer>> {
        if key.code == KeyCode::Esc {
            return DialogResult::Cancel;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Left | KeyCode::Char('p') => {
                    if self.question_index > 0 {
                        self.set_question(self.question_index - 1);
                    }
                    return DialogResult::Continue;
                }
                KeyCode::Right | KeyCode::Char('n') => {
                    if self.question_index + 1 < self.questions.len() {
                        self.set_question(self.question_index + 1);
                    }
                    return DialogResult::Continue;
                }
                KeyCode::Char('s') | KeyCode::Enter => return DialogResult::Submit(self.submit()),
                _ => return DialogResult::Continue,
            }
        }
        if self.editing_custom {
            if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
                let custom_input = self.custom_input.lines().join("\n");
                if let Some(answer) = self.current_answer_mut() {
                    answer.custom_input = custom_input;
                }
                self.editing_custom = false;
            } else {
                self.custom_input.input(key);
            }
            return DialogResult::Continue;
        }
        match key.code {
            KeyCode::Up if self.option_index > 0 => self.option_index -= 1,
            KeyCode::Down
                if self
                    .current_question()
                    .is_some_and(|question| self.option_index + 1 < question.options.len()) =>
            {
                self.option_index += 1
            }
            KeyCode::Tab => {
                if self
                    .current_question()
                    .is_some_and(|question| !question.multi)
                {
                    if let Some(answer) = self.current_answer_mut() {
                        answer.selected_options.clear();
                    }
                }
                self.editing_custom = true;
            }
            KeyCode::Enter | KeyCode::Char(' ')
                if self
                    .current_question()
                    .is_some_and(|question| !question.options.is_empty()) =>
            {
                self.toggle_current_option()
            }
            _ => {}
        }
        DialogResult::Continue
    }

    pub fn handle_paste(&mut self, text: &str) {
        if self.editing_custom {
            self.custom_input.insert_str(text);
            let custom_input = self.custom_input.lines().join("\n");
            if let Some(answer) = self.current_answer_mut() {
                answer.custom_input = custom_input;
            }
        }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let width = (area.width * 80 / 100).max(48).min(area.width);
        let height = (area.height * 75 / 100).max(12).min(area.height);
        let dialog_area = super::centered_rect(area, width, height);
        frame.render_widget(Clear, dialog_area);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.accent))
            .title(format!(
                " {} · Question {}/{} ",
                self.title,
                self.question_index + 1,
                self.questions.len()
            ))
            .title_style(Style::default().fg(theme.title).bold());
        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);
        let Some(question) = self.current_question() else {
            return;
        };
        let mut lines = vec![Line::from(Span::styled(
            &question.question,
            Style::default().fg(theme.text).bold(),
        ))];
        for (index, option) in question.options.iter().enumerate() {
            let checked = self
                .current_answer()
                .is_some_and(|answer| answer.selected_options.contains(&option.label));
            let marker = if checked { "[x]" } else { "[ ]" };
            let prefix = if index == self.option_index && !self.editing_custom {
                ">"
            } else {
                " "
            };
            lines.push(Line::from(format!(
                "{prefix} {marker} {}{}",
                option.label,
                if question.recommended == Some(index) {
                    " (recommended)"
                } else {
                    ""
                }
            )));
            if let Some(description) = &option.description {
                lines.push(Line::from(Span::styled(
                    format!("      {description}"),
                    Style::default().fg(theme.dimmed),
                )));
            }
        }
        let options_height = lines.len().min(inner.height.saturating_sub(4) as usize) as u16;
        frame.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: false }),
            Rect {
                x: inner.x,
                y: inner.y,
                width: inner.width,
                height: options_height,
            },
        );
        let custom_area = Rect {
            x: inner.x,
            y: inner.y.saturating_add(options_height),
            width: inner.width,
            height: inner
                .height
                .saturating_sub(options_height)
                .saturating_sub(1),
        };
        let custom_block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(if self.editing_custom {
                theme.accent
            } else {
                theme.border
            }))
            .title(" Custom answer (optional) ");
        let mut custom_input = self.custom_input.clone();
        custom_input.set_block(custom_block);
        custom_input.set_style(Style::default().fg(theme.text));
        if !self.editing_custom {
            custom_input.set_cursor_style(Style::default());
        }
        frame.render_widget(&custom_input, custom_area);
        frame.render_widget(
            Paragraph::new(
                "↑/↓ option  Enter/Space select  Tab custom text  Ctrl←/→ question  Ctrl+Enter/Ctrl+S submit  Esc cancel",
            )
            .style(Style::default().fg(theme.dimmed)),
            Rect {
                x: inner.x,
                y: inner.y.saturating_add(inner.height.saturating_sub(1)),
                width: inner.width,
                height: 1,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn control(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    fn questions() -> Vec<AgentIntegrationQuestion> {
        vec![
            AgentIntegrationQuestion {
                id: "language".into(),
                question: "Language?".into(),
                options: vec![option("Rust"), option("Go")],
                multi: false,
                recommended: None,
            },
            AgentIntegrationQuestion {
                id: "checks".into(),
                question: "Checks?".into(),
                options: vec![option("test"), option("lint")],
                multi: true,
                recommended: None,
            },
        ]
    }

    fn option(label: &str) -> aoe_plugin_api::agent_integration::AgentIntegrationOption {
        aoe_plugin_api::agent_integration::AgentIntegrationOption {
            label: label.into(),
            description: None,
        }
    }

    #[test]
    fn single_and_multiple_selections_are_batched_with_all_question_ids() {
        let mut dialog = AgentQuestionDialog::new("Session", questions());
        dialog.handle_key(key(KeyCode::Tab));
        for ch in "discarded".chars() {
            dialog.handle_key(key(KeyCode::Char(ch)));
        }
        dialog.handle_key(key(KeyCode::Tab));
        dialog.handle_key(key(KeyCode::Down));
        dialog.handle_key(key(KeyCode::Enter));
        dialog.handle_key(control(KeyCode::Right));
        dialog.handle_key(key(KeyCode::Enter));
        dialog.handle_key(key(KeyCode::Down));
        dialog.handle_key(key(KeyCode::Enter));
        let DialogResult::Submit(answers) = dialog.handle_key(control(KeyCode::Char('s'))) else {
            panic!("expected submission")
        };
        assert_eq!(answers.len(), 2);
        assert_eq!(answers[0].id, "language");
        assert_eq!(answers[0].selected_options, vec!["Go".to_string()]);
        assert_eq!(answers[0].custom_input, None);
        assert_eq!(
            answers[1].selected_options,
            vec!["test".to_string(), "lint".to_string()]
        );
    }

    #[test]
    fn custom_text_survives_navigation_and_is_batched() {
        let mut dialog = AgentQuestionDialog::new("Session", questions());
        dialog.handle_key(key(KeyCode::Tab));
        for ch in "explain why".chars() {
            dialog.handle_key(key(KeyCode::Char(ch)));
        }
        dialog.handle_key(control(KeyCode::Right));
        dialog.handle_key(control(KeyCode::Left));
        let DialogResult::Submit(answers) = dialog.handle_key(control(KeyCode::Char('s'))) else {
            panic!("expected submission")
        };
        assert_eq!(answers[0].custom_input.as_deref(), Some("explain why"));
        assert_eq!(answers[1].custom_input, None);
    }

    #[test]
    fn escape_cancels_without_producing_answers() {
        let mut dialog = AgentQuestionDialog::new("Session", questions());
        assert!(matches!(
            dialog.handle_key(key(KeyCode::Esc)),
            DialogResult::Cancel
        ));
    }
}
