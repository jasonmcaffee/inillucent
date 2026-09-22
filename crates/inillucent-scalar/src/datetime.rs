//! The date and time built-ins.
//!
//! Invariant: everything is computed on the *Julian day number*, as a double,
//! exactly as SQLite does. That is not an implementation detail that could be
//! swapped for a civil-calendar library: the value `julianday()` returns is
//! part of the observable behaviour, `unixepoch()` is derived from it, and the
//! `+N days` modifiers are additions to it. A civil-date implementation would
//! agree on most inputs and disagree on the ones that matter - the proleptic
//! Gregorian calendar before 1582, and the half-day offset that makes a Julian
//! day start at noon.
//!
//! `localtime` and `utc` ask the machine's zone, through
//! [`inillucent_vfs::zone`]. They used to answer NULL and nothing, on the
//! argument that an answer which depends on the machine's zone is one two
//! engines cannot be graded against. task-1979's differential corpus is that
//! grading, and it runs both engines on one machine, where the argument does
//! not hold: `datetime('2020-01-01 12:00:00','utc')` is a fixed conversion
//! both engines can be asked for and compared on. The same objection would
//! apply to `'now'`, which this module has always implemented.
//!
//! Three things here are kept the way SQLite keeps them rather than the way a
//! Julian day alone would give:
//!
//! - **The range.** A date outside Julian day 0 to 5373484.5 - roughly the
//!   year -4713 to the end of 9999 - is NULL rather than a date nobody can
//!   store. [`Moment::inside_the_range`].
//! - **The fields as they were spelled.** `'2020-01-01 24:00:00'` is an hour
//!   24 SQLite accepts and does not carry into the next day, so `date()` of it
//!   is `2020-01-01` and `strftime('%H', ...)` is `24`, while `julianday()` of
//!   it is the next day's midnight. [`Moment::spelled`] holds the fields until
//!   a modifier makes them stale.
//! - **What a `floor` modifier takes off.** Adding a month to 31 January lands
//!   on 31 February, which carries into March; `floor` asks for the last day
//!   of February instead. [`Moment::overflow`] is how many days the carry
//!   moved, recorded when it happens and subtracted when `floor` asks.

use inillucent_sql::function::TimeFunc;
use inillucent_value::{numeric, TextEncoding, Value};

/// The Julian day number of 1970-01-01T00:00:00Z.
const UNIX_EPOCH_JD: f64 = 2440587.5;

/// Seconds in a day, as the conversion between the two scales.
const SECONDS_PER_DAY: f64 = 86_400.0;

/// Returns the Julian day of the wall clock, now.
///
/// It is read once per statement rather than per call, which is what makes two
/// mentions of `'now'` in one statement agree.
pub fn julian_now() -> f64 {
    let since_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs_f64())
        .unwrap_or(0.0);
    UNIX_EPOCH_JD + since_epoch / SECONDS_PER_DAY
}

/// The first Julian day a date function will answer with.
const FIRST_JULIAN_DAY: f64 = 0.0;

/// The first Julian day past the last one a date function will answer with.
///
/// SQLite's bound, in its own units, is `iJD <= 464269060799999` milliseconds,
/// which is the last millisecond of 9999-12-31.
const PAST_LAST_JULIAN_DAY: f64 = 5_373_484.5;

/// How a year is written out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum YearStyle {
    /// `date()`, `time()` and `datetime()`: a minus sign if the year is
    /// negative, then four digits. SQLite writes the year of
    /// `date('0000-01-01','-1 day')` as `-0001`.
    Signed,
    /// `strftime()`: C's `%04d`, which counts the sign against the width, so
    /// the same year is `-001`. SQLite's two formatters really do differ here
    /// and both were checked against 3.53.4.
    Printf,
}

/// A broken-down date and time, as the formatter reads it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Civil {
    /// The proleptic Gregorian year.
    pub year: i64,
    /// The month, 1 to 12.
    pub month: i64,
    /// The day of the month, 1 to 31.
    pub day: i64,
    /// The hour, 0 to 23.
    pub hour: i64,
    /// The minute, 0 to 59.
    pub minute: i64,
    /// The second, with its fraction.
    pub second: f64,
}

/// Which zone conversion a modifier has already applied.
///
/// **Repeating the same one is a no-op, and the other one is not (task-1979,
/// F13's neighbour).** `datetime(x,'utc','utc')` is `datetime(x,'utc')` in
/// SQLite, and `datetime(x,'utc','+1 day','utc')` is that shifted by a day - an
/// ordinary modifier in between does not clear it. `datetime(x,'utc',
/// 'localtime')` does convert twice and lands back where it started. All eight
/// orderings were read off 3.53.4 rather than worked out from the rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Zone {
    /// Neither modifier has run.
    Unset,
    /// The last one was `utc`.
    Utc,
    /// The last one was `localtime`.
    Local,
}

/// A time value part-way through being resolved.
///
/// SQLite's `DateTime` keeps a Julian day and the broken-down fields side by
/// side and marks which of them is current; this is the same idea with the
/// three things that behaviour turns on.
#[derive(Clone, Copy, Debug)]
struct Moment {
    /// The Julian day, which is always current.
    day: f64,
    /// The fields exactly as the input spelled them, while no modifier has
    /// made them stale.
    ///
    /// **This is what keeps hour 24 (task-1979, F12).** SQLite accepts
    /// `'2020-01-01 24:00:00'` and leaves the fields alone, so `date()` of it
    /// answers `2020-01-01` and `strftime('%H', ...)` answers `24`, while
    /// `julianday()` of it answers the next day's midnight. Rendering from the
    /// Julian day instead answered `2020-01-02` and hour `00`. One modifier -
    /// any modifier - recomputes the fields and the carry happens, which is
    /// also SQLite's rule: `datetime('2020-01-01 24:00:00','+0 day')` is
    /// `2020-01-02 00:00:00` in both engines.
    spelled: Option<Civil>,
    /// How many days a `floor` modifier would take off.
    ///
    /// **Set by the month and year modifiers, read by `floor` (task-1979,
    /// F14).** `date('2020-01-31','+1 month')` lands on 31 February, which is
    /// two days past the end of that February, so the date carries to 2 March
    /// and this holds 2; `floor` subtracts it and the answer is 29 February.
    /// `ceiling` is the default and only clears it.
    overflow: i64,
    /// Which of the two zone conversions ran last - see [`Zone`].
    zone: Zone,
}

