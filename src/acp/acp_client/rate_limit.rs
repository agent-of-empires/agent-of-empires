//! Rate-limit classification and the captured rejected-window reset time.

use crate::acp::state::RateLimitInfo;
use std::collections::HashMap;

/// The rejected window that clears last, which is the one the park waits for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CapturedReset {
    /// `rateLimitType`, or empty when the report named none.
    pub(crate) window: String,
    pub(crate) resets_at: crate::time::Timestamp,
}

impl CapturedReset {
    /// The window names the limit, so reports for different windows of one account stay apart.
    fn kind(&self) -> String {
        if self.window.is_empty() {
            "rate_limit".to_string()
        } else {
            self.window.clone()
        }
    }
}

/// `errorKind`s the adapter reports for an account-level limit: a quota window
/// (`rate_limit`) or a spend cap or empty credit balance (`billing_error`).
/// `overloaded` and `server_error` are capacity trouble that another account
/// meets just the same, so they stay plain errors.
const ACCOUNT_LIMIT_KINDS: &[&str] = &["rate_limit", "billing_error"];

/// The status prose and `errorKind` of a flattened error message, split at the JSON object that
/// runs to its end, which is the error's own `data`; braces in the prose are not. Without a
/// string `errorKind` there it is `None`: a nested `cause.errorKind` is never the top level.
fn flattened_error(message: &str) -> Option<(&str, String)> {
    let (at, data): (usize, serde_json::Value) =
        message.match_indices('{').find_map(|(at, _)| {
            let data = serde_json::from_str(&message[at..]).ok()?;
            Some((at, data))
        })?;
    let kind = data.get("errorKind")?.as_str()?.to_string();
    Some((
        message[..at].trim_end_matches(|c: char| c == ':' || c.is_whitespace()),
        kind,
    ))
}

/// A captured rejected-window epoch is the authority on the reset; without one
/// the reset and window come from the status text (`refine_from_status`).
pub(crate) fn classify_rate_limit_error(
    err: &agent_client_protocol::Error,
    captured: Option<&CapturedReset>,
    now: crate::time::Timestamp,
) -> Option<RateLimitInfo> {
    // Present data is authoritative; only a flattened error may use the message.
    let Some(data) = err.data.as_ref() else {
        return classify_rate_limit_from_message(&err.message, captured, now);
    };
    let kind = data.get("errorKind")?.as_str()?;
    let kind = *ACCOUNT_LIMIT_KINDS.iter().find(|k| **k == kind)?;
    Some(limit_info(
        super::errors::public_error_message(err),
        kind,
        captured,
        now,
    ))
}

/// Fallback for an outer error that kept only the serialized `errorKind`.
fn classify_rate_limit_from_message(
    message: &str,
    captured: Option<&CapturedReset>,
    now: crate::time::Timestamp,
) -> Option<RateLimitInfo> {
    let (prose, kind) = flattened_error(message)?;
    let kind = *ACCOUNT_LIMIT_KINDS.iter().find(|k| **k == kind)?;
    let status = super::spawn::scrub_stderr_secrets(prose);
    let status = if status.len() > 2048 {
        super::delete::truncate_for_log(&status, 2045)
    } else {
        status.into_owned()
    };
    Some(limit_info(status, kind, captured, now))
}

fn limit_info(
    status: String,
    kind: &str,
    captured: Option<&CapturedReset>,
    now: crate::time::Timestamp,
) -> RateLimitInfo {
    // A captured window is a quota window's: a billing error keeps its own kind and takes no reset.
    let captured = captured.filter(|_| kind == "rate_limit");
    refine_from_status(
        RateLimitInfo {
            status,
            resets_at: captured.map(|c| c.resets_at),
            kind: captured.map_or_else(|| kind.to_string(), |c| c.kind()),
        },
        now,
        Observation::Live,
    )
}

/// What the reference time of a status reading is. A live report is read at the moment it was
/// seen. A stored event's recorded time can follow its reset (a delayed write), and a reset
/// that names no day ("resets 6:30pm") then rolls to the next day, so only a reset that names
/// its date is read against one; any other keeps the timed schedule.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Observation {
    Live,
    Recorded,
}

/// Fills the reset and window a report left out from its status text, e.g.
/// "You've hit your weekly limit · resets Nov 18, 12am (Europe/Berlin)", read
/// relative to `now`, the time the limit was seen (`observation`).
pub(crate) fn refine_from_status(
    mut info: RateLimitInfo,
    now: crate::time::Timestamp,
    observation: Observation,
) -> RateLimitInfo {
    let beyond_five_hours = |reset: crate::time::Timestamp| {
        reset.as_unix_secs() - now.as_unix_secs() > FIVE_HOUR_WINDOW_SECS
    };
    // A stored status may still carry the serialized error data behind its prose.
    let prose = flattened_error(&info.status).map_or(info.status.as_str(), |(prose, _)| prose);
    if info.kind == "rate_limit" {
        // A captured reset a five-hour window cannot reach keeps the generic kind.
        let incompatible = status_window(prose) == Some("five_hour")
            && info.resets_at.is_some_and(beyond_five_hours);
        if let Some(window) = status_window(prose).filter(|_| !incompatible) {
            info.kind = window.to_string();
        }
    }
    if info.resets_at.is_none() {
        info.resets_at = status_reset(prose, now, observation)
            .filter(|reset| info.kind != "five_hour" || !beyond_five_hours(*reset));
        if info.resets_at.is_none() && prose.contains("resets ") {
            tracing::debug!(
                target: "acp.protocol",
                status = %prose,
                "usage-limit status names a reset that could not be read; keeping the timed schedule"
            );
        }
    }
    info
}

