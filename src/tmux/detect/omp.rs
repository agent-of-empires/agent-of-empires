//! Current classic OMP UI roles, independent of status presets and extension footers.

use std::sync::LazyLock;

use regex::Regex;
use unicode_width::UnicodeWidthStr;

use super::adapter::{Adapter, Evaluation};
use super::region::{Evidence, Rows, Screen};
use super::{CapturePolicy, CaptureScope, Observation};

pub(super) static ADAPTER: Adapter = Adapter {
    capture: CapturePolicy {
        scope: CaptureScope::Visible,
        fetch_missing_title: true,
        observe_ownership: true,
    },
    evidence_names: &[
        "activity",
        "dialog",
        "hidden_ask",
        "working_title",
        "error",
        "local_history",
    ],
    observation_token: |observation| {
        Some(if !observation.live_owned {
            0
        } else if title_is_working(observation.title) {
            2
        } else {
            1
        })
    },
    evaluate: |manifest, rows, observation| {
        with_screen(rows, observation, |screen, evidence| {
            manifest
                .evaluate(screen, observation.hook)
                .map(|rule| Evaluation::new(rule, evidence))
        })
    },
    #[cfg(test)]
    rule_matches: |manifest, rule, rows, observation| {
        with_screen(rows, observation, |screen, _| {
            manifest.rule_matches(rule, screen, observation.hook)
        })
    },
};

fn with_screen<'a, T>(
    mut rows: Rows<'a>,
    observation: &Observation<'a>,
    evaluate: impl FnOnce(&Screen<'_>, &[Evidence<'a>]) -> T,
) -> T {
    let surface = Surface::new(rows.source, &mut rows.non_empty, &rows.blank_before);
    let live = |evidence| {
        if observation.live_owned {
            evidence
        } else {
            Evidence::default()
        }
    };
    let evidence = [
        live(surface.activity),
        live(surface.dialog),
        live(surface.hidden_ask),
        if observation.live_owned && title_is_working(observation.title) {
            Evidence {
                text: observation.title,
                bottom_offset: 0,
                diagnostic: None,
            }
        } else {
            Evidence::default()
        },
        if observation.live_owned && !surface.error.text.is_empty() {
            surface.error
        } else {
            surface.legacy_error
        },
        live(surface.local_history),
    ];
    evaluate(&Screen::new(rows, observation.title, &evidence), &evidence)
}

fn title_is_working(title: &str) -> bool {
    let Some(rest) = title.strip_prefix("π ") else {
        return false;
    };
    let Some(frame) = rest.chars().next() else {
        return false;
    };
    matches!(
        frame,
        '\u{2800}'..='\u{28ff}' | '○' | '◔' | '◑' | '◕' | '●' | '-' | '\\' | '|' | '/' | ':'
    ) && (rest.len() == frame.len_utf8() || rest[frame.len_utf8()..].starts_with(' '))
}

fn line_evidence<'a>(lines: &[&'a str], index: usize) -> Evidence<'a> {
    Evidence {
        text: lines[index],
        bottom_offset: lines.len() - index - 1,
        diagnostic: None,
    }
}

#[derive(Default)]
struct Surface<'a> {
    activity: Evidence<'a>,
    dialog: Evidence<'a>,
    hidden_ask: Evidence<'a>,
    error: Evidence<'a>,
    legacy_error: Evidence<'a>,
    local_history: Evidence<'a>,
}

#[derive(Clone, Copy)]
enum Body {
    Box,
    Band,
    Prompt,
    Rules,
    Field,
    Rail,
}

#[derive(Clone, Copy)]
struct Composer {
    start: usize,
    end: usize,
    body: Body,
    slot: Option<usize>,
    clipped: bool,
}

const DISMISSAL: &str = "Dismissed when you send your next message";

macro_rules! modifier_pattern {
    () => {
        r"(?:(?:c\s*t\s*r\s*l|a\s*l\s*t|s\s*h\s*i\s*f\s*t|m\s*e\s*t\s*a|c\s*m\s*d|s\s*u\s*p\s*e\s*r|o\s*p\s*t\s*i\s*o\s*n)\+\s*|[⌃⌥⇧⌘\x{f0633}-\x{f0636}]\s*)"
    };
}