impl Moment {
    /// Returns a moment with no spelled fields and no carry, which is what a
    /// number or the clock gives.
    ///
    /// @param day - the Julian day
    fn at(day: f64) -> Moment {
        Moment {
            day,
            spelled: None,
            overflow: 0,
            zone: Zone::Unset,
        }
    }

    /// Returns the same instant with the spelled fields discarded.
    ///
    /// @param day - the Julian day the modifier produced
    fn moved_to(self, day: f64) -> Moment {
        Moment {
            day,
            spelled: None,
            ..self
        }
    }

    /// Returns the broken-down fields a formatter should read.
    fn civil(self) -> Civil {
        self.spelled.unwrap_or_else(|| civil_of(self.day))
    }

    /// Returns the same instant with the calendar date recomputed from the
    /// Julian day and the clock left as it was spelled.
    ///
    /// **A day of the month the month does not have is normalised, and the hour
    /// is not.** `date('2020-02-30')` is `2020-03-01` in SQLite and
    /// `datetime('2020-02-30 24:00:00')` is `2020-03-02 24:00:00`: the date
    /// carried and the hour stayed at 24. Its `isDate` says so in one line -
    /// `if( argc==1 && p->validYMD && p->D>28 ) p->validYMD = 0;` - and the
    /// fields it clears are the calendar ones only. With a modifier written
    /// after it the rule does not apply at all, which is why
    /// `datetime('2020-02-30','subsec')` is `2020-02-30 00:00:00.000`.
    fn with_recomputed_date(self) -> Moment {
        let Some(spelled) = self.spelled else {
            return self;
        };
        if spelled.day <= 28 {
            return self;
        }
        let recomputed = civil_of(self.day);
        Moment {
            spelled: Some(Civil {
                year: recomputed.year,
                month: recomputed.month,
                day: recomputed.day,
                ..spelled
            }),
            ..self
        }
    }

    /// Reports whether the date is one SQLite will answer with.
    ///
    /// Outside it every date function answers NULL rather than a year nobody
    /// can store: `date('9999-12-31','+1 day')` is NULL, not `10000-01-01`
    /// (task-1979, F11).
    fn inside_the_range(self) -> bool {
        (FIRST_JULIAN_DAY..PAST_LAST_JULIAN_DAY).contains(&self.day)
    }
}

/// Calls a date or time function.
///
/// `now` is the wall clock the caller supplies rather than one this module
/// reads, so a statement that names `'now'` twice sees one time and a test can
/// pin it.
pub fn call(
    func: TimeFunc,
    arguments: &[Value<'static>],
    now: f64,
    encoding: TextEncoding,
) -> Value<'static> {
    if func == TimeFunc::TimeDiff {
        return timediff(arguments, now, encoding);
    }
    let (format, rest) = match func {
        TimeFunc::StrfTime => {
            let Some(first) = arguments.first() else {
                return Value::Null;
            };
            let Value::Text(text) = first else {
                return Value::Null;
            };
            (Some(text.utf8_bytes().to_vec()), arguments.get(1..))
        }
        _ => (None, arguments.get(..)),
    };
    let rest = rest.unwrap_or(&[]);
    let Some(moment) = resolve(rest, now, encoding) else {
        return Value::Null;
    };
    let day = moment.day;
    let civil = moment.civil();
    // **`subsec` is a modifier the *renderer* has to know about.** It asks for
    // the fractional second, so `datetime(x, 'subsec')` formats seconds as
    // `%f` rather than `%S`. It reached `unixepoch` and nothing else, so
    // `datetime('2024-03-01 12:00:00', 'subsec')` answered `12:00:00` where
    // SQLite answers `12:00:00.000` - a modifier accepted and dropped, which is
    // the one outcome a caller cannot detect.
    let subsec = asks_for_subsec(rest);
    match func {
        TimeFunc::JulianDay => Value::Real(day),
        TimeFunc::UnixEpoch => unix_epoch(rest, day, encoding),
        TimeFunc::Date => render(day, civil, b"%Y-%m-%d", YearStyle::Signed),
        TimeFunc::Time => render(
            day,
            civil,
            if subsec { b"%H:%M:%f" } else { b"%H:%M:%S" },
            YearStyle::Signed,
        ),
        TimeFunc::DateTime => render(
            day,
            civil,
            if subsec {
                b"%Y-%m-%d %H:%M:%f"
            } else {
                b"%Y-%m-%d %H:%M:%S"
            },
            YearStyle::Signed,
        ),
        TimeFunc::StrfTime => render(day, civil, &format.unwrap_or_default(), YearStyle::Printf),
        TimeFunc::TimeDiff => Value::Null,
    }
}

