//! UTC timestamp whose text forms match what chrono wrote, so persisted JSON and
//! wire formats stay byte-identical.

use std::borrow::Cow;
use std::fmt;
use std::ops::{Add, AddAssign, Sub};
use std::time::SystemTime;

use ::time::error::TryFromParsed;
use ::time::format_description::well_known::Rfc3339;
use ::time::{Date, OffsetDateTime, UtcDateTime};

pub use ::time::SignedDuration as Duration;

#[derive(Debug)]
pub enum ParseError {
    Format(::time::error::Parse),
    /// Valid text whose UTC instant falls outside years -9999..=9999.
    OutOfRange,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Format(e) => e.fmt(f),
            Self::OutOfRange => f.write_str("timestamp out of range"),
        }
    }
}

impl std::error::Error for ParseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Format(e) => Some(e),
            Self::OutOfRange => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(UtcDateTime);

/// Fractional-second digits in RFC 3339 output, as in chrono's `SecondsFormat`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecondsFormat {
    Secs,
    Millis,
    Micros,
    /// 0, 3, 6 or 9 digits: the fewest that represent the value exactly.
    AutoSi,
}

impl Timestamp {
    pub const MIN: Self = Self(UtcDateTime::MIN);
    pub const MAX: Self = Self(UtcDateTime::MAX);

    pub fn now() -> Self {
        Self(UtcDateTime::now())
    }

    /// Accepts any RFC 3339 offset, or a colon-less one like `+0130` as
    /// chrono's serde reader did, and normalizes it to UTC. Also reads chrono's
    /// signed years such as `-0001-01-01T00:00:00Z`.
    pub fn parse_rfc3339(s: &str) -> Result<Self, ParseError> {
        let signed = s.strip_prefix(['+', '-']).and_then(|rest| {
            let end = 1 + rest.find('-')?;
            Some((s[..end].parse::<i32>().ok()?, &s[end..]))
        });
        let dt = match signed {
            Some((year, tail)) => {
                // 2000 is a leap year, so the day is checked against `year` below.
                let dt = parse_offset(&format!("2000{tail}"))?;
                if !(-262_143..=262_142).contains(&year)
                    || dt.day() > ::time::util::days_in_month(dt.month(), year)
                {
                    return Err(ParseError::OutOfRange);
                }
                // Clamp chrono values outside our range so stored records still load.
                if year > 9999 {
                    return Ok(Self::MAX);
                }
                if year < -9999 {
                    return Ok(Self::MIN);
                }
                let date = Date::from_calendar_date(year, dt.month(), dt.day())
                    .map_err(|e| ParseError::Format(TryFromParsed::ComponentRange(e).into()))?;
                dt.replace_date(date)
            }
            None => parse_offset(s)?,
        };
        dt.checked_to_utc().map(Self).ok_or(ParseError::OutOfRange)
    }

    pub fn from_unix_secs(secs: i64) -> Option<Self> {
        UtcDateTime::from_unix_timestamp(secs).ok().map(Self)
    }

    pub fn from_millis(millis: i64) -> Option<Self> {
        UtcDateTime::from_unix_timestamp_nanos(i128::from(millis) * 1_000_000)
            .ok()
            .map(Self)
    }

    /// Like `from_millis`, but saturates values chrono could represent.
    pub fn from_millis_saturating(ms: i64) -> Option<Self> {
        const CHRONO_MIN_MS: i64 = -8_334_601_228_800_000;
        const CHRONO_MAX_MS: i64 = 8_210_266_876_799_999;
        Self::from_millis(ms).or(match ms {
            1..=CHRONO_MAX_MS => Some(Self::MAX),
            CHRONO_MIN_MS..=-1 => Some(Self::MIN),
            _ => None,
        })
    }

    pub fn as_unix_secs(self) -> i64 {
        self.0.unix_timestamp()
    }

    /// Floors toward negative infinity, as chrono's `timestamp_millis` does.
    pub fn as_millis(self) -> i64 {
        self.0.unix_timestamp_nanos().div_euclid(1_000_000) as i64
    }

    pub fn checked_add(self, duration: Duration) -> Option<Self> {
        self.0.checked_add(duration).map(Self)
    }

    /// AutoSi fraction with a `+00:00` suffix, like chrono's `to_rfc3339`.
    pub fn to_rfc3339(self) -> String {
        self.to_rfc3339_opts(SecondsFormat::AutoSi, false)
    }

    pub fn to_rfc3339_opts(self, format: SecondsFormat, use_z: bool) -> String {
        let suffix = if use_z { "Z" } else { "+00:00" };
        format!(
            "{}T{}{}{suffix}",
            self.date(),
            self.hms(),
            self.fraction(format)
        )
    }