macro_rules! key_pattern {
    () => {
        concat!(
            r"(?:", modifier_pattern!(), r")*",
            r"(?:e\s*s\s*c(?:\s*a\s*p\s*e)?|e\s*n\s*t\s*e\s*r|r\s*e\s*t\s*u\s*r\s*n|s\s*p\s*a\s*c\s*e|t\s*a\s*b|b\s*a\s*c\s*k\s*s\s*p\s*a\s*c\s*e|d\s*e\s*l\s*e\s*t\s*e|i\s*n\s*s(?:\s*e\s*r\s*t)?|h\s*o\s*m\s*e|e\s*n\s*d|p\s*(?:a\s*g\s*e|g)\s*(?:u\s*p|d\s*(?:o\s*w\s*n|n))|u\s*p|d\s*o\s*w\s*n|l\s*e\s*f\s*t|r\s*i\s*g\s*h\s*t|f\s*[1-9]\s*[0-9]?|↑↓|[\p{L}\p{N}\x21-\x2f\x3a-\x40\x5b-\x60\x7b-\x7e⎋⏎⇥⌫⌦␣↑↓←→\x{f12b7}\x{f0311}\x{f0312}\x{f006e}\x{f0e7e}\x{f1050}])"
        )
    };
}

struct Patterns {
    key: Regex,
    modifiers: Regex,
    duration: Regex,
    hidden: Regex,
    card_heading: Regex,
    card_caption: Regex,
    maintenance: Regex,
}

static PATTERNS: LazyLock<Patterns> = LazyLock::new(|| {
    Patterns {
        key: Regex::new(concat!(r"(?i)^", key_pattern!(), r"(?:/", key_pattern!(), r")*(?:\s+|$)")).unwrap(),
        modifiers: Regex::new(concat!(r"(?i)^(?:", modifier_pattern!(), r")+$")).unwrap(),
        duration: Regex::new(r"^(?:0|[1-9][0-9]*)[smh]$").unwrap(),
        hidden: Regex::new(r"^Ask(?: \([0-9]+s\))? hidden:").unwrap(),
        card_heading: Regex::new(r"#[1-9][0-9]*").unwrap(),
        card_caption: Regex::new(r"(?:[0-9]+x[0-9]+|\+[0-9]+ (?:lines|chars))").unwrap(),
        maintenance: Regex::new(r"(?ix)^(?:retrying\s+\(\s*\d+\s*/\s*\d+\s*\)\s+in\s+(?:\d+(?:\.\d+)?\s*(?:ms|s|m|h|d)\s*)+|(?:(?:context\s+overflow\s+detected,\s+|response\s+incomplete,\s+|idle\s+)?(?:auto\s+server\s+compaction|auto-handoff|auto-shake|auto-snapcompact|auto\s+context-full\s+maintenance)))[….]+\s*$").unwrap(),
}
});

fn key_hint(key: &str) -> bool {
    let key = key.trim();
    PATTERNS
        .key
        .find(key)
        .is_some_and(|matched| matched.end() == key.len())
}

fn key_message(line: &str) -> Option<&str> {
    let line = line.trim_start();
    PATTERNS
        .key
        .find(line)
        .map(|matched| &line[matched.end()..])
}

fn key_led(line: &str) -> bool {
    key_message(line).is_some()
}

fn horizontal(line: &str) -> bool {
    line.starts_with("──") || line.starts_with("--")
}

fn side(line: &str) -> bool {
    line.starts_with('│') || line.starts_with('|')
}

fn divider(line: &str) -> bool {
    line.starts_with('├')
        || (line.starts_with('+')
            && line.trim_end().ends_with('+')
            && line.chars().all(|c| matches!(c, '+' | '-' | '=' | ' ')))
}

fn box_top(line: &str) -> bool {
    (line.starts_with('╭') && line.trim_end().ends_with('╮'))
        || (line.starts_with("+-") && line.trim_end().ends_with('+'))
}

fn box_bottom(line: &str) -> bool {
    (line.starts_with("╰─") && line.trim_end().ends_with('╯'))
        || (line.starts_with("+-") && line.trim_end().ends_with('+'))
}