/// Reports whether an argument list carries the `subsec` modifier.
///
/// @param arguments - the time value and its modifiers
fn asks_for_subsec(arguments: &[Value<'static>]) -> bool {
    arguments.iter().skip(1).any(|modifier| {
        let Value::Text(text) = modifier else {
            return false;
        };
        let folded = trim(&text.utf8_bytes()).to_ascii_lowercase();
        folded == b"subsec" || folded == b"subsecond"
    })
}

/// Returns the moment one argument list resolves to.
///
/// The first argument is the time value and every later one is a modifier,
/// applied in order. With no arguments at all the value is `'now'`, which is
/// why `date()` and `date('now')` are the same call.
///
/// `None` when any step has no answer, and also when the date the steps landed
/// on is outside the range SQLite answers with - see
/// [`Moment::inside_the_range`].
///
/// @param arguments - the time value and its modifiers
/// @param now - the wall clock the caller supplies
/// @param encoding - the text encoding a numeric string is read in
fn resolve(arguments: &[Value<'static>], now: f64, encoding: TextEncoding) -> Option<Moment> {
    let mut moment = match arguments.first() {
        Some(value) => parse_time_value(value, now, encoding)?,
        None => Moment::at(now),
    };
    if arguments.len() <= 1 {
        moment = moment.with_recomputed_date();
    }
    for modifier in arguments.get(1..).unwrap_or(&[]) {
        let Value::Text(text) = modifier else {
            return None;
        };
        moment = apply_modifier(moment, &text.utf8_bytes())?;
    }
    moment.inside_the_range().then_some(moment)
}

/// Returns `unixepoch()`, honouring the `subsec` modifier.
fn unix_epoch(arguments: &[Value<'static>], day: f64, encoding: TextEncoding) -> Value<'static> {
    let _ = encoding;
    let seconds = (day - UNIX_EPOCH_JD) * SECONDS_PER_DAY;
    if asks_for_subsec(arguments) {
        // **Rounded to the millisecond first, for the same reason
        // `whole_seconds` rounds.** A Julian day is a binary fraction, so
        // `unixepoch('2024-03-01 09:05:07','subsec')` came out as
        // 1709283906.9999843 where SQLite answers 1709283907.0. SQLite holds
        // the instant in whole milliseconds and divides, and a millisecond is
        // as fine as `subsec` ever reports, so nothing a caller can ask for is
        // lost by matching it.
        return Value::Real(milliseconds_of(seconds) / 1000.0);
    }
    Value::Integer(whole_seconds(seconds))
}

/// Returns the whole seconds a Julian-day difference stands for.
///
/// **Flooring the double directly was one second low.** A Julian day is a
/// binary fraction, so `2024-03-01 09:05:07` comes back as
/// `1709283906.9999998` rather than as `1709283907`, and `floor` on that is
/// 1,709,283,906 - a wrong answer on `strftime('%s', ...)` and on
/// `unixepoch()` alike, for every timestamp whose representation happens to
/// fall short. SQLite works in whole milliseconds throughout, so the value is
/// rounded to a millisecond first and only then reduced to seconds; a
/// half-second is still floored, which is what makes the truncation SQLite's
/// rather than a rounding of its own.
///
/// @param seconds - the difference from the epoch, in seconds
fn whole_seconds(seconds: f64) -> i64 {
    (milliseconds_of(seconds) as i64).div_euclid(1000)
}

/// Returns the whole milliseconds a difference in seconds stands for.
///
/// The unit SQLite holds a time in, and the one both `unixepoch` answers are
/// derived from - see `whole_seconds` for what flooring the double instead
/// cost.
///
/// @param seconds - the difference from the epoch, in seconds
fn milliseconds_of(seconds: f64) -> f64 {
    (seconds * 1000.0).round()
}

/// Returns `timediff(a, b)` as SQLite's `+YYYY-MM-DD HH:MM:SS.SSS` string.
fn timediff(arguments: &[Value<'static>], now: f64, encoding: TextEncoding) -> Value<'static> {
    let (Some(left), Some(right)) = (arguments.first(), arguments.get(1)) else {
        return Value::Null;
    };
    let (Some(left), Some(right)) = (
        parse_time_value(left, now, encoding),
        parse_time_value(right, now, encoding),
    ) else {
        return Value::Null;
    };
    if !left.inside_the_range() || !right.inside_the_range() {
        return Value::Null;
    }
    let (left, right) = (left.day, right.day);
    let (sign, low, high) = if left >= right {
        ('+', right, left)
    } else {
        ('-', left, right)
    };
    let mut from = civil_of(low);
    let to = civil_of(high);
    // Years and months are counted on the calendar rather than derived from the
    // day count, because they are not a fixed number of days: the difference
    // between 31 January and 1 March is one month and one day, whichever year
    // it is.
    let mut years = to.year - from.year;
    let mut months = to.month - from.month;
    if months < 0 {
        years -= 1;
        months += 12;
    }
    from.year += years;
    from.month += months;
    if from.month > 12 {
        from.year += 1;
        from.month -= 12;
    }
    let mut anchor = julian_of(from);
    // Backing off one month at a time, and more than once. Adding a month to
    // 31 January overshoots by carrying into March, and the month before that
    // overshoots too whenever February is short - so `timediff('2026-03-01',
    // '2026-01-31')` is nought months and twenty-nine days, and a single step
    // back left it at one month and minus two days.
    let mut guard = 0usize;
    while anchor > high && (years > 0 || months > 0) && guard < 24 {
        guard = guard.saturating_add(1);
        if months == 0 {
            years -= 1;
            months = 11;
        } else {
            months -= 1;
        }
        let mut back = civil_of(low);
        back.year += years;
        back.month += months;
        while back.month > 12 {
            back.year += 1;
            back.month -= 12;
        }
        anchor = julian_of(back);
    }
    let remainder = high - anchor;
    let days = remainder.floor();
    let seconds = (remainder - days) * SECONDS_PER_DAY;
    let hours = (seconds / 3600.0).floor();
    let minutes = ((seconds - hours * 3600.0) / 60.0).floor();
    let whole = seconds - hours * 3600.0 - minutes * 60.0;
    let text = format!(
        "{sign}{:04}-{:02}-{:02} {:02}:{:02}:{:06.3}",
        years, months, days as i64, hours as i64, minutes as i64, whole
    );
    // A fallible allocation is the only way this can fail, and a NULL is
    // the answer SQLite gives when it cannot build the string either.
    Value::owned_text(text.as_bytes()).unwrap_or(Value::Null)
}

/// Returns the moment a time value names.
///
/// The forms are SQLite's: a number is a Julian day unless a `unixepoch`
/// modifier says otherwise, and text is one of the ISO-8601 shapes or the
/// literal `now`.
///
/// Only the ISO-8601 shapes carry [`Moment::spelled`] fields, and only when the
/// text named no zone offset. A number has no fields to keep, and an offset
/// means the fields as written are not the instant that was stored.
///
/// @param value - the first argument of the call
/// @param now - the wall clock the caller supplies
/// @param encoding - the text encoding a numeric string is read in
fn parse_time_value(value: &Value<'static>, now: f64, encoding: TextEncoding) -> Option<Moment> {
    match value {
        Value::Null => None,
        Value::Integer(integer) => Some(Moment::at(*integer as f64)),
        Value::Real(real) => Some(Moment::at(*real)),
        Value::Blob(_) => None,
        Value::Text(text) => {
            let raw = text.utf8_bytes().to_vec();
            let trimmed = trim(&raw);
            if trimmed.eq_ignore_ascii_case(b"now") {
                return Some(Moment::at(now));
            }
            // **Trailing whitespace only** (task-2066 section 4.2, item 27).
            // `date(' 2024-01-01')` answered `2024-01-01` here and is NULL in
            // SQLite, whose `parseYyyyMmDd` reads the string it was given and
            // skips nothing before the year. It *does* skip whitespace after
            // the date, which is why `date('2024-01-01 ')` is a date in both -
            // and trimming both ends made the first case wrong while a raw
            // parse would have made the second wrong. The reference's own
            // answers to the pair are `differential_part8`'s `t2066-003`.
            if let Some((day, spelled)) = parse_iso(trim_end(&raw)) {
                return Some(Moment {
                    day,
                    spelled,
                    overflow: 0,
                    zone: Zone::Unset,
                });
            }
            // A numeric string is a Julian day, which is what makes
            // `date('2451545.0')` work.
            if numeric::looks_numeric(trimmed, encoding) {
                let parsed = numeric::atof(trimmed, encoding);
                return Some(Moment::at(parsed.value));
            }
            None
        }
    }
}

/// Returns the bytes with any trailing whitespace removed.
///
/// @param bytes - the text as it was written
fn trim_end(bytes: &[u8]) -> &[u8] {
    let mut end = bytes.len();
    while end > 0
        && end
            .checked_sub(1)
            .and_then(|index| bytes.get(index))
            .is_some_and(|byte| numeric::is_space(*byte))
    {
        end = end.saturating_sub(1);
    }
    bytes.get(..end).unwrap_or(bytes)
}

/// Returns the bytes with leading and trailing spaces removed.
fn trim(bytes: &[u8]) -> &[u8] {
    let mut start = 0usize;
    let mut end = bytes.len();
    while start < end
        && bytes
            .get(start)
            .is_some_and(|byte| numeric::is_space(*byte))
    {
        start = start.saturating_add(1);
    }
    while end > start
        && end
            .checked_sub(1)
            .and_then(|index| bytes.get(index))
            .is_some_and(|byte| numeric::is_space(*byte))
    {
        end = end.saturating_sub(1);
    }
    bytes.get(start..end).unwrap_or(&[])
}

/// Parses one of the ISO-8601 shapes SQLite accepts.
///
/// The accepted set is exactly SQLite's: a date, a time, or a date and a time
/// separated by a space or a `T`, with an optional fractional second and an
/// optional `Z` or `±HH:MM` offset.
///
/// The fields come back beside the Julian day when the text named no offset,
/// so that an hour 24 survives to the formatter - see [`Moment::spelled`].
///
/// @param bytes - the text, already trimmed
fn parse_iso(bytes: &[u8]) -> Option<(f64, Option<Civil>)> {
    let (date_part, time_part) = split_datetime(bytes)?;
    let mut civil = Civil {
        year: 2000,
        month: 1,
        day: 1,
        hour: 0,
        minute: 0,
        second: 0.0,
    };
    let mut offset_minutes = 0i64;
    if let Some(date) = date_part {
        let (year, month, day) = parse_date(date)?;
        civil.year = year;
        civil.month = month;
        civil.day = day;
    }
    if let Some(time) = time_part {
        let (hour, minute, second, offset) = parse_time(time)?;
        civil.hour = hour;
        civil.minute = minute;
        civil.second = second;
        offset_minutes = offset;
    }
    if date_part.is_none() && time_part.is_none() {
        return None;
    }
    let day = julian_of(civil);
    if offset_minutes != 0 {
        return Some((day - (offset_minutes as f64) / 1440.0, None));
    }
    Some((day, Some(civil)))
}

/// Splits a timestamp into its date and time halves.
fn split_datetime(bytes: &[u8]) -> Option<(Option<&[u8]>, Option<&[u8]>)> {
    if bytes.is_empty() {
        return None;
    }
    // **An upper-case `T` only.** SQLite accepts a space or a `T` between the
    // date and the time and nothing else, so `datetime('2024-03-01t12:34:56')`
    // is NULL there; accepting the lower-case one here turned a string SQLite
    // says is not a date into one.
    let separator = bytes.iter().position(|byte| *byte == b' ' || *byte == b'T');
    match separator {
        Some(at) => {
            let date = bytes.get(..at)?;
            let time = bytes.get(at.saturating_add(1)..)?;
            Some((Some(date), Some(time)))
        }
        None => {
            // A bare value is a date when it holds a `-`, and a time when it
            // holds a `:`. Anything else is neither.
            if bytes.contains(&b'-') {
                Some((Some(bytes), None))
            } else if bytes.contains(&b':') {
                Some((None, Some(bytes)))
            } else {
                None
            }
        }
    }
}

/// Parses `YYYY-MM-DD`.
///
/// **The widths are exact** (task-2066 section 4.2, item 27). `digits` accepts
/// a run of any length, so `date('2024-1-1')` and `date('02024-01-01')` both
/// parsed here and both are NULL in SQLite, whose `getDigits` is called with
/// `"40f-21a-21d"` - four digits, then two, then two, and no other shape is a
/// date. A date function that accepts more spellings than the reference is a
/// migration that silently changes which rows a `WHERE` keeps.
fn parse_date(bytes: &[u8]) -> Option<(i64, i64, i64)> {
    let mut parts = bytes.split(|byte| *byte == b'-');
    let year = digits_wide(parts.next()?, 4)?;
    let month = digits_wide(parts.next()?, 2)?;
    let day = digits_wide(parts.next()?, 2)?;
    if parts.next().is_some() {
        return None;
    }
    (1..=12).contains(&month).then_some(())?;
    (1..=31).contains(&day).then_some(())?;
    Some((year, month, day))
}

/// Parses `HH:MM[:SS[.SSS]][Z|±HH:MM]`, returning the offset in minutes.
fn parse_time(bytes: &[u8]) -> Option<(i64, i64, f64, i64)> {
    let mut body = bytes;
    let mut offset = 0i64;
    if let Some(last) = body.last() {
        if *last == b'Z' || *last == b'z' {
            body = body.get(..body.len().saturating_sub(1))?;
        }
    }
    if let Some(at) = body.iter().rposition(|byte| *byte == b'+' || *byte == b'-') {
        // A sign after the first character is a zone offset; one at the start
        // is not a time at all.
        if at > 0 {
            let sign = if body.get(at) == Some(&b'-') { -1 } else { 1 };
            let zone = body.get(at.saturating_add(1)..)?;
            let mut halves = zone.split(|byte| *byte == b':');
            let hours = digits_wide(halves.next()?, 2)?;
            let minutes = halves.next().map_or(Some(0), |held| digits_wide(held, 2))?;
            offset = sign * (hours * 60 + minutes);
            body = body.get(..at)?;
        }
    }
    // Two digits each, which is what SQLite's `"20c:20e"` and `"20g"` ask for.
    // The fraction after the seconds is the one field whose width is free.
    let mut parts = body.split(|byte| *byte == b':');
    let hour = digits_wide(parts.next()?, 2)?;
    let minute = digits_wide(parts.next()?, 2)?;
    let second = match parts.next() {
        Some(text) => {
            let mut halves = text.splitn(2, |byte| *byte == b'.');
            let whole = digits_wide(halves.next()?, 2)? as f64;
            let fraction = match halves.next() {
                Some(digits_after) => {
                    let value = digits(digits_after)? as f64;
                    let scale = 10f64.powi(digits_after.len() as i32);
                    value / scale
                }
                None => 0.0,
            };
            whole + fraction
        }
        None => 0.0,
    };
    if parts.next().is_some() {
        return None;
    }
    (hour <= 24 && minute <= 59 && second < 60.0).then_some(())?;
    Some((hour, minute, second, offset))
}

/// Parses exactly `width` ASCII digits.
///
/// @param bytes - the field as it was written
/// @param width - how many digits the field must have
fn digits_wide(bytes: &[u8], width: usize) -> Option<i64> {
    (bytes.len() == width).then_some(())?;
    digits(bytes)
}

/// Parses a run of ASCII digits.
fn digits(bytes: &[u8]) -> Option<i64> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut value = 0i64;
    for byte in bytes {
        value = value
            .checked_mul(10)?
            .checked_add(i64::from(byte.saturating_sub(b'0')))?;
    }
    Some(value)
}

/// The Julian day of 1970-01-01 00:00 UTC, which is where Unix time starts.
pub const UNIX_EPOCH_JULIAN_DAY: f64 = 2_440_587.5;

/// Returns the civil date a Unix day number names.
///
/// **The one implementation of this in the workspace (task-1961, A10).** There
/// were three more - `inillucent-cli`'s `archive.rs`, `inillucent-ext`'s
/// `zipfile.rs` and the `perfhistory` harness - each a private copy of Howard
/// Hinnant's `civil_from_days` answering a bare `(i64, i64, i64)` that a caller
/// had to read in the right order. The date functions in this module had
/// already been doing the same arithmetic, with a named result and a round trip
/// test against [`julian_of`], since before any of them was written.
///
/// @param days - days since 1970-01-01, which may be negative
pub fn civil_of_unix_day(days: i64) -> Civil {
    civil_of(days as f64 + UNIX_EPOCH_JULIAN_DAY)
}

/// Returns the Julian day of a civil date and time.
///
/// The formula is the standard one for the proleptic Gregorian calendar, and
/// the half-day is the reason a Julian day starts at noon.
pub fn julian_of(civil: Civil) -> f64 {
    let (mut year, mut month) = (civil.year, civil.month);
    if month <= 2 {
        year -= 1;
        month += 12;
    }
    let a = year.div_euclid(100);
    let b = 2 - a + a.div_euclid(4);
    let days = (365.25 * ((year + 4716) as f64)).floor()
        + (30.6001 * ((month + 1) as f64)).floor()
        + civil.day as f64
        + b as f64
        - 1524.5;
    days + (civil.hour as f64) / 24.0
        + (civil.minute as f64) / 1440.0
        + civil.second / SECONDS_PER_DAY
}

/// Returns the civil date and time of a Julian day.
///
/// @param day - the Julian day, where 2440587.5 is 1970-01-01 00:00 UTC
pub fn civil_of(day: f64) -> Civil {
    let shifted = day + 0.5;
    let z = shifted.floor();
    let fraction = shifted - z;
    let z = z as i64;
    // The correction is applied at every date, not only after 1582. SQLite's
    // calendar is the *proleptic* Gregorian one, so a date in 1200 is the
    // Gregorian date rather than the Julian one people used at the time - and
    // switching calendars here while `julian_of` did not made the two
    // directions disagree by the seven days between them.
    let alpha = ((z as f64 - 1867216.25) / 36524.25).floor() as i64;
    let a = z + 1 + alpha - alpha.div_euclid(4);
    let b = a + 1524;
    let c = ((b as f64 - 122.1) / 365.25).floor() as i64;
    let d = (365.25 * c as f64).floor() as i64;
    let e = ((b - d) as f64 / 30.6001).floor() as i64;
    let day_of_month = b - d - (30.6001 * e as f64).floor() as i64;
    let month = if e < 14 { e - 1 } else { e - 13 };
    let year = if month > 2 { c - 4716 } else { c - 4715 };
    // The seconds are rounded to the millisecond before they are broken up,
    // because a value that is 59.9999999 seconds past the minute renders as
    // ":60" otherwise - a time that does not exist.
    let mut seconds = (fraction * SECONDS_PER_DAY * 1000.0).round() / 1000.0;
    let mut hour = (seconds / 3600.0).floor() as i64;
    seconds -= (hour as f64) * 3600.0;
    let mut minute = (seconds / 60.0).floor() as i64;
    seconds -= (minute as f64) * 60.0;
    if minute >= 60 {
        minute -= 60;
        hour += 1;
    }
    Civil {
        year,
        month,
        day: day_of_month,
        hour,
        minute,
        second: seconds,
    }
}

/// Applies one modifier to a moment.
///
/// Every modifier but `subsec` makes the spelled fields stale, which is what
/// carries an hour 24 into the next day the moment anything else is asked for -
/// SQLite's rule as well as this one.
///
/// @param moment - the moment so far
/// @param modifier - the modifier, as written
fn apply_modifier(moment: Moment, modifier: &[u8]) -> Option<Moment> {
    let day = moment.day;
    let folded = trim(modifier).to_ascii_lowercase();
    if folded == b"subsec" || folded == b"subsecond" {
        // Read by `unixepoch` and by the renderer rather than applied, so the
        // fields it was given are still the fields it has.
        return Some(moment);
    }
    if folded == b"julianday" {
        return Some(moment.moved_to(day));
    }
    if folded == b"unixepoch" {
        return Some(moment.moved_to(UNIX_EPOCH_JD + day / SECONDS_PER_DAY));
    }
    if folded == b"auto" {
        // A value large enough to be a unix timestamp is one; anything else is
        // already a Julian day. SQLite's own threshold.
        if day > PAST_LAST_JULIAN_DAY {
            return Some(moment.moved_to(UNIX_EPOCH_JD + day / SECONDS_PER_DAY));
        }
        return Some(moment.moved_to(day));
    }
    if folded == b"localtime" {
        if moment.zone == Zone::Local {
            return Some(moment);
        }
        let mut moved = moment.moved_to(day + zone_offset_days(day)?);
        moved.zone = Zone::Local;
        return Some(moved);
    }
    if folded == b"utc" {
        if moment.zone == Zone::Utc {
            return Some(moment);
        }
        // Two steps, as SQLite does it: the offset is asked for again at the
        // instant the first step landed on, so a conversion that crosses a
        // daylight saving boundary uses the offset in force on the far side.
        let first = zone_offset_days(day)?;
        let second = zone_offset_days(day - first)?;
        let mut moved = moment.moved_to(day - second);
        moved.zone = Zone::Utc;
        return Some(moved);
    }
    if folded == b"ceiling" {
        // The default, so it only forgets what a `floor` would have taken off.
        return Some(Moment {
            day,
            spelled: None,
            overflow: 0,
            zone: moment.zone,
        });
    }
    if folded == b"floor" {
        return Some(Moment {
            day: day - moment.overflow as f64,
            spelled: None,
            overflow: 0,
            zone: moment.zone,
        });
    }
    if let Some(rest) = folded.strip_prefix(b"start of ") {
        let mut civil = civil_of(day);
        civil.hour = 0;
        civil.minute = 0;
        civil.second = 0.0;
        match rest {
            b"day" => {}
            b"month" => civil.day = 1,
            b"year" => {
                civil.month = 1;
                civil.day = 1;
            }
            _ => return None,
        }
        return Some(moment.moved_to(julian_of(civil)));
    }
    if let Some(rest) = folded.strip_prefix(b"weekday ") {
        let wanted = digits(trim(rest))?;
        if wanted > 6 {
            return None;
        }
        // Julian day 0 is a Monday, so day-of-week is `(jd + 1.5) mod 7` with
        // Sunday as zero - SQLite's numbering.
        let current = ((day + 1.5).floor() as i64).rem_euclid(7);
        let forward = (wanted - current).rem_euclid(7);
        let mut civil = civil_of(day + forward as f64);
        civil.hour = 0;
        civil.minute = 0;
        civil.second = 0.0;
        return Some(moment.moved_to(julian_of(civil)));
    }
    apply_offset(moment, &folded)
}

/// Returns the local zone's offset from UTC at one Julian day, in days.
///
/// `None` when the operating system will not convert that instant, which is
/// what makes `localtime` and `utc` answer NULL for a date outside the range
/// the platform's own conversion accepts rather than guessing an offset.
///
/// @param day - the instant, as a Julian day
fn zone_offset_days(day: f64) -> Option<f64> {
    let seconds = whole_seconds((day - UNIX_EPOCH_JD) * SECONDS_PER_DAY);
    let offset = inillucent_vfs::zone::local_offset_seconds(seconds)?;
    Some(offset as f64 / SECONDS_PER_DAY)
}

/// Returns how many days past the end of its month a date reaches.
///
/// **What a `floor` modifier takes off (task-1979, F14).** A month or year
/// modifier lands on a day of the month the new month may not have: one month
/// after 31 January is 31 February, which carries into March. `ceiling`, the
/// default, keeps the carry; `floor` asks for the last day of the month
/// instead, and this is the difference between the two. 31 February is two days
/// past a leap February and three past a short one, so
/// `date('2020-01-31','+1 month','floor')` is 29 February 2020 and
/// `date('2021-01-31','+1 month','floor')` is 28 February 2021.
///
/// @param civil - the date the modifier built, before the carry is applied
fn days_past_the_month(civil: Civil) -> i64 {
    if civil.day <= 28 {
        return 0;
    }
    // The months with 31 days. A day of the month never exceeds 31, so nothing
    // reaches past one of them.
    if matches!(civil.month, 1 | 3 | 5 | 7 | 8 | 10 | 12) {
        return 0;
    }
    if civil.month != 2 {
        return i64::from(civil.day == 31);
    }
    let leap = civil.year.rem_euclid(4) == 0
        && (civil.year.rem_euclid(100) != 0 || civil.year.rem_euclid(400) == 0);
    match leap {
        true => civil.day - 29,
        false => civil.day - 28,
    }
}

/// Applies a `±NNN unit` modifier.
///
/// @param moment - the moment so far
/// @param folded - the modifier, trimmed and lowercased
fn apply_offset(moment: Moment, folded: &[u8]) -> Option<Moment> {
    let day = moment.day;
    let at = folded.iter().position(|byte| *byte == b' ')?;
    let amount = folded.get(..at)?;
    let unit = trim(folded.get(at.saturating_add(1)..)?);
    let negative = amount.first() == Some(&b'-');
    let magnitude = if amount.first() == Some(&b'+') || negative {
        amount.get(1..)?
    } else {
        amount
    };
    if magnitude.is_empty() || !numeric::looks_numeric(magnitude, TextEncoding::Utf8) {
        return None;
    }
    let parsed = numeric::atof(magnitude, TextEncoding::Utf8).value;
    let signed = if negative { -parsed } else { parsed };
    let unit = unit.strip_suffix(b"s").unwrap_or(unit);
    match unit {
        b"day" => Some(moment.moved_to(day + signed)),
        b"hour" => Some(moment.moved_to(day + signed / 24.0)),
        b"minute" => Some(moment.moved_to(day + signed / 1440.0)),
        b"second" => Some(moment.moved_to(day + signed / SECONDS_PER_DAY)),
        // Months and years move the calendar rather than a fixed number of
        // days, and the day of the month is clamped the way SQLite clamps it:
        // one month after 31 January is 3 March in a non-leap year, because the
        // overflow carries rather than saturating. How far it carried is kept,
        // because a `floor` modifier after it asks for the last day of the
        // month instead - see `days_past_the_month`.
        b"month" => {
            let mut civil = civil_of(day);
            let total = civil.year * 12 + (civil.month - 1) + signed as i64;
            civil.year = total.div_euclid(12);
            civil.month = total.rem_euclid(12) + 1;
            Some(moved_by_calendar(moment, civil))
        }
        b"year" => {
            let mut civil = civil_of(day);
            civil.year += signed as i64;
            Some(moved_by_calendar(moment, civil))
        }
        _ => None,
    }
}

/// Returns the moment a calendar date names, with its carry recorded.
///
/// @param moment - the moment the modifier was applied to
/// @param civil - the date a month or year modifier built, which may name a day
///   the month does not have
fn moved_by_calendar(moment: Moment, civil: Civil) -> Moment {
    Moment {
        day: julian_of(civil),
        spelled: None,
        overflow: days_past_the_month(civil),
        zone: moment.zone,
    }
}

/// Renders one moment through a `strftime` format.
///
/// The specifiers that name a field of the calendar read `civil`, which is what
/// the input spelled when it spelled one; the specifiers that name a position
/// in time - `%s`, `%J`, the week and weekday numbers - read `day`. That split
/// is SQLite's, and it is what makes `strftime('%H','2020-01-01 24:00:00')`
/// answer `24` while `strftime('%s', ...)` answers the next day's midnight.
///
/// @param day - the Julian day
/// @param civil - the broken-down fields to read
/// @param format - the format string
/// @param years - how to write a year out; the two SQLite formatters differ
fn render(day: f64, civil: Civil, format: &[u8], years: YearStyle) -> Value<'static> {
    let mut out: Vec<u8> = Vec::new();
    let mut index = 0usize;
    while index < format.len() {
        let byte = format.get(index).copied().unwrap_or(0);
        index = index.saturating_add(1);
        if byte != b'%' {
            out.push(byte);
            continue;
        }
        let Some(code) = format.get(index).copied() else {
            out.push(b'%');
            break;
        };
        index = index.saturating_add(1);
        match code {
            b'%' => out.push(b'%'),
            b'd' => push_padded(&mut out, civil.day, 2),
            b'e' => {
                let text = format!("{:2}", civil.day);
                out.extend_from_slice(text.as_bytes());
            }
            b'f' => {
                let text = format!("{:06.3}", civil.second);
                out.extend_from_slice(text.as_bytes());
            }
            b'F' => {
                push_year(&mut out, civil.year, years);
                let text = format!("-{:02}-{:02}", civil.month, civil.day);
                out.extend_from_slice(text.as_bytes());
            }
            b'H' => push_padded(&mut out, civil.hour, 2),
            b'I' => {
                let hour = match civil.hour % 12 {
                    0 => 12,
                    other => other,
                };
                push_padded(&mut out, hour, 2);
            }
            b'j' => push_padded(&mut out, day_of_year(civil), 3),
            b'J' => {
                // SQLite prints this one with sixteen significant digits, which
                // is not what the shortest round-trip rendering gives:
                // `2460370.878553241` against `2460370.8785532406`.
                out.extend_from_slice(sixteen_significant(day).as_bytes());
            }
            // The space-padded hours, which were being echoed back as `%k` and
            // `%l`. Fixing one member of a specifier family and assuming the
            // rest works is exactly how bugs in the others stay hidden, so the
            // whole table was walked against the reference rather than just
            // the ones that were reported.
            b'k' => {
                let text = format!("{:2}", civil.hour);
                out.extend_from_slice(text.as_bytes());
            }
            b'l' => {
                let hour = match civil.hour % 12 {
                    0 => 12,
                    other => other,
                };
                let text = format!("{hour:2}");
                out.extend_from_slice(text.as_bytes());
            }
            b'g' => push_padded(&mut out, iso_week(day).0.rem_euclid(100), 2),
            b'm' => push_padded(&mut out, civil.month, 2),
            b'M' => push_padded(&mut out, civil.minute, 2),
            b'p' => out.extend_from_slice(if civil.hour < 12 { b"AM" } else { b"PM" }),
            b'P' => out.extend_from_slice(if civil.hour < 12 { b"am" } else { b"pm" }),
            b'R' => {
                let text = format!("{:02}:{:02}", civil.hour, civil.minute);
                out.extend_from_slice(text.as_bytes());
            }
            b's' => {
                let seconds = whole_seconds((day - UNIX_EPOCH_JD) * SECONDS_PER_DAY);
                out.extend_from_slice(seconds.to_string().as_bytes());
            }
            b'S' => push_padded(&mut out, civil.second.floor() as i64, 2),
            b'T' => {
                let text = format!(
                    "{:02}:{:02}:{:02}",
                    civil.hour,
                    civil.minute,
                    civil.second.floor() as i64
                );
                out.extend_from_slice(text.as_bytes());
            }
            b'u' => {
                let weekday = weekday(day);
                push_padded(&mut out, if weekday == 0 { 7 } else { weekday }, 1);
            }
            b'w' => push_padded(&mut out, weekday(day), 1),
            b'U' => push_padded(&mut out, week_from(civil, days_after_sunday(day)), 2),
            b'V' => push_padded(&mut out, iso_week(day).1, 2),
            b'W' => push_padded(&mut out, week_from(civil, days_after_monday(day)), 2),
            b'G' => {
                let text = format!("{:04}", iso_week(day).0);
                out.extend_from_slice(text.as_bytes());
            }
            b'Y' => push_year(&mut out, civil.year, years),
            // **A specifier SQLite does not have makes the whole call NULL**,
            // rather than putting the two characters back. `strftime('%y', d)`
            // is NULL in SQLite and was the literal text `%y` here, which is a
            // format string silently half-applied.
            _ => return Value::Null,
        }
    }
    Value::owned_text(&out).unwrap_or(Value::Null)
}

/// Renders a double the way C's `%.16g` does.
///
/// Sixteen significant digits, trailing zeros removed, and the fixed form for
/// an exponent in the range a Julian day lives in. Only `%J` needs it, and it
/// needs it exactly: the shortest round-trip rendering Rust gives has
/// seventeen digits for the same value.
///
/// @param value - the number to render
fn sixteen_significant(value: f64) -> String {
    if !value.is_finite() || value == 0.0 {
        return format!("{value}");
    }
    let exponent = value.abs().log10().floor() as i32;
    if !(-5..16).contains(&exponent) {
        return format!("{value}");
    }
    let decimals = (15 - exponent).max(0) as usize;
    let text = format!("{value:.decimals$}");
    if !text.contains('.') {
        return text;
    }
    let trimmed = text.trim_end_matches('0');
    trimmed.trim_end_matches('.').to_string()
}

/// Appends a year.
///
/// **The two SQLite formatters differ on a negative year (task-1979, F23).**
/// `date('0000-01-01','-1 day')` is `-0001-12-31` because `dateFunc` writes the
/// sign and then four digits of its own; `strftime('%Y', ...)` on the same
/// value is `-001` because it hands the year to C's `%04d`, which counts the
/// sign against the width. Both were checked against 3.53.4. This module
/// answered `-001` for both, which was right for one of them.
///
/// @param out - the buffer being built
/// @param year - the year, which may be negative
/// @param style - which of the two formatters is asking
fn push_year(out: &mut Vec<u8>, year: i64, style: YearStyle) {
    let text = match style {
        YearStyle::Printf => format!("{year:04}"),
        YearStyle::Signed => match year < 0 {
            true => format!("-{:04}", year.saturating_abs()),
            false => format!("{year:04}"),
        },
    };
    out.extend_from_slice(text.as_bytes());
}

/// Appends a zero-padded number.
fn push_padded(out: &mut Vec<u8>, value: i64, width: usize) {
    let text = format!("{value:0width$}");
    out.extend_from_slice(text.as_bytes());
}

/// Returns the day of the week, Sunday as zero.
fn weekday(day: f64) -> i64 {
    ((day + 1.5).floor() as i64).rem_euclid(7)
}

/// Returns the day of the year, 1 January as one.
fn day_of_year(civil: Civil) -> i64 {
    let start = julian_of(Civil {
        year: civil.year,
        month: 1,
        day: 1,
        hour: 0,
        minute: 0,
        second: 0.0,
    });
    let here = julian_of(Civil {
        hour: 0,
        minute: 0,
        second: 0.0,
        ..civil
    });
    (here - start) as i64 + 1
}

/// Returns how many days have passed since the first of January.
///
/// Zero on the first, which is what SQLite's `daysAfterJan01` counts. Our
/// `day_of_year` is one-based, because `%j` is.
fn days_after_jan01(civil: Civil) -> i64 {
    day_of_year(civil).saturating_sub(1)
}

/// Returns how many days have passed since the week's Monday, Monday as zero.
fn days_after_monday(day: f64) -> i64 {
    (weekday(day) + 6).rem_euclid(7)
}

/// Returns how many days have passed since the week's Sunday, Sunday as zero.
fn days_after_sunday(day: f64) -> i64 {
    weekday(day)
}

/// Returns a week number counted from the year's first Monday or first Sunday.
///
/// **SQLite's own arithmetic, transcribed.** `%W` counts weeks whose first day
/// is Monday and `%U` weeks whose first day is Sunday; in both, the days before
/// the year's first such day are week 00 and the first is week 01. The previous
/// implementation counted from the first *Sunday* for `%W` and was off by one
/// besides, which put `strftime('%Y-%W','2024-03-01')` at `2024-08` against the
/// reference's `2024-09`.
///
/// @param civil - the instant, as a date
/// @param days_after_start - days since the week's first day
fn week_from(civil: Civil, days_after_start: i64) -> i64 {
    (days_after_jan01(civil) - days_after_start + 7) / 7
}

/// Returns the ISO-8601 week-numbering year and week of a date.
///
/// The week a date belongs to is the week holding its Thursday, and the year is
/// that Thursday's - which is why the two have to be computed together and why
/// `2023-01-01` is week 52 of 2022.
///
/// **A Julian day starts at noon, so `floor` is the wrong midnight.** For a
/// timestamp before noon it lands on the previous calendar day, and the week
/// computed from it was a day early: `strftime('%V %G','2026-01-04')` answered
/// week 53 of 2025 where SQLite answers week 01 of 2026, while the same date at
/// 13:00 answered correctly - which is why this survived every case that
/// carried a time. The calendar day's own midnight is `(day + 0.5).floor()
/// - 0.5`, the basis `weekday` already counts from.
///
/// @param day - the julian day
fn iso_week(day: f64) -> (i64, i64) {
    let midnight = (day + 0.5).floor() - 0.5;
    let thursday = midnight + (3 - days_after_monday(day)) as f64;
    let moved = civil_of(thursday);
    (moved.year, days_after_jan01(moved) / 7 + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Julian day of the unix epoch is the constant everything else is
    /// derived from, so it is worth pinning on its own.
    #[test]
    fn the_epoch_is_where_it_should_be() {
        let civil = Civil {
            year: 1970,
            month: 1,
            day: 1,
            hour: 0,
            minute: 0,
            second: 0.0,
        };
        assert!((julian_of(civil) - UNIX_EPOCH_JD).abs() < 1e-9);
    }

    /// Every civil date round-trips through the Julian day and back.
    #[test]
    fn civil_dates_round_trip() {
        for (year, month, day) in [
            (1970, 1, 1),
            (2000, 2, 29),
            (1999, 12, 31),
            (2026, 9, 3),
            (1582, 10, 15),
            (1200, 6, 6),
        ] {
            let civil = Civil {
                year,
                month,
                day,
                hour: 13,
                minute: 45,
                second: 30.0,
            };
            let back = civil_of(julian_of(civil));
            assert_eq!(
                (back.year, back.month, back.day, back.hour, back.minute),
                (year, month, day, 13, 45),
                "{year}-{month}-{day}"
            );
        }
    }
}