    /// `%Y-%m-%d %H:%M:%S`.
    pub fn to_ymd_hms(self) -> String {
        format!("{} {}", self.date(), self.hms())
    }

    fn date(self) -> String {
        let year = self.0.year();
        let sign = if year < 0 { "-" } else { "" };
        format!(
            "{sign}{:04}-{:02}-{:02}",
            year.unsigned_abs(),
            u8::from(self.0.month()),
            self.0.day()
        )
    }

    fn hms(self) -> String {
        format!(
            "{:02}:{:02}:{:02}",
            self.0.hour(),
            self.0.minute(),
            self.0.second()
        )
    }

    fn fraction(self, format: SecondsFormat) -> String {
        let nanos = self.0.nanosecond();
        let digits = match format {
            SecondsFormat::Secs => 0,
            SecondsFormat::Millis => 3,
            SecondsFormat::Micros => 6,
            SecondsFormat::AutoSi if nanos == 0 => 0,
            SecondsFormat::AutoSi if nanos.is_multiple_of(1_000_000) => 3,
            SecondsFormat::AutoSi if nanos.is_multiple_of(1_000) => 6,
            SecondsFormat::AutoSi => 9,
        };
        if digits == 0 {
            return String::new();
        }
        let scaled = nanos / 10u32.pow(9 - digits);
        format!(".{scaled:0width$}", width = digits as usize)
    }
}

fn parse_offset(s: &str) -> Result<OffsetDateTime, ParseError> {
    OffsetDateTime::parse(s, &Rfc3339)
        .or_else(|e| {
            let b = s.as_bytes();
            let n = b.len();
            if n >= 6
                && matches!(b[n - 5], b'+' | b'-')
                && b[n - 6].is_ascii_digit()
                && b[n - 4..].iter().all(u8::is_ascii_digit)
            {
                let (head, minutes) = s.split_at(n - 2);
                OffsetDateTime::parse(&format!("{head}:{minutes}"), &Rfc3339)
            } else {
                Err(e)
            }
        })
        .map_err(ParseError::Format)
}

/// chrono's `Display`: `2024-01-02 03:04:05.123 UTC`.
impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}{} UTC",
            self.to_ymd_hms(),
            self.fraction(SecondsFormat::AutoSi)
        )
    }
}

/// chrono's `Debug`, which equals its serde form.
impl fmt::Debug for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_rfc3339_opts(SecondsFormat::AutoSi, true))
    }
}

/// The Unix epoch, as chrono's `Default`.
impl Default for Timestamp {
    fn default() -> Self {
        Self(UtcDateTime::UNIX_EPOCH)
    }
}

impl From<SystemTime> for Timestamp {
    fn from(t: SystemTime) -> Self {
        Self(UtcDateTime::from(t))
    }
}

impl Add<Duration> for Timestamp {
    type Output = Self;
    fn add(self, rhs: Duration) -> Self {
        Self(self.0 + rhs)
    }
}

impl AddAssign<Duration> for Timestamp {
    fn add_assign(&mut self, rhs: Duration) {
        self.0 += rhs;
    }
}

impl Sub<Duration> for Timestamp {
    type Output = Self;
    fn sub(self, rhs: Duration) -> Self {
        Self(self.0 - rhs)
    }
}

impl Sub for Timestamp {
    type Output = Duration;
    fn sub(self, rhs: Self) -> Duration {
        self.0 - rhs.0
    }
}

impl serde::Serialize for Timestamp {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_rfc3339_opts(SecondsFormat::AutoSi, true))
    }
}

impl<'de> serde::Deserialize<'de> for Timestamp {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = Cow::<str>::deserialize(deserializer)?;
        Self::parse_rfc3339(&s).map_err(serde::de::Error::custom)
    }
}