fn parked_pi(line: &str) -> bool {
    line.split_whitespace().any(|word| {
        matches!(word, "π" | "pi" | "\u{f0d57}")
            || word.starts_with("π-")
            || word.starts_with("pi-")
            || word.starts_with("\u{f0d57}-")
    })
}

fn timed_pi(line: &str) -> bool {
    let mut words = line.split_whitespace();
    let mut previous = words.next().unwrap_or_default();
    for word in words {
        if PATTERNS.duration.is_match(word)
            && !matches!(previous, "⏱" | "\u{f017}" | "◷" | "◴" | "⌛")
            && previous.chars().count() == 1
            && !previous.chars().all(char::is_lowercase)
        {
            return true;
        }
        previous = word;
    }
    false
}

fn status_line(line: &str) -> bool {
    parked_pi(line)
        || timed_pi(line)
        || line.starts_with('\u{e0b6}')
        || (line.starts_with(' ')
            && !line.starts_with("  ")
            && line.split_whitespace().count() > 1
            && line.trim() != "to retry"
            && !key_led(line))
}

fn error_caption_start(lines: &[&str], blank_before: &[bool], close: usize) -> Option<usize> {
    if !horizontal(lines[close]) || !lines[close].chars().all(|c| matches!(c, '─' | '-' | ' ')) {
        return None;
    }
    let mut expected = DISMISSAL
        .chars()
        .rev()
        .filter(|c| !c.is_whitespace())
        .peekable();
    for index in (0..close).rev() {
        let line = lines[index].trim_end();
        let line = if index + 1 == close {
            line.strip_suffix('.').unwrap_or(line)
        } else {
            line
        };
        for actual in line.chars().rev().filter(|c| !c.is_whitespace()) {
            if !actual.eq_ignore_ascii_case(&expected.next()?) {
                return None;
            }
        }
        if expected.peek().is_none() {
            return Some(index);
        }
        if blank_before[index] {
            return None;
        }
    }
    None
}

fn hard_status(line: &str) -> bool {
    parked_pi(line) || timed_pi(line) || line.starts_with('\u{e0b6}')
}

fn composer_at(lines: &[&str], blank_before: &[bool], index: usize) -> Option<Composer> {
    let line = lines[index];
    if box_bottom(line) {
        let mut start = index;
        while start > 0 && (side(lines[start - 1]) || divider(lines[start - 1])) {
            start -= 1;
        }
        let preceding_top = start > 0 && box_top(lines[start - 1]);
        let complete = preceding_top || (start < index && box_top(lines[start]));
        if preceding_top {
            start -= 1;
        }
        if complete || start < index {
            return Some(Composer {
                start,
                end: index,
                body: Body::Box,
                slot: complete.then_some(start),
                clipped: !complete,
            });
        }
    }
    let (start, mut end, body, slot) = if line.starts_with("╰─") {
        let slot = index
            .checked_sub(1)
            .filter(|&i| status_line(lines[i]) && (hard_status(lines[i]) || !blank_before[i]));
        (index, index, Body::Band, slot)
    } else if line.starts_with('❯') {
        let start = index
            .checked_sub(1)
            .filter(|&i| {
                horizontal(lines[i])
                    && !blank_before[index]
                    && error_caption_start(lines, blank_before, i).is_none()
            })
            .unwrap_or(index);
        (start, index, Body::Prompt, None)
    } else if line.starts_with('▐') || line.starts_with('▎') {
        let prefix = line.chars().next().unwrap();
        if index > 0 && lines[index - 1].starts_with(prefix) {
            return None;
        }
        let body = if prefix == '▐' {
            Body::Field
        } else {
            Body::Rail
        };
        (index, index, body, None)
    } else if horizontal(line) {
        if error_caption_start(lines, blank_before, index).is_some() {
            return None;
        }
        let start = (0..index).rev().find(|&i| horizontal(lines[i]))?;
        if lines[start + 1..index].iter().any(|line| {
            line.starts_with('╭')
                || line.starts_with("╰─")
                || line.starts_with('▐')
                || line.starts_with('▎')
        }) {
            return None;
        }
        return Some(Composer {
            start,
            end: index,
            body: Body::Rules,
            slot: (index + 1 < lines.len() && status_line(lines[index + 1])).then_some(index + 1),
            clipped: false,
        });
    } else {
        return None;
    };
    while end + 1 < lines.len() {
        let next = lines[end + 1];
        let continues = match body {
            Body::Band => next.starts_with("   "),
            Body::Prompt => next.starts_with("  "),
            Body::Field => next.starts_with('▐'),
            Body::Rail => next.starts_with('▎'),
            _ => false,
        };
        if !continues {
            break;
        }
        end += 1;
    }
    let mut slot = slot;
    if !matches!(body, Body::Band) {
        slot = (end + 1 < lines.len() && status_line(lines[end + 1])).then_some(end + 1);
    }
    Some(Composer {
        start,
        end,
        body,
        slot,
        clipped: false,
    })
}