/// A weekly window is the longest the status text can name; anything further out is misread.
const MAX_RESET_AHEAD_SECS: i64 = 8 * 24 * 3600;

/// A reset this close behind `now` is a window that just cleared (or a write that lagged the
/// report), not one that rolls to tomorrow.
const JUST_ELAPSED_SECS: i64 = 600;

/// The two instants of a repeated local time lie at most this far apart.
const FOLD_SPAN_SECS: i64 = 2 * 3600;

/// A five-hour window cannot name a reset further out than this.
pub(crate) const FIVE_HOUR_WINDOW_SECS: i64 = 5 * 3600;

/// The quota window a status line names, as the adapter's `rateLimitType` would.
fn status_window(status: &str) -> Option<&'static str> {
    let status = status.to_ascii_lowercase();
    if status.contains("weekly limit") {
        Some("seven_day")
    } else if status.contains("5-hour limit") || status.contains("session limit") {
        Some("five_hour")
    } else {
        None
    }
}

/// The next instant matching "resets [Mon D, ]H[:MM]am|pm (Zone)" after `now`,
/// read in the stated IANA zone. `None` when the text is anything else, when the
/// reset just passed, when it lies beyond a weekly window, or when it names no date and `now`
/// is only a stored event's recorded time.
fn status_reset(
    status: &str,
    now: crate::time::Timestamp,
    observation: Observation,
) -> Option<crate::time::Timestamp> {
    let rest = &status[status.rfind("resets ")? + "resets ".len()..];
    let (when, zone) = rest.split_once(" (")?;
    let tz = jiff::tz::TimeZone::get(zone.split(')').next()?).ok()?;
    let (date, clock) = when
        .split_once(", ")
        .or_else(|| when.split_once(" at "))
        .map_or((None, when), |(date, clock)| (Some(date), clock));
    if date.is_none() && observation == Observation::Recorded {
        return None;
    }
    let (hour, minute) = parse_clock(clock)?;
    let now = jiff::Timestamp::from_second(now.as_unix_secs()).ok()?;
    let local_today = now.to_zoned_datetime(tz.clone()).date();
    let days: Vec<jiff::civil::Date> = match date {
        // Yesterday only serves the just-elapsed check.
        None => [
            local_today.yesterday().ok(),
            Some(local_today),
            local_today.tomorrow().ok(),
        ]
        .into_iter()
        .flatten()
        .collect(),
        Some(date) => {
            let (month, day) = parse_month_day(date)?;
            [
                local_today.year() - 1,
                local_today.year(),
                local_today.year() + 1,
            ]
            .into_iter()
            .filter_map(|year| jiff::civil::Date::new(year, month, day).ok())
            .collect()
        }
    };
    // A repeated local time (clocks going back) is both of its instants; one the clocks skip
    // (going forward) never happens.
    let instants: Vec<jiff::Timestamp> = days
        .iter()
        .flat_map(|day| {
            let at = tz.to_ambiguous_timestamp(day.at(hour, minute, 0, 0));
            let skipped = matches!(at.offset(), jiff::tz::AmbiguousOffset::Gap { .. });
            [at.earlier(), at.later()].map(|t| t.ok().filter(|_| !skipped))
        })
        .flatten()
        .collect();
    let behind = |t: &jiff::Timestamp| now.as_second() - t.as_second();
    let future = instants.iter().copied().filter(|t| *t > now).min();
    if let Some(elapsed) = instants
        .iter()
        .filter(|t| (0..JUST_ELAPSED_SECS).contains(&behind(t)))
        .max()
    {
        // Only the other instant of a repeated local time can still be ahead of one just gone.
        let twin = |f: &jiff::Timestamp| f.as_second() - elapsed.as_second() <= FOLD_SPAN_SECS;
        if !future.as_ref().is_some_and(twin) {
            return None;
        }
    }
    let reset = future?;
    (reset.as_second() - now.as_second() <= MAX_RESET_AHEAD_SECS)
        .then(|| crate::time::Timestamp::from_unix_secs(reset.as_second()))?
}

/// "6pm", "6:30pm", "12am" as 24-hour `(hour, minute)`.
fn parse_clock(clock: &str) -> Option<(i8, i8)> {
    let clock = clock.trim().to_ascii_lowercase();
    let (digits, pm) = match (clock.strip_suffix("am"), clock.strip_suffix("pm")) {
        (Some(digits), _) => (digits, false),
        (_, Some(digits)) => (digits, true),
        _ => return None,
    };
    let (hour, minute) = digits.split_once(':').unwrap_or((digits, "0"));
    let (hour, minute): (i8, i8) = (hour.parse().ok()?, minute.parse().ok()?);
    ((1..=12).contains(&hour) && (0..60).contains(&minute))
        .then_some((hour % 12 + if pm { 12 } else { 0 }, minute))
}

fn parse_month_day(date: &str) -> Option<(i8, i8)> {
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let (month, day) = date.trim().split_once(' ')?;
    let month = month.get(..3)?.to_ascii_lowercase();
    let month = MONTHS.iter().position(|m| *m == month)? as i8 + 1;
    Some((month, day.trim().parse().ok()?))
}