/// Same schema as schemars' `chrono04` impl for `DateTime<Utc>`.
impl schemars::JsonSchema for Timestamp {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        "DateTime".into()
    }

    fn schema_id() -> Cow<'static, str> {
        "chrono::DateTime".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "format": "date-time"
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64, nanos: u32) -> Timestamp {
        let nanos = i128::from(secs) * 1_000_000_000 + i128::from(nanos);
        Timestamp(UtcDateTime::from_unix_timestamp_nanos(nanos).unwrap())
    }

    /// Expected strings were captured from chrono 0.4.45.
    #[test]
    fn formats_match_chrono() {
        use SecondsFormat::*;
        const B: i64 = 1_704_164_645;
        // (secs, nanos, serde/Debug, to_rfc3339 and %+, Millis, Micros, Display)
        let cases: &[(i64, u32, &str, &str, &str, &str, &str)] = &[
            (
                B,
                0,
                "2024-01-02T03:04:05Z",
                "2024-01-02T03:04:05+00:00",
                "2024-01-02T03:04:05.000Z",
                "2024-01-02T03:04:05.000000Z",
                "2024-01-02 03:04:05 UTC",
            ),
            (
                B,
                100_000_000,
                "2024-01-02T03:04:05.100Z",
                "2024-01-02T03:04:05.100+00:00",
                "2024-01-02T03:04:05.100Z",
                "2024-01-02T03:04:05.100000Z",
                "2024-01-02 03:04:05.100 UTC",
            ),
            (
                B,
                120_000_000,
                "2024-01-02T03:04:05.120Z",
                "2024-01-02T03:04:05.120+00:00",
                "2024-01-02T03:04:05.120Z",
                "2024-01-02T03:04:05.120000Z",
                "2024-01-02 03:04:05.120 UTC",
            ),
            (
                B,
                123_000_000,
                "2024-01-02T03:04:05.123Z",
                "2024-01-02T03:04:05.123+00:00",
                "2024-01-02T03:04:05.123Z",
                "2024-01-02T03:04:05.123000Z",
                "2024-01-02 03:04:05.123 UTC",
            ),
            (
                B,
                123_400_000,
                "2024-01-02T03:04:05.123400Z",
                "2024-01-02T03:04:05.123400+00:00",
                "2024-01-02T03:04:05.123Z",
                "2024-01-02T03:04:05.123400Z",
                "2024-01-02 03:04:05.123400 UTC",
            ),
            (
                B,
                123_456_000,
                "2024-01-02T03:04:05.123456Z",
                "2024-01-02T03:04:05.123456+00:00",
                "2024-01-02T03:04:05.123Z",
                "2024-01-02T03:04:05.123456Z",
                "2024-01-02 03:04:05.123456 UTC",
            ),
            (
                B,
                123_456_789,
                "2024-01-02T03:04:05.123456789Z",
                "2024-01-02T03:04:05.123456789+00:00",
                "2024-01-02T03:04:05.123Z",
                "2024-01-02T03:04:05.123456Z",
                "2024-01-02 03:04:05.123456789 UTC",
            ),
            (
                B,
                1_000,
                "2024-01-02T03:04:05.000001Z",
                "2024-01-02T03:04:05.000001+00:00",
                "2024-01-02T03:04:05.000Z",
                "2024-01-02T03:04:05.000001Z",
                "2024-01-02 03:04:05.000001 UTC",
            ),
            (
                B,
                1,
                "2024-01-02T03:04:05.000000001Z",
                "2024-01-02T03:04:05.000000001+00:00",
                "2024-01-02T03:04:05.000Z",
                "2024-01-02T03:04:05.000000Z",
                "2024-01-02 03:04:05.000000001 UTC",
            ),
            (
                -1_000_086_399,
                500_000_000,
                "1938-04-23T22:13:21.500Z",
                "1938-04-23T22:13:21.500+00:00",
                "1938-04-23T22:13:21.500Z",
                "1938-04-23T22:13:21.500000Z",
                "1938-04-23 22:13:21.500 UTC",
            ),
            (
                -1,
                999_999_999,
                "1969-12-31T23:59:59.999999999Z",
                "1969-12-31T23:59:59.999999999+00:00",
                "1969-12-31T23:59:59.999Z",
                "1969-12-31T23:59:59.999999Z",
                "1969-12-31 23:59:59.999999999 UTC",
            ),
            (
                -62_198_755_200,
                0,
                "-0001-01-01T00:00:00Z",
                "-0001-01-01T00:00:00+00:00",
                "-0001-01-01T00:00:00.000Z",
                "-0001-01-01T00:00:00.000000Z",
                "-0001-01-01 00:00:00 UTC",
            ),
        ];
        for &(secs, nanos, serde, rfc, millis, micros, display) in cases {
            let t = at(secs, nanos);
            let whole = &micros[..micros.len() - 8];
            let ymd_hms = whole.replace('T', " ");
            assert_eq!(serde_json::to_string(&t).unwrap(), format!("\"{serde}\""));
            assert_eq!(format!("{t:?}"), serde);
            assert_eq!(t.to_rfc3339_opts(AutoSi, true), serde);
            assert_eq!(t.to_rfc3339(), rfc);
            assert_eq!(t.to_rfc3339_opts(AutoSi, false), rfc);
            assert_eq!(t.to_rfc3339_opts(Secs, true), format!("{whole}Z"));
            assert_eq!(t.to_rfc3339_opts(Secs, false), format!("{whole}+00:00"));
            assert_eq!(t.to_rfc3339_opts(Millis, true), millis);
            assert_eq!(
                t.to_rfc3339_opts(Millis, false),
                millis.replace('Z', "+00:00")
            );
            assert_eq!(t.to_rfc3339_opts(Micros, true), micros);
            assert_eq!(
                t.to_rfc3339_opts(Micros, false),
                micros.replace('Z', "+00:00")
            );
            assert_eq!(t.to_ymd_hms(), ymd_hms);
            assert_eq!(t.to_string(), display);
            assert_eq!(t.as_unix_secs(), secs);
            assert_eq!(
                serde_json::from_str::<Timestamp>(&format!("\"{serde}\"")).unwrap(),
                t
            );
            assert_eq!(Timestamp::parse_rfc3339(rfc).unwrap(), t);
        }
        // chrono's timestamp_millis floors.
        assert_eq!(at(-1, 999_999_999).as_millis(), -1);
        assert_eq!(
            at(-1_000_086_399, 500_000_000).as_millis(),
            -1_000_086_398_500
        );
        assert_eq!(
            Timestamp::from_millis(-1_000_086_398_500),
            Some(at(-1_000_086_399, 500_000_000))
        );
        assert_eq!(Timestamp::from_unix_secs(B), Some(at(B, 0)));
        assert_eq!(Timestamp::default(), at(0, 0));
    }

    #[test]
    fn from_millis_saturates_only_within_chronos_range() {
        let cases = [
            (1_704_164_645_123, Some(at(1_704_164_645, 123_000_000))),
            (8_210_266_876_799_999, Some(Timestamp::MAX)),
            (8_210_266_876_800_000, None),
            (i64::MAX, None),
            (-8_334_601_228_800_000, Some(Timestamp::MIN)),
            (-8_334_601_228_800_001, None),
        ];
        for (ms, expected) in cases {
            assert_eq!(Timestamp::from_millis_saturating(ms), expected, "{ms}");
        }
    }

    #[test]
    fn parses_chrono_inputs_and_schema() {
        let base = at(1_704_164_645, 0);
        let cases = [
            ("2024-01-02T03:04:05Z", base),
            ("2024-01-02T03:04:05+00:00", base),
            ("2024-01-02T05:04:05+02:00", base),
            ("2024-01-02t03:04:05z", base),
            ("2024-01-02 03:04:05Z", base),
            ("2024-01-02T03:04:05+0000", base),
            ("2024-01-02T04:34:05+0130", base),
            ("2024-01-01T22:04:05-0500", base),
            ("2024-01-02T03:04:05.1Z", at(1_704_164_645, 100_000_000)),
            ("2024-01-02T03:04:05.123Z", at(1_704_164_645, 123_000_000)),
            (
                "2024-01-02T03:04:05.123456Z",
                at(1_704_164_645, 123_456_000),
            ),
            (
                "2024-01-02T03:04:05.123456789Z",
                at(1_704_164_645, 123_456_789),
            ),
            ("+2024-01-02T04:34:05+0130", base),
            ("+57000-01-01T00:00:00Z", Timestamp::MAX),
            ("-10000-01-01T00:00:00Z", Timestamp::MIN),
        ];
        for (input, expected) in cases {
            assert_eq!(
                Timestamp::parse_rfc3339(input).unwrap(),
                expected,
                "{input}"
            );
        }
        assert!(Timestamp::parse_rfc3339("2024-01-02T03:04:05").is_err());
        assert!(Timestamp::parse_rfc3339("9999-12-31T23:59:59-01:00").is_err());
        assert!(Timestamp::parse_rfc3339("2024-01-02T03:04:05+000").is_err());
        for bad in [
            "-0001-02-29T00:00:00Z",
            "+57001-02-29T00:00:00Z",
            "+262143-01-01T00:00:00Z",
            "+",
            "-x-01-01T00:00:00Z",
            "+57000",
        ] {
            assert!(Timestamp::parse_rfc3339(bad).is_err(), "{bad}");
        }

        #[derive(serde::Deserialize)]
        struct Row {
            #[serde(default)]
            at: Option<Timestamp>,
        }
        for json in [r#"{}"#, r#"{"at":null}"#] {
            assert_eq!(serde_json::from_str::<Row>(json).unwrap().at, None);
        }

        // Captured from schemars' chrono04 impl for `DateTime<Utc>`.
        assert_eq!(
            serde_json::to_value(schemars::schema_for!(Timestamp)).unwrap(),
            serde_json::json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "title": "DateTime",
                "type": "string",
                "format": "date-time"
            })
        );
    }
}