fn card_start(lines: &[&str], end: usize) -> Option<usize> {
    let start = end.checked_sub(5)?;
    let heading = lines[start].trim_end();
    let caption = lines[end].trim_end();
    let rounded = heading.starts_with('╭')
        && heading.ends_with('╮')
        && caption.starts_with('╰')
        && caption.ends_with('╯');
    let ascii = heading.starts_with("+-")
        && heading.ends_with('+')
        && caption.starts_with('+')
        && caption.ends_with('+');
    if !rounded && !ascii {
        return None;
    }
    let width = heading.width();
    let joins = if rounded { "╮  ╭" } else { "+  +" };
    (width >= 14
        && (width + 2).is_multiple_of(16)
        && heading.matches(joins).count() == (width + 2) / 16 - 1
        && PATTERNS.card_heading.is_match(heading)
        && PATTERNS.card_caption.is_match(caption)
        && lines[start + 1..end]
            .iter()
            .all(|line| side(line) && line.trim_end().width() == width))
    .then_some(start)
}

fn rail_body(line: &str) -> &str {
    line.trim_start_matches(['│', '|'])
        .trim_end_matches(['│', '|', ' '])
        .trim()
}

fn find_composer(lines: &[&str], blank_before: &[bool], before: usize) -> Option<Composer> {
    (0..before).rev().find_map(|index| {
        if card_start(lines, index).is_some() {
            None
        } else {
            composer_at(lines, blank_before, index)
        }
    })
}

fn history_footer(lines: &[&str], composer: Composer) -> Option<usize> {
    let start = composer.start;
    let end = composer.end;
    if !matches!(composer.body, Body::Box)
        || composer.clipped
        || end < start + 8
        || !lines[start + 1..end]
            .iter()
            .all(|line| side(line) && line.trim_end().ends_with(['│', '|']))
        || !rail_body(lines[start + 1]).is_empty()
        || !rail_body(lines[start + 3]).is_empty()
        || !rail_body(lines[end - 1]).is_empty()
    {
        return None;
    }
    let input = rail_body(lines[start + 2]);
    if input != ">" && !input.starts_with("> ") {
        return None;
    }
    let separator = (start + 5..end - 1)
        .rev()
        .find(|&i| rail_body(lines[i]).is_empty())?;
    let results = &lines[start + 4..separator];
    let selected = results.iter().any(|line| {
        let body = rail_body(line);
        body.starts_with("❯ ") || body.starts_with("\u{f054} ") || body.starts_with("> ")
    });
    let empty = results.len() == 1
        && ["ⓘ ", "\u{f129} ", "[i] "]
            .iter()
            .any(|prefix| rail_body(results[0]).starts_with(prefix));
    if !selected && !empty {
        return None;
    }
    let mut tokens = lines[separator + 1..end - 1]
        .iter()
        .flat_map(|line| rail_body(line).split_whitespace());
    let modifiers = |text: &str| PATTERNS.modifiers.is_match(text);
    for (index, action) in ["navigate", "select", "cancel"].into_iter().enumerate() {
        let mut key = tokens.next()?;
        while !key_hint(key) {
            if !modifiers(key)
                && !key
                    .rsplit_once('/')
                    .is_some_and(|(prefix, suffix)| key_hint(prefix) && modifiers(suffix))
            {
                return None;
            }
            key = tokens.next()?;
        }
        if tokens.next()? != action {
            return None;
        }
        if index < 2 && !matches!(tokens.next()?, "·" | "-") {
            return None;
        }
    }
    tokens.next().is_none().then_some(end - 2)
}