/// A rejected window from a `usage_update`'s `_meta["_claude/rateLimit"]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RateLimitRejection {
    /// `rateLimitType`, or empty; keys captures so windows cannot overwrite
    /// each other.
    pub(super) window: String,
    pub(super) resets_at_secs: i64,
}

/// Extra usage keeps serving turns once the subscription window rejects.
fn overage_covers(info: &serde_json::Value) -> bool {
    matches!(
        info.get("overageStatus").and_then(|s| s.as_str()),
        Some("allowed" | "allowed_warning")
    )
}

/// Only rejections are retained: a warning's epoch cannot be tied to the
/// window that later rejects (#3152).
pub(super) fn rate_limit_rejection_from_meta(
    meta: &Option<agent_client_protocol::v1::schema::Meta>,
) -> Option<RateLimitRejection> {
    let info = meta.as_ref()?.get("_claude/rateLimit")?;
    if info.get("status")?.as_str()? != "rejected" || overage_covers(info) {
        return None;
    }
    let v = info.get("resetsAt")?;
    let raw = v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))?;
    if raw <= 0 {
        return None;
    }
    // Seconds past ~year 5138 are really milliseconds.
    let resets_at_secs = if raw > 100_000_000_000 {
        raw / 1000
    } else {
        raw
    };
    Some(RateLimitRejection {
        window: info
            .get("rateLimitType")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        resets_at_secs,
    })
}

pub(super) fn recovered_rate_limit_window(
    meta: &Option<agent_client_protocol::v1::schema::Meta>,
) -> Option<&str> {
    let info = meta.as_ref()?.get("_claude/rateLimit")?;
    (overage_covers(info)
        || info
            .get("status")
            .and_then(|status| status.as_str())
            .is_some_and(|status| status != "rejected"))
    .then(|| {
        info.get("rateLimitType")
            .and_then(|window| window.as_str())
            .unwrap_or_default()
    })
}

/// Records a rejected window and drops one a later report shows has recovered,
/// so a stale rejection cannot park a turn. Returns whether a rejection was kept.
pub(super) fn record_rate_limit_report(
    captures: &std::sync::Mutex<HashMap<String, i64>>,
    meta: &Option<agent_client_protocol::v1::schema::Meta>,
) -> bool {
    if meta
        .as_ref()
        .and_then(|meta| meta.get("_claude/rateLimit"))
        .is_none()
    {
        return false;
    }
    let mut captures = captures.lock().expect("rate-limit capture mutex poisoned");
    if let Some(r) = rate_limit_rejection_from_meta(meta) {
        captures.insert(r.window, r.resets_at_secs);
        return true;
    }
    if let Some(window) = recovered_rate_limit_window(meta) {
        captures.remove(window);
    }
    false
}

/// The window with the latest future reset, since every rejected window must clear.
pub(super) fn latest_future_reset<'a>(
    captures: impl Iterator<Item = (&'a String, &'a i64)>,
    now: crate::time::Timestamp,
) -> Option<CapturedReset> {
    captures
        .filter_map(|(window, &secs)| {
            let resets_at =
                crate::time::Timestamp::from_millis_saturating(secs.saturating_mul(1000))?;
            Some((
                secs,
                CapturedReset {
                    window: window.clone(),
                    resets_at,
                },
            ))
        })
        .filter(|(_, captured)| captured.resets_at > now)
        .max_by_key(|(secs, _)| *secs)
        .map(|(_, captured)| captured)
}

pub(super) fn captured_rate_limit_reset(
    captures: &std::sync::Mutex<HashMap<String, i64>>,
    now: crate::time::Timestamp,
) -> Option<CapturedReset> {
    latest_future_reset(
        captures
            .lock()
            .expect("rate-limit capture mutex poisoned")
            .iter(),
        now,
    )
}

/// A turn nobody prompted ends without an error, so a still-future rejected
/// window is the only evidence it ended on the limit. Drains the captures under
/// one lock so a window recorded meanwhile is never cleared unobserved.
pub(super) fn take_rate_limit_from_captured_rejection(
    captures: &std::sync::Mutex<HashMap<String, i64>>,
    now: crate::time::Timestamp,
) -> Option<RateLimitInfo> {
    let drained = std::mem::take(&mut *captures.lock().expect("rate-limit capture mutex poisoned"));
    latest_future_reset(drained.iter(), now).map(|captured| RateLimitInfo {
        status: "usage limit reached".to_string(),
        resets_at: Some(captured.resets_at),
        kind: captured.kind(),
    })
}

/// The agent text of the current turn while it has no prompt and ran no tool,
/// so a limit reported only as text can park it (#5990). `None` once the turn
/// is disqualified; each terminal starts it over.
pub(super) struct AgentTurnText(Option<String>);

impl Default for AgentTurnText {
    fn default() -> Self {
        Self(Some(String::new()))
    }
}

impl AgentTurnText {
    pub(super) fn observe(&mut self, event: &crate::acp::Event) {
        use crate::acp::Event;
        match event {
            Event::AgentMessageChunk { text, .. } => {
                if let Some(turn) = &mut self.0 {
                    turn.push_str(if turn.is_empty() {
                        text.trim_start()
                    } else {
                        text
                    });
                    // Bytes, so the check is constant time: a limit sentence
                    // fits in four bytes a char, plus a little trailing space.
                    if turn.len() > RateLimitInfo::AGENT_TEXT_MAX_CHARS * 4 + 64 {
                        self.disqualify();
                    }
                }
            }
            Event::UserPromptSent { .. }
            | Event::UserDiffCommentsPrompt { .. }
            | Event::ToolCallStarted { .. } => self.disqualify(),
            Event::Stopped { .. } => *self = Self::default(),
            _ => {}
        }
    }