fn dialog_footer(lines: &[&str], composer: Composer) -> Option<usize> {
    if !matches!(composer.body, Body::Box)
        || !lines[composer.end]
            .trim_end()
            .chars()
            .all(|c| matches!(c, '╰' | '─' | '╯' | '+' | '-'))
        || lines[composer.start..composer.end].iter().any(|line| {
            line.strip_prefix('│')
                .or_else(|| line.strip_prefix('|'))
                .is_some_and(|body| !body.trim_end().ends_with(['│', '|']))
        })
        || composer.slot.is_some_and(|i| {
            (lines[i].starts_with("╭── ") || lines[i].starts_with("+-- "))
                && (parked_pi(lines[i]) || timed_pi(lines[i]))
        })
    {
        return None;
    }
    let has_selection = lines[composer.start..=composer.end].iter().any(|line| {
        let body = rail_body(line);
        body.starts_with("❯ ") || body.starts_with("\u{f054} ") || body.starts_with("> ")
    });
    (composer.start..composer.end).rev().find(|&i| {
        if !side(lines[i]) {
            return false;
        }
        let body = rail_body(lines[i]);
        let body = body
            .strip_prefix('[')
            .and_then(|inner| inner.strip_suffix(']'))
            .map(str::trim)
            .unwrap_or(body);
        let keyed = key_led(body);
        let separate_footer = i > composer.start && i + 1 == composer.end && divider(lines[i - 1]);
        let action = body.contains("submit")
            || ((has_selection || separate_footer)
                && (body.contains("select") || body.contains("toggle")))
            || (has_selection
                && (body.contains("confirm")
                    || body.contains("open")
                    || body.contains("scroll")
                    || body.contains("navigate")));
        (keyed
            && action
            && (body.contains("cancel")
                || body.contains('…')
                || body.split_whitespace().next_back().is_some_and(|key| {
                    matches!(key, "⎋" | "\u{f12b7}")
                        || key.eq_ignore_ascii_case("esc")
                        || key.eq_ignore_ascii_case("escape")
                })))
            || (keyed && body.contains("to paste inline") && has_selection)
            || (body.starts_with("Finish or clear the current prompt to answer")
                && body.contains("cancel"))
    })
}

fn mask_composer(lines: &mut [&str], composer: Composer, footer: Option<usize>) {
    if composer.clipped && footer.is_none() {
        return;
    }
    for (i, line) in lines
        .iter_mut()
        .enumerate()
        .take(composer.end + 1)
        .skip(composer.start)
    {
        if Some(i) == footer || Some(i) == composer.slot {
            continue;
        }
        let body = rail_body(line);
        if footer.is_some() && side(line) && !body.is_empty() {
            *line = if body.starts_with("❯ ")
                || body.starts_with("\u{f054} ")
                || body.starts_with("> ")
            {
                "│ ❯ · │"
            } else if line.trim_start_matches(['│', '|']).starts_with("  ") {
                "│   · │"
            } else {
                "│ · │"
            };
            continue;
        }
        *line = match composer.body {
            Body::Box if i == composer.end => "╰─╯",
            Body::Box if i == composer.start && !composer.clipped => continue,
            Body::Box => "│ │",
            Body::Band if i == composer.start => "╰─",
            Body::Prompt if i == composer.start && horizontal(line) => continue,
            Body::Prompt => "❯",
            Body::Rules if i == composer.start || i == composer.end => continue,
            Body::Rules => "│",
            Body::Band => "│",
            Body::Field => "▐",
            Body::Rail => "▎",
        };
    }
}

fn ui_boundary(line: &str) -> bool {
    horizontal(line)
        || side(line)
        || box_top(line)
        || box_bottom(line)
        || line.starts_with("╰─")
        || line.starts_with('▐')
        || line.starts_with('▎')
        || PATTERNS.hidden.is_match(line)
}

// Rows still borrow this capture; take spans before masking replaces any row.
fn source_span<'a>(source: &'a str, lines: &[&str], start: usize, end: usize) -> &'a str {
    let base = source.as_ptr().addr();
    let first = lines[start].as_ptr().addr() - base;
    let last = lines[end - 1];
    &source[first..last.as_ptr().addr() - base + last.len()]
}

fn error_diagnostic<'a>(
    source: &'a str,
    lines: &[&str],
    blank_before: &[bool],
    caption: usize,
) -> Option<&'a str> {
    let mut start = caption;
    let mut bordered = false;
    while let Some(index) = start.checked_sub(1) {
        let line = lines[index].trim();
        if horizontal(line) && line.chars().all(|c| matches!(c, '─' | '-' | ' ')) {
            // Another banner's closing border cannot open this message.
            if error_caption_start(lines, blank_before, index).is_some() {
                return None;
            }
            bordered = true;
            break;
        }
        if ui_boundary(line) || hard_status(line) || line.starts_with('❯') {
            return None;
        }
        start = index;
    }
    if start == caption
        || (!bordered && blank_before[start + 1..caption].iter().any(|blank| *blank))
    {
        return None;
    }
    let mut body = source_span(source, lines, start, caption).trim();
    // Only this adapter knows the theme's error icon; common presentation keeps source text.
    for glyph in ["✖", "✘", "✕", "[!!]", "\u{f00d}"] {
        if let Some(rest) = body.strip_prefix(glyph) {
            body = rest.trim_start();
            break;
        }
    }
    (!body.is_empty()).then_some(body)
}

fn spinner_led(text: &str) -> bool {
    let Some(frame) = text.split_whitespace().next() else {
        return false;
    };
    matches!(frame, "-" | "\\" | "|" | "/")
        || (!key_hint(frame)
            && frame.chars().all(|c| {
                !c.is_alphanumeric()
                    && !c.is_whitespace()
                    && !matches!(
                        c,
                        '•' | '\u{f111}' | '※' | '❯' | '\u{f054}' | '│' | '┃' | '▐' | '▎'
                    )
            }))
}

fn cancellation_key(text: &str) -> Option<&str> {
    let (_, suffix) = text.trim_end().strip_suffix(')')?.rsplit_once('(')?;
    let mut expected = "to cancel"
        .chars()
        .rev()
        .filter(|c| !c.is_whitespace())
        .peekable();
    for (index, actual) in suffix
        .char_indices()
        .rev()
        .filter(|(_, c)| !c.is_whitespace())
    {
        if Some(actual) != expected.next() {
            return None;
        }
        if expected.peek().is_none() {
            return Some(suffix[..index].trim());
        }
    }
    None
}

fn maintenance(text: &str) -> bool {
    if !spinner_led(text) {
        return false;
    }
    if cancellation_key(text).is_some_and(|key| key.is_empty() || key_hint(key)) {
        return true;
    }
    let message = text
        .trim_start()
        .split_once(char::is_whitespace)
        .map_or("", |(_, rest)| rest.trim());
    PATTERNS.maintenance.is_match(message)
}