    pub(super) fn disqualify(&mut self) {
        self.0 = None;
    }

    /// The limit the turn's only text reports, starting the next turn over.
    pub(super) fn take_rate_limit(&mut self) -> Option<RateLimitInfo> {
        std::mem::take(self)
            .0
            .and_then(|text| RateLimitInfo::from_agent_text(&text))
    }
}

/// Captures belong to the turn that observed them, so each turn boundary drops them.
pub(super) fn clear_captured_rejections(captures: &std::sync::Mutex<HashMap<String, i64>>) {
    captures
        .lock()
        .expect("rate-limit capture mutex poisoned")
        .clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn err(message: &str, data: Option<serde_json::Value>) -> agent_client_protocol::Error {
        let mut err = agent_client_protocol::Error::internal_error();
        err.message = message.into();
        err.data = data;
        err
    }

    /// Monday 2026-11-16 10:00 UTC, which is 11:00 in Europe/Berlin.
    fn now() -> crate::time::Timestamp {
        crate::time::Timestamp::parse_rfc3339("2026-11-16T10:00:00Z").unwrap()
    }

    fn utc(s: &str) -> Option<crate::time::Timestamp> {
        Some(crate::time::Timestamp::parse_rfc3339(s).unwrap())
    }

    /// The reset a status line names, read in the zone it states.
    #[test]
    fn status_reset_reads_the_stated_zone() {
        let weekly = "Internal error: You've hit your weekly limit";
        let cases = [
            (format!("{weekly} · resets Nov 18, 12am (Europe/Berlin)"), utc("2026-11-17T23:00:00Z")),
            (format!("{weekly} · resets 12am (Europe/Berlin)"), utc("2026-11-16T23:00:00Z")),
            (format!("{weekly} · resets Nov 17, 11pm (UTC)"), utc("2026-11-17T23:00:00Z")),
            ("You've hit your 5-hour limit · resets 6:30pm (Europe/Berlin)".into(), utc("2026-11-16T17:30:00Z")),
            ("You've hit your session limit · resets 3pm (Europe/Berlin)".into(), utc("2026-11-16T14:00:00Z")),
            // A time already past today means tomorrow.
            ("You've hit your session limit · resets 9:20am (Europe/Berlin)".into(), utc("2026-11-17T08:20:00Z")),
            // Further out than a weekly window is a misread, not a reset.
            ("weekly limit · resets Jan 2, 12am (UTC)".into(), None),
            // Wrapped in a flattened connection error.
            (
                format!("ACP connection failed: {weekly} · resets Nov 18, 12am (Europe/Berlin): {{\"errorKind\":\"rate_limit\"}}"),
                utc("2026-11-17T23:00:00Z"),
            ),
            ("usage limit reached".into(), None),
            ("You've hit your weekly limit".into(), None),
            ("weekly limit · resets 6pm".into(), None),
            ("weekly limit · resets 6pm (Mars/Olympus)".into(), None),
            ("weekly limit · resets 25pm (UTC)".into(), None),
            ("weekly limit · resets soon (UTC)".into(), None),
        ];
        for (status, want) in cases {
            assert_eq!(
                status_reset(&status, now(), Observation::Live),
                want,
                "{status}"
            );
        }
    }

    /// Ingestion: the weekly limit arrives as an `errorKind: rate_limit` error with
    /// no captured epoch, and its reset and window come from the status text.
    #[test]
    fn classify_reads_reset_and_window_from_the_status_text() {
        let limit = json!({ "errorKind": "rate_limit" });
        let weekly =
            "Internal error: You've hit your weekly limit · resets Nov 18, 12am (Europe/Berlin)";
        let info =
            classify_rate_limit_error(&err(weekly, Some(limit.clone())), None, now()).unwrap();
        assert_eq!(info.resets_at, utc("2026-11-17T23:00:00Z"));
        assert_eq!(info.kind, "seven_day");

        for (status, kind) in [
            (
                "You've hit your 5-hour limit · resets 6pm (UTC)",
                "five_hour",
            ),
            (
                "You've hit your session limit · resets 6pm (UTC)",
                "five_hour",
            ),
            ("You've hit your limit", "rate_limit"),
        ] {
            let info =
                classify_rate_limit_error(&err(status, Some(limit.clone())), None, now()).unwrap();
            assert_eq!(info.kind, kind, "{status}");
        }

        // A five-hour window cannot reach tomorrow: a stale 9:20am read at 11:00 Berlin is
        // dropped, while a midnight rollover an hour ahead stands.
        let five = |status: &str, now: &str| {
            let now = crate::time::Timestamp::parse_rfc3339(now).unwrap();
            classify_rate_limit_error(&err(status, Some(limit.clone())), None, now)
                .unwrap()
                .resets_at
        };
        assert_eq!(
            five(
                "You've hit your session limit · resets 9:20am (Europe/Berlin)",
                "2026-11-16T10:00:00Z"
            ),
            None
        );
        assert_eq!(
            five(
                "You've hit your 5-hour limit · resets 12:30am (UTC)",
                "2026-11-16T23:30:00Z"
            ),
            utc("2026-11-17T00:30:00Z")
        );

        // The captured epoch outranks the text, and names the window.
        let captured = CapturedReset {
            window: "seven_day_opus".into(),
            resets_at: utc("2026-11-20T00:00:00Z").unwrap(),
        };
        let info =
            classify_rate_limit_error(&err(weekly, Some(limit)), Some(&captured), now()).unwrap();
        assert_eq!(info.resets_at, utc("2026-11-20T00:00:00Z"));
        assert_eq!(info.kind, "seven_day_opus");

        // The flattened fallback reads the same text.
        let flat = format!("ACP connection failed: {weekly}: {{\"errorKind\":\"rate_limit\"}}");
        let info = classify_rate_limit_from_message(&flat, None, now()).unwrap();
        assert_eq!(
            (info.resets_at, info.kind.as_str()),
            (utc("2026-11-17T23:00:00Z"), "seven_day")
        );
    }

    /// A stored report is read against its recorded time, which can follow the reset it names:
    /// a clock-only reset then keeps the timed schedule instead of rolling to tomorrow, while a
    /// reset that names its date reads the same either way.
    #[test]
    fn a_recorded_clock_only_reset_is_not_read_against_the_write_time() {
        let stored = |status: &str| RateLimitInfo {
            status: status.into(),
            resets_at: None,
            kind: "rate_limit".into(),
        };
        // Recorded at 11:00 Berlin; the first text names 9am, which passed two hours before.
        for (status, live, recorded) in [
            (
                "You've hit your limit · resets 9am (Europe/Berlin)",
                utc("2026-11-17T08:00:00Z"),
                None,
            ),
            (
                "You've hit your weekly limit · resets Nov 18, 12am (Europe/Berlin)",
                utc("2026-11-17T23:00:00Z"),
                utc("2026-11-17T23:00:00Z"),
            ),
        ] {
            assert_eq!(
                refine_from_status(stored(status), now(), Observation::Live).resets_at,
                live,
                "live: {status}"
            );
            assert_eq!(
                refine_from_status(stored(status), now(), Observation::Recorded).resets_at,
                recorded,
                "recorded: {status}"
            );
        }
    }

    /// Boundaries of the text reader: a year rollover, a just-cleared reset, a repeated
    /// local hour, and serialized error data trailing the status.
    #[test]
    fn status_reset_edges() {
        let at = |s: &str| crate::time::Timestamp::parse_rfc3339(s).unwrap();
        let cases = [
            // Year rollover within a weekly window.
            (
                "limit · resets Jan 2, 12am (UTC)",
                at("2026-12-30T12:00:00Z"),
                utc("2027-01-02T00:00:00Z"),
            ),
            // A write that lagged the report must not roll the reset a day or a year ahead.
            ("limit · resets 6pm (UTC)", at("2026-11-18T18:00:30Z"), None),
            (
                "limit · resets Nov 18, 6pm (UTC)",
                at("2026-11-18T18:00:30Z"),
                None,
            ),
            // 1:30am occurs twice on 2024-11-03 (05:30Z, then 06:30Z); the later one is still ahead.
            (
                "limit · resets Nov 3, 1:30am (America/New_York)",
                at("2024-11-03T06:00:00Z"),
                utc("2024-11-03T06:30:00Z"),
            ),
            (
                "limit · resets Nov 3, 1:30am (America/New_York)",
                at("2024-11-03T04:00:00Z"),
                utc("2024-11-03T05:30:00Z"),
            ),
            // A clock-only reset that passed on the previous local date is just elapsed too.
            (
                "limit · resets 11:59pm (UTC)",
                at("2026-11-17T00:01:00Z"),
                None,
            ),
            // The earlier 1:30am just passed, the repeated one is still ahead: that one stands.
            (
                "limit · resets Nov 3, 1:30am (America/New_York)",
                at("2024-11-03T05:35:00Z"),
                utc("2024-11-03T06:30:00Z"),
            ),
            // 2:30am does not exist on 2024-03-10 in New York; no instant stands in for it.
            (
                "limit · resets Mar 10, 2:30am (America/New_York)",
                at("2024-03-10T06:00:00Z"),
                None,
            ),
            // Claude Code's `at` form of a dated reset.
            (
                "weekly limit · resets Nov 18 at 4pm (America/Toronto)",
                at("2026-11-16T10:00:00Z"),
                utc("2026-11-18T21:00:00Z"),
            ),
            // Braces in a structured message are part of the status.
            (
                "Weekly limit for {account} · resets Nov 18, 12am (UTC)",
                at("2026-11-16T10:00:00Z"),
                utc("2026-11-18T00:00:00Z"),
            ),
        ];
        for (status, now, want) in cases {
            assert_eq!(
                status_reset(status, now, Observation::Live),
                want,
                "{status} at {now}"
            );
        }
    }

    /// Only the error's own `errorKind` decides; a nested cause cannot turn capacity trouble
    /// into an account limit.
    #[test]
    fn flattened_kind_is_the_top_level_one() {
        let nested = "ACP connection failed: Internal error: busy: {\"errorKind\":\"overloaded\",\"cause\":{\"errorKind\":\"billing_error\"}}";
        assert!(classify_rate_limit_from_message(nested, None, now()).is_none());
        let top = "ACP connection failed: Internal error: low: {\"cause\":{\"errorKind\":\"overloaded\"},\"errorKind\":\"billing_error\"}";
        assert_eq!(
            classify_rate_limit_from_message(top, None, now())
                .unwrap()
                .kind,
            "billing_error"
        );
        assert!(
            classify_rate_limit_from_message("{ not json errorKind rate_limit", None, now())
                .is_none()
        );
        // A JSON object in the prose is not the error data behind it.
        let braced = "ACP connection failed: weekly limit for {\"scope\":\"account\"} · resets Nov 18, 12am (UTC): {\"errorKind\":\"rate_limit\"}";
        let info = classify_rate_limit_from_message(braced, None, now()).unwrap();
        assert_eq!(info.kind, "seven_day");
        assert_eq!(info.resets_at, utc("2026-11-18T00:00:00Z"));
        assert!(info.status.contains("{\"scope\":\"account\"}"));
        // ...nor does an `errorKind` in the prose stand in for the data's.
        let spoofed = "ACP connection failed: odd {\"errorKind\":\"rate_limit\"} text: {\"errorKind\":\"overloaded\"}";
        assert!(classify_rate_limit_from_message(spoofed, None, now()).is_none());
        // Serialized data after the status is not status: its fields never read as the reset.
        let with_detail = "ACP connection failed: weekly limit · resets Nov 18, 12am (UTC): {\"errorKind\":\"rate_limit\",\"detail\":\"quota resets after billing\"}";
        let info = classify_rate_limit_from_message(with_detail, None, now()).unwrap();
        assert_eq!(info.resets_at, utc("2026-11-18T00:00:00Z"));
        assert_eq!(info.kind, "seven_day");
        // A stored status that still carries its serialized data reads only its prose.
        let stored = RateLimitInfo {
            status: "ACP connection failed: weekly limit · resets Nov 18, 12am (UTC): {\"errorKind\":\"rate_limit\",\"detail\":\"Previous window resets Nov 20, 12am (UTC)\"}".into(),
            resets_at: None,
            kind: "rate_limit".into(),
        };
        assert_eq!(
            refine_from_status(stored, now(), Observation::Recorded).resets_at,
            utc("2026-11-18T00:00:00Z")
        );
        // A captured reset a five-hour window cannot reach keeps the generic kind.
        let far = CapturedReset {
            window: String::new(),
            resets_at: utc("2026-11-20T10:00:00Z").unwrap(),
        };
        let info = classify_rate_limit_error(
            &err(
                "You've hit your 5-hour limit · resets 6pm (UTC)",
                Some(json!({ "errorKind": "rate_limit" })),
            ),
            Some(&far),
            now(),
        )
        .unwrap();
        assert_eq!(
            (info.kind.as_str(), info.resets_at),
            ("rate_limit", utc("2026-11-20T10:00:00Z"))
        );
        // JSON inside the message is not the error's data: the structured error decides.
        let structured = err(
            "Limit reached for {\"scope\":\"account\"}",
            Some(json!({ "errorKind": "rate_limit" })),
        );
        assert!(classify_rate_limit_error(&structured, None, now()).is_some());
        let misleading = err(
            r#"limit: {"errorKind":"rate_limit"}"#,
            Some(json!({ "errorKind": "overloaded" })),
        );
        assert!(classify_rate_limit_error(&misleading, None, now()).is_none());
        let flattened = err(r#"limit: {"errorKind":"billing_error"}"#, None);
        assert_eq!(
            classify_rate_limit_error(&flattened, None, now())
                .unwrap()
                .kind,
            "billing_error"
        );
        // Data without its own errorKind ends the search; a nested one is never promoted.
        let cause_only =
            "ACP connection failed: low: {\"cause\":{\"errorKind\":\"billing_error\"}}";
        assert!(classify_rate_limit_from_message(cause_only, None, now()).is_none());
        // A billing error keeps its own kind and takes no captured quota window.
        let quota = CapturedReset {
            window: "five_hour".into(),
            resets_at: utc("2026-11-16T15:00:00Z").unwrap(),
        };
        let billing = classify_rate_limit_error(
            &err("low", Some(json!({ "errorKind": "billing_error" }))),
            Some(&quota),
            now(),
        )
        .unwrap();
        assert_eq!(
            (billing.kind.as_str(), billing.resets_at),
            ("billing_error", None)
        );
    }

    /// Account-level limits park; capacity trouble another account would meet too does not.
    #[test]
    fn only_account_level_error_kinds_park() {
        for (kind, parks) in [
            ("rate_limit", true),
            ("billing_error", true),
            ("overloaded", false),
            ("server_error", false),
            ("authentication_failed", false),
            ("invalid_request", false),
            ("max_output_tokens", false),
            ("unknown", false),
        ] {
            let data = Some(json!({ "errorKind": kind }));
            let message = format!("boom: {{\"errorKind\":\"{kind}\"}}");
            let direct =
                classify_rate_limit_error(&err("Credit balance is too low", data), None, now());
            let flat = classify_rate_limit_from_message(&message, None, now());
            assert_eq!((direct.is_some(), flat.is_some()), (parks, parks), "{kind}");
            if parks {
                assert_eq!(
                    direct.unwrap().resets_at,
                    None,
                    "{kind}: no reset is invented"
                );
            }
        }
        let billing = json!({ "errorKind": "billing_error" });
        let info = classify_rate_limit_error(
            &err("Credit balance is too low", Some(billing)),
            None,
            now(),
        )
        .unwrap();
        assert_eq!(info.kind, "billing_error");
    }

    #[test]
    fn agent_turn_text_parks_only_a_lone_limit_sentence() {
        use crate::acp::Event;
        let chunk = |text: &str| Event::AgentMessageChunk {
            text: text.into(),
            message_id: None,
        };
        let stopped = || Event::Stopped {
            reason: "agent_idle".into(),
        };
        let tool = || Event::ToolCallStarted {
            tool_call: crate::acp::state::ToolCall {
                prompt: None,
                id: "t".into(),
                name: "Bash".into(),
                kind: "execute".into(),
                args_preview: "{}".into(),
                started_at: crate::time::Timestamp::now(),
                parent_tool_call_id: None,
                memory_recall: None,
                diffs: Vec::new(),
            },
        };
        // (name, events, parks)
        let cases = [
            (
                "limit in two chunks",
                vec![
                    chunk("You've hit your 5-hour limit"),
                    chunk(" · resets 10pm"),
                ],
                true,
            ),
            (
                "the previous turn's text is dropped at its terminal",
                vec![chunk("Done."), stopped(), chunk("You've hit your limit")],
                true,
            ),
            (
                "after a tool call",
                vec![tool(), chunk("You've hit your limit")],
                false,
            ),
            ("ordinary text", vec![chunk("Build passed.")], false),
            (
                "long text stops collecting",
                vec![chunk(&"x".repeat(201)), chunk("You've hit your limit")],
                false,
            ),
        ];
        for (name, events, parks) in cases {
            let mut turn = AgentTurnText::default();
            events.iter().for_each(|e| turn.observe(e));
            assert_eq!(turn.take_rate_limit().is_some(), parks, "{name}");
            assert!(turn.take_rate_limit().is_none(), "{name}: taking resets");
        }
        for filler in ["x", " "] {
            let mut long = AgentTurnText::default();
            long.observe(&chunk("Working."));
            for _ in 0..=RateLimitInfo::AGENT_TEXT_MAX_CHARS * 4 + 64 {
                long.observe(&chunk(filler));
            }
            assert!(
                long.0.is_none(),
                "{filler:?}: long text releases its buffer"
            );
        }
        let mut padded = AgentTurnText::default();
        for _ in 0..1000 {
            padded.observe(&chunk(" "));
        }
        padded.observe(&chunk("You've hit your limit"));
        assert!(
            padded.take_rate_limit().is_some(),
            "leading space is dropped"
        );
        let mut prompted = AgentTurnText::default();
        prompted.disqualify();
        prompted.observe(&chunk("You've hit your limit"));
        assert!(prompted.take_rate_limit().is_none(), "prompted turn");
    }

    #[test]
    fn classify_rate_limit_error_cases() {
        let limit = json!({ "errorKind": "rate_limit" });
        let resets_at = crate::time::Timestamp::now() + crate::time::Duration::minutes(10);
        let captured = CapturedReset {
            window: "seven_day".into(),
            resets_at,
        };

        // #3152: no captured epoch means an unknown reset, never a guess.
        let info = classify_rate_limit_error(
            &err("You've hit your limit", Some(limit.clone())),
            None,
            now(),
        )
        .unwrap();
        assert_eq!(info.kind, "rate_limit");
        assert!(info.status.contains("hit your limit"));
        assert_eq!(info.resets_at, None);

        let info =
            classify_rate_limit_error(&err("limit", Some(limit)), Some(&captured), now()).unwrap();
        assert_eq!(info.resets_at, Some(resets_at));
        assert_eq!(info.kind, "seven_day", "the window names the limit");

        let with_reset = json!({ "errorKind": "rate_limit", "resets_at": "2099-01-01T00:00:00Z" });
        let info = classify_rate_limit_error(&err("x", Some(with_reset)), None, now()).unwrap();
        assert_eq!(info.resets_at, None);

        let internal = json!({ "errorKind": "internal" });
        assert!(classify_rate_limit_error(&err("closed", Some(internal)), None, now()).is_none());
        let invalid = agent_client_protocol::Error::invalid_params();
        assert!(classify_rate_limit_error(&invalid, None, now()).is_none());

        // The same fingerprint inside a flattened error message.
        let resets_at = crate::time::Timestamp::now() + crate::time::Duration::minutes(20);
        for msg in [
            "ACP connection failed: Internal error: limit: {\n  \"errorKind\":\"rate_limit\"\n}",
            "{\n  \"errorKind\": \"rate_limit\"\n}",
        ] {
            for (window, kind) in [("", "rate_limit"), ("five_hour", "five_hour")] {
                let captured = CapturedReset {
                    window: window.into(),
                    resets_at,
                };
                let info = classify_rate_limit_from_message(msg, Some(&captured), now()).unwrap();
                assert_eq!(info.kind, kind);
                assert_eq!(info.resets_at, Some(resets_at));
            }
        }
        assert!(classify_rate_limit_from_message("connection refused", None, now()).is_none());
    }

    #[test]
    fn rate_limit_reset_windows() {
        let secs = 4_102_444_800_i64;
        let meta = |value| {
            Some(serde_json::Map::from_iter([(
                "_claude/rateLimit".to_string(),
                value,
            )]))
        };
        let rejection = |window: &str| {
            Some(RateLimitRejection {
                window: window.to_string(),
                resets_at_secs: secs,
            })
        };
        let cases = [
            (
                meta(
                    json!({ "status": "rejected", "rateLimitType": "five_hour", "resetsAt": secs }),
                ),
                rejection("five_hour"),
            ),
            // #3028: millisecond epochs normalize to seconds.
            (
                meta(json!({ "status": "rejected", "resetsAt": secs * 1000 })),
                rejection(""),
            ),
            (meta(json!({ "status": "allowed", "resetsAt": secs })), None),
            (
                meta(json!({ "status": "allowed_warning", "resetsAt": secs })),
                None,
            ),
            (
                meta(
                    json!({ "status": "rejected", "resetsAt": secs, "overageStatus": "rejected" }),
                ),
                rejection(""),
            ),
            (
                meta(json!({ "status": "rejected", "resetsAt": secs, "overageStatus": "allowed" })),
                None,
            ),
            (
                meta(
                    json!({ "status": "rejected", "resetsAt": secs, "overageStatus": "allowed_warning" }),
                ),
                None,
            ),
            (meta(json!({ "status": "rejected" })), None),
            (meta(json!({ "status": "rejected", "resetsAt": 0 })), None),
            (None, None),
            (
                Some(serde_json::Map::from_iter([(
                    "claudeCode".to_string(),
                    json!({}),
                )])),
                None,
            ),
        ];
        for (meta, want) in cases {
            assert_eq!(rate_limit_rejection_from_meta(&meta), want, "{meta:?}");
        }

        // A window that recovers, or that extra usage starts covering, after
        // rejecting is dropped; others stay.
        let report = |status: &str, overage: &str, window: &str| {
            meta(json!({
                "status": status,
                "overageStatus": overage,
                "rateLimitType": window,
                "resetsAt": secs,
            }))
        };
        let captures = std::sync::Mutex::new(HashMap::new());
        for (status, overage, window, kept, want) in [
            ("rejected", "rejected", "five_hour", true, vec!["five_hour"]),
            (
                "rejected",
                "rejected",
                "seven_day",
                true,
                vec!["five_hour", "seven_day"],
            ),
            ("allowed", "rejected", "five_hour", false, vec!["seven_day"]),
            (
                "allowed_warning",
                "rejected",
                "other",
                false,
                vec!["seven_day"],
            ),
            ("rejected", "allowed", "seven_day", false, vec![]),
        ] {
            assert_eq!(
                record_rate_limit_report(&captures, &report(status, overage, window)),
                kept
            );
            let mut have: Vec<_> = captures.lock().unwrap().keys().cloned().collect();
            have.sort();
            assert_eq!(have, want, "{status} {overage} {window}");
        }

        {
            let now = crate::time::Timestamp::from_unix_secs(1_800_000_000).unwrap();
            let five_hour = now + crate::time::Duration::hours(2);
            let seven_day = now + crate::time::Duration::days(3);
            let past = now - crate::time::Duration::minutes(1);
            let cases = [
                (
                    vec![("five_hour", five_hour), ("seven_day", seven_day)],
                    Some(("seven_day", seven_day)),
                ),
                (
                    vec![("five_hour", five_hour), ("seven_day", past)],
                    Some(("five_hour", five_hour)),
                ),
                (vec![("", five_hour)], Some(("rate_limit", five_hour))),
                (vec![], None),
                (vec![("five_hour", past)], None),
            ];
            for (resets, want) in cases {
                let captures = std::sync::Mutex::new(
                    resets
                        .iter()
                        .map(|(window, dt)| (window.to_string(), dt.as_unix_secs()))
                        .collect(),
                );
                let captured =
                    captured_rate_limit_reset(&captures, now).map(|c| (c.kind(), c.resets_at));
                let want = want.map(|(kind, dt)| (kind.to_string(), dt));
                assert_eq!(captured, want);
                let info = take_rate_limit_from_captured_rejection(&captures, now)
                    .map(|i| (i.kind, i.resets_at.unwrap()));
                assert_eq!(info, want);
                assert!(take_rate_limit_from_captured_rejection(&captures, now).is_none());
            }

            // A reset past year 9999 still reports the limit.
            let captures = std::sync::Mutex::new(HashMap::from([(
                "seven_day".to_string(),
                253_402_300_800 + 86_400,
            )]));
            let info = take_rate_limit_from_captured_rejection(&captures, now).unwrap();
            assert_eq!(info.resets_at, Some(crate::time::Timestamp::MAX));

            // Of two resets both past year 9999, the later raw reset wins in either order.
            let five_hour = ("five_hour".to_string(), 253_402_300_800 + 86_400);
            let seven_day = ("seven_day".to_string(), 253_402_300_800 + 2 * 86_400);
            for order in [[&five_hour, &seven_day], [&seven_day, &five_hour]] {
                let captured = latest_future_reset(order.into_iter().map(|(w, s)| (w, s)), now);
                assert_eq!(captured.unwrap().kind(), "seven_day");
            }

            // One chrono could not represent is dropped, as chrono did.
            for secs in [8_210_266_876_800, i64::MAX, -1_000_000_000_000] {
                let captures =
                    std::sync::Mutex::new(HashMap::from([("seven_day".to_string(), secs)]));
                assert!(take_rate_limit_from_captured_rejection(&captures, now).is_none());
            }
        }
    }
}