impl<'a> Surface<'a> {
    fn new(source: &'a str, lines: &mut [&'a str], blank_before: &[bool]) -> Self {
        let current = find_composer(lines, blank_before, lines.len());
        let history = current.and_then(|composer| history_footer(lines, composer));
        let local_history = history.map_or_else(Evidence::default, |i| line_evidence(lines, i));
        let mut dialog = current
            .filter(|_| history.is_none())
            .and_then(|composer| dialog_footer(lines, composer).map(|footer| (composer, footer)));
        let mut before = current.map_or(0, |composer| composer.start);
        while history.is_none() && dialog.is_none() {
            let Some(composer) = find_composer(lines, blank_before, before) else {
                break;
            };
            if history_footer(lines, composer).is_some() {
                mask_composer(lines, composer, None);
            } else {
                dialog = dialog_footer(lines, composer).map(|footer| (composer, footer));
            }
            before = composer.start;
        }
        let error = current
            .and_then(|composer| {
                (0..composer.start).rev().find_map(|close| {
                    error_caption_start(lines, blank_before, close).map(|caption| (close, caption))
                })
            })
            .map_or_else(Evidence::default, |(close, caption)| {
                let mut evidence = line_evidence(lines, close);
                evidence.diagnostic = error_diagnostic(source, lines, blank_before, caption);
                evidence
            });
        let mut activity = Evidence::default();
        let mut activity_start = None;
        let mut cards = None;
        if let Some(composer) = current {
            let mut boundary = composer
                .slot
                .filter(|&i| i < composer.start)
                .unwrap_or(composer.start);
            let cards_end = boundary;
            while let Some(start) = boundary.checked_sub(1).and_then(|i| card_start(lines, i)) {
                boundary = start;
            }
            if boundary < cards_end {
                cards = Some(boundary..cards_end);
            }
            let parked = composer.slot.is_some_and(|i| parked_pi(lines[i]));
            let timed = composer.slot.is_some_and(|i| timed_pi(lines[i]));
            for (candidate, end) in [boundary, composer.start].into_iter().enumerate() {
                if end == 0 || (candidate == 1 && end == boundary) {
                    continue;
                }
                let mut start = end - 1;
                while start > 0 && !blank_before[start] {
                    start -= 1;
                }
                if lines[start..end].iter().any(|line| ui_boundary(line)) {
                    continue;
                }
                let text = source_span(source, lines, start, end);
                let explicit_maintenance = maintenance(text);
                let regular = !explicit_maintenance
                    && !parked
                    // The interrupt frame and padding each add one column.
                    && (text
                        .strip_prefix("  ")
                        .is_some_and(|key| !key.starts_with(char::is_whitespace))
                        || timed)
                    && key_message(text).is_some_and(|message| {
                        let mut words = message.split_whitespace();
                        let first = words.next();
                        let second = words.next();
                        let passive = first == Some("to") && second == Some("retry");
                        let pending = (dialog.is_some() || history.is_some())
                            && first.is_some_and(|word| word.eq_ignore_ascii_case("waiting"))
                            && second.is_none();
                        !passive && !pending
                    });
                if (explicit_maintenance || regular) && (blank_before[start] || timed) {
                    let marker = composer
                        .slot
                        .filter(|&i| i < composer.start && !blank_before[i])
                        .unwrap_or(end - 1)
                        .max(end - 1);
                    activity = line_evidence(lines, marker);
                    activity_start = Some(start);
                    break;
                }
            }
        }
        let dialog = dialog.filter(|(old, footer)| {
            activity_start.is_none_or(|start| start <= *footer)
                && !current.is_some_and(|composer| {
                    composer.end > old.end
                        && composer.slot.is_some_and(|i| parked_pi(lines[i]))
                        && !rail_body(lines[*footer])
                            .starts_with("Finish or clear the current prompt to answer")
                })
        });
        let hidden = (0..lines.len()).rev().find(|&i| {
            PATTERNS.hidden.is_match(lines[i])
                && activity_start.is_none_or(|start| start <= i)
                && current.is_none_or(|composer| {
                    i < composer.start && !blank_before[i + 1..=composer.start].iter().any(|b| *b)
                })
        });
        if history.is_some() {
            // Covered dialogs cannot establish a new request under local navigation.
            let mut before = current.map_or(0, |composer| composer.start);
            while let Some(composer) = find_composer(lines, blank_before, before) {
                if dialog_footer(lines, composer).is_some()
                    || history_footer(lines, composer).is_some()
                {
                    lines[composer.start..=composer.end].fill("│ │");
                }
                before = composer.start;
            }
        }
        if let Some(composer) = current {
            mask_composer(
                lines,
                composer,
                dialog
                    .filter(|(d, _)| d.end == composer.end)
                    .map(|(_, f)| f),
            );
        }
        if let Some((composer, footer)) = dialog {
            mask_composer(lines, composer, Some(footer));
        }
        if let Some(cards) = cards {
            for chunk in lines[cards].chunks_mut(6) {
                for line in &mut chunk[1..5] {
                    *line = "│ │";
                }
            }
        }
        let legacy_error = lines
            .iter()
            .enumerate()
            .skip(lines.len().saturating_sub(6))
            .rev()
            .find(|(_, line)| {
                line.as_bytes()
                    .windows(DISMISSAL.len())
                    .any(|window| window.eq_ignore_ascii_case(DISMISSAL.as_bytes()))
            })
            .map_or_else(Evidence::default, |(i, _)| line_evidence(lines, i));
        Self {
            activity,
            dialog: dialog.map_or_else(Evidence::default, |(_, footer)| {
                line_evidence(lines, footer)
            }),
            hidden_ask: hidden.map_or_else(Evidence::default, |i| line_evidence(lines, i)),
            error,
            legacy_error,
            local_history,
        }
    }
}
