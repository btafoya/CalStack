//! Hand-rolled RFC 5545 recurrence expansion (ADR-012).
//!
//! Own engine over the supported subset: FREQ (DAILY/WEEKLY/MONTHLY/YEARLY),
//! INTERVAL, COUNT, UNTIL, BYDAY (with MONTHLY ordinals), BYMONTHDAY, BYMONTH.
//! Occurrences are generated in the event's wall-clock time so DST transitions
//! behave the way clients expect, then converted to UTC instants. Expansion is
//! bounded: a hard step cap plus the query window bounds unbounded RRULEs.

use chrono::{
    DateTime, Datelike, LocalResult, NaiveDate, NaiveDateTime, Offset, TimeZone, Utc, Weekday,
};
use chrono_tz::Tz;

use crate::DateOrDateTime;

#[derive(Debug, thiserror::Error)]
pub enum RecurrenceError {
    #[error("unsupported RRULE: {0}")]
    Unsupported(String),
    #[error("unknown timezone: {0}")]
    UnknownTimezone(String),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Freq {
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

/// BYDAY entry: plain weekday, or weekday with an ordinal for MONTHLY
/// (e.g. `2MO` second Monday, `-1FR` last Friday).
#[derive(Debug, Clone, Copy, PartialEq)]
struct ByDay {
    weekday: Weekday,
    ordinal: Option<i32>,
}

#[derive(Debug, Clone)]
struct ParsedRRule {
    freq: Freq,
    interval: u32,
    count: Option<u32>,
    until_instant: Option<DateTime<Utc>>,
    until_date: Option<NaiveDate>,
    by_day: Vec<ByDay>,
    by_monthday: Vec<i32>,
    by_month: Vec<u32>,
}

fn weekday_from_str(s: &str) -> Option<Weekday> {
    Some(match s {
        "MO" => Weekday::Mon,
        "TU" => Weekday::Tue,
        "WE" => Weekday::Wed,
        "TH" => Weekday::Thu,
        "FR" => Weekday::Fri,
        "SA" => Weekday::Sat,
        "SU" => Weekday::Sun,
        _ => return None,
    })
}

fn parse_ints(val: &str) -> Result<Vec<i32>, RecurrenceError> {
    val.split(',')
        .map(|d| {
            d.parse()
                .map_err(|_| RecurrenceError::Unsupported(val.into()))
        })
        .collect()
}

impl ParsedRRule {
    fn parse(value: &str) -> Result<Self, RecurrenceError> {
        let mut freq = None;
        let mut interval = 1u32;
        let mut count = None;
        let mut until_instant = None;
        let mut until_date = None;
        let mut by_day = Vec::new();
        let mut by_monthday = Vec::new();
        let mut by_month = Vec::new();
        for part in value.trim().split(';') {
            let Some((key, val)) = part.split_once('=') else {
                return Err(RecurrenceError::Unsupported(part.into()));
            };
            match key {
                "FREQ" => {
                    freq = Some(match val {
                        "DAILY" => Freq::Daily,
                        "WEEKLY" => Freq::Weekly,
                        "MONTHLY" => Freq::Monthly,
                        "YEARLY" => Freq::Yearly,
                        _ => return Err(RecurrenceError::Unsupported(format!("FREQ={val}"))),
                    });
                }
                "INTERVAL" => {
                    interval = val
                        .parse()
                        .map_err(|_| RecurrenceError::Unsupported(part.into()))?;
                    if interval == 0 {
                        return Err(RecurrenceError::Unsupported("INTERVAL=0".into()));
                    }
                }
                "COUNT" => {
                    count = Some(
                        val.parse()
                            .map_err(|_| RecurrenceError::Unsupported(part.into()))?,
                    )
                }
                "UNTIL" => {
                    // RFC 5545 compact UNTIL forms: RFC 3339, and the compact
                    // forms real clients send — `...Z` is a UTC instant, a
                    // bare local wall clock is bounded by day.
                    until_instant = DateTime::parse_from_rfc3339(val)
                        .ok()
                        .map(|d| d.with_timezone(&Utc))
                        .or_else(|| {
                            NaiveDateTime::parse_from_str(val, "%Y%m%dT%H%M%SZ")
                                .ok()
                                .map(|n| Utc.from_utc_datetime(&n))
                        });
                    until_date = NaiveDate::parse_from_str(val, "%Y%m%d").ok().or_else(|| {
                        NaiveDateTime::parse_from_str(val, "%Y%m%dT%H%M%S")
                            .ok()
                            .map(|n| n.date())
                    });
                    if until_instant.is_none() && until_date.is_none() {
                        return Err(RecurrenceError::Unsupported(format!("UNTIL={val}")));
                    }
                }
                "BYDAY" => {
                    for day in val.split(',') {
                        let (ordinal, weekday_part) = if day.len() > 2 {
                            let (n, wd) = day.split_at(day.len() - 2);
                            (n.parse::<i32>().ok(), wd)
                        } else {
                            (None, day)
                        };
                        let weekday = weekday_from_str(weekday_part)
                            .ok_or_else(|| RecurrenceError::Unsupported(part.into()))?;
                        by_day.push(ByDay { weekday, ordinal });
                    }
                }
                "BYMONTHDAY" => by_monthday = parse_ints(val)?,
                "BYMONTH" => {
                    let months = parse_ints(val)?;
                    if months.iter().any(|m| !(1..=12).contains(m)) {
                        return Err(RecurrenceError::Unsupported(format!("BYMONTH={val}")));
                    }
                    by_month = months.iter().map(|m| *m as u32).collect();
                }
                "WKST" => {} // accepted; week-start nuance not needed for this subset
                other => return Err(RecurrenceError::Unsupported(other.into())),
            }
        }
        Ok(Self {
            freq: freq.ok_or_else(|| RecurrenceError::Unsupported("missing FREQ".into()))?,
            interval,
            count,
            until_instant,
            until_date,
            by_day,
            by_monthday,
            by_month,
        })
    }
}

/// One compiled STANDARD/DAYLIGHT sub-component of a VTIMEZONE (ADR-012):
/// the transition anchor in wall-clock time and the offsets it switches
/// between. Stored as JSONB in the `timezones` table.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ZoneRule {
    /// DTSTART of the sub-component, in the zone's own wall clock
    /// (interpreted against `offset_from_secs`).
    pub dtstart: NaiveDateTime,
    /// TZOFFSETFROM: offset in effect just before this transition.
    pub offset_from_secs: i32,
    /// TZOFFSETTO: offset in effect from this transition onward.
    pub offset_to_secs: i32,
    /// RRULE for the recurrence of this transition, if any.
    pub rrule: Option<String>,
    /// Explicit RDATE transition points (wall clock, same basis as DTSTART).
    pub rdates: Vec<NaiveDateTime>,
}

/// Compile window around load time.
/// ponytail: transitions are compiled ±[`ZONE_WINDOW_YEARS`] (4-year window);
/// expansions queried outside it run on the window-edge offset. Widen the
/// constant if long-horizon alarms ever need more.
pub const ZONE_WINDOW_YEARS: i64 = 2;

/// Compiles VTIMEZONE rules into UTC offset transition pairs over
/// `[window_from, window_to]`. The first pair is the base offset in effect at
/// the window start (each rule's occurrence instants are
/// `wall_clock - offset_from`). Errors on rules the RRULE subset can't
/// represent — callers reject such VTIMEZONEs at PUT.
pub fn compile_zone(
    rules: &[ZoneRule],
    window_from: DateTime<Utc>,
    window_to: DateTime<Utc>,
) -> Result<Vec<(i64, i32)>, RecurrenceError> {
    // ponytail: candidate generation reuses the RRULE engine with UTC as a
    // pseudo-zone (transitions are wall-clock minus offset_from), so UNTIL
    // comparisons are off by the zone offset (<1 day) — irrelevant for
    // termination semantics.
    let mut raw: Vec<(i64, i32, i32)> = Vec::new(); // (utc, offset_to, offset_from)
    for rule in rules {
        let parsed = rule.rrule.as_deref().map(ParsedRRule::parse).transpose()?;
        let mut candidates = vec![rule.dtstart];
        if let Some(parsed) = parsed {
            let pseudo = Zone::Tz(chrono_tz::UTC);
            candidates.extend(expand_rrule(
                &parsed,
                rule.dtstart,
                &pseudo,
                window_to.naive_utc(),
            )?);
        }
        candidates.extend(rule.rdates.iter().copied());
        // Bound memory: keep in-window transitions plus, per rule, the single
        // latest pre-window transition (it fixes the offset carried into the
        // window — a fixed-offset zone's DTSTART may sit decades back).
        let mut last_before: Option<(i64, i32, i32)> = None;
        for naive in candidates {
            let instant = naive.and_utc() - chrono::Duration::seconds(rule.offset_from_secs as i64);
            let t = instant.timestamp();
            let entry = (t, rule.offset_to_secs, rule.offset_from_secs);
            if t > window_to.timestamp() {
                continue;
            }
            if t < window_from.timestamp() {
                last_before = Some(match last_before {
                    Some(prev) if prev.0 >= t => prev,
                    _ => entry,
                });
            } else {
                raw.push(entry);
            }
        }
        if let Some(entry) = last_before {
            raw.push(entry);
        }
    }
    if raw.is_empty() {
        return Err(RecurrenceError::Unsupported(
            "no transitions compile in the window".into(),
        ));
    }
    raw.sort_by_key(|(t, _, _)| *t);
    raw.dedup_by_key(|(t, _, _)| *t);
    // Offset in effect at the window start: the latest pre-window transition's
    // target offset, else the first transition's prior offset.
    let first_in_window = raw.partition_point(|(t, _, _)| *t < window_from.timestamp());
    let base = if first_in_window > 0 {
        raw[first_in_window - 1].1
    } else {
        raw[0].2
    };
    let mut out: Vec<(i64, i32)> = Vec::with_capacity(raw.len() - first_in_window + 1);
    out.push((window_from.timestamp(), base));
    out.extend(raw[first_in_window..].iter().map(|(t, to, _)| (*t, *to)));
    Ok(out)
}

/// tzid → UTC offset transitions for zones that are not in the tzdb
/// (client-supplied VTIMEZONEs, ADR-012). Each zone's list is sorted
/// `(utc_epoch_secs, offset_secs)`; the first entry is the base offset at the
/// compile window start.
#[derive(Debug, Clone, Default)]
pub struct TzResolver {
    zones: std::collections::HashMap<String, Vec<(i64, i32)>>,
}

impl TzResolver {
    pub fn insert(&mut self, tzid: impl Into<String>, transitions: Vec<(i64, i32)>) {
        self.zones.insert(tzid.into(), transitions);
    }

    pub fn get(&self, tzid: &str) -> Option<&Vec<(i64, i32)>> {
        self.zones.get(tzid)
    }
}

/// A resolved zone: either a tzdb zone or compiled custom transitions.
#[derive(Debug, Clone)]
pub enum Zone {
    Tz(Tz),
    Custom(Vec<(i64, i32)>),
}

impl Zone {
    /// Offset (seconds east of UTC) in effect at epoch second `secs`.
    fn offset_secs_at(&self, secs: i64) -> i32 {
        match self {
            Zone::Tz(tz) => Utc
                .timestamp_opt(secs, 0)
                .single()
                .map(|at| at.with_timezone(tz).offset().fix().local_minus_utc())
                .unwrap_or(0),
            Zone::Custom(transitions) => transitions
                .iter()
                .rev()
                .find(|(t, _)| *t <= secs)
                .map(|(_, o)| *o)
                .or_else(|| transitions.first().map(|(_, o)| *o))
                .unwrap_or(0),
        }
    }

    /// The earliest UTC instant mapping to this local wall clock. A
    /// nonexistent local time (spring-forward gap) maps to the transition
    /// boundary instead of `None` — custom zones are compiled data, and the
    /// tzdb variant keeps chrono's skip semantics.
    pub fn from_local(&self, naive: NaiveDateTime) -> Option<DateTime<Utc>> {
        match self {
            Zone::Tz(tz) => match tz.from_local_datetime(&naive) {
                LocalResult::Single(dt) => Some(dt.with_timezone(&Utc)),
                LocalResult::Ambiguous(earliest, _) => Some(earliest.with_timezone(&Utc)),
                LocalResult::None => None,
            },
            Zone::Custom(_) => {
                let t = naive.and_utc().timestamp();
                let before = self.offset_secs_at(t);
                let instant = t - before as i64;
                let after = self.offset_secs_at(instant);
                let best = if before == after {
                    instant
                } else {
                    instant.min(t - after as i64)
                };
                Utc.timestamp_opt(best, 0).single()
            }
        }
    }

    /// The local wall clock at this instant.
    pub fn to_local(&self, at: DateTime<Utc>) -> NaiveDateTime {
        match self {
            Zone::Tz(tz) => at.with_timezone(tz).naive_local(),
            Zone::Custom(_) => {
                let offset = self.offset_secs_at(at.timestamp());
                (at + chrono::Duration::seconds(offset as i64)).naive_utc()
            }
        }
    }
}

/// True when the tzid is a tzdb (IANA) zone. tzdb zones keep precedence over
/// stored VTIMEZONE definitions (IANA identity per the data rules).
pub fn is_tzdb_tzid(tzid: &str) -> bool {
    tzid.parse::<Tz>().is_ok()
}

/// Resolves a tzid to a zone. `custom` carries stored VTIMEZONE transitions
/// for client-supplied ids; a tzid that is neither tzdb nor resolvable is an
/// error — there is no silent UTC fallback (ADR-012). `None` (floating
/// events) is UTC by definition.
pub fn resolve_tz(
    tzid: Option<&str>,
    custom: Option<&TzResolver>,
) -> Result<Zone, RecurrenceError> {
    let Some(tzid) = tzid else {
        return Ok(Zone::Tz(chrono_tz::UTC));
    };
    if let Ok(tz) = tzid.parse::<Tz>() {
        return Ok(Zone::Tz(tz));
    }
    if let Some(transitions) = custom.and_then(|r| r.get(tzid)) {
        return Ok(Zone::Custom(transitions.clone()));
    }
    Err(RecurrenceError::UnknownTimezone(tzid.to_string()))
}

/// Week index in local calendar space (weeks are 7-day blocks from Monday).
fn week_index(date: NaiveDate) -> i64 {
    // days since Unix epoch, shifted so Monday starts the block
    (date.num_days_from_ce() - 1) as i64 / 7
}

fn days_in_month(year: i32, month: u32) -> u32 {
    let next_month = if month == 12 {
        NaiveDate::from_ymd_opt(year + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(year, month + 1, 1)
    };
    next_month
        .and_then(|next| next.pred_opt())
        .map_or(30, |last| last.day())
}

fn nth_weekday_of_month(
    year: i32,
    month: u32,
    weekday: Weekday,
    ordinal: i32,
) -> Option<NaiveDate> {
    if ordinal == 0 {
        return None;
    }
    let days = days_in_month(year, month);
    let matches: Vec<u32> = (1..=days)
        .filter(|d| {
            NaiveDate::from_ymd_opt(year, month, *d).is_some_and(|date| date.weekday() == weekday)
        })
        .collect();
    let index = if ordinal > 0 {
        (ordinal as usize).checked_sub(1)?
    } else {
        matches.len().checked_sub((-ordinal) as usize)?
    };
    let day = matches.get(index)?;
    NaiveDate::from_ymd_opt(year, month, *day)
}

fn matches_monthday(date: NaiveDate, rules: &[i32]) -> bool {
    rules.iter().any(|r| {
        if *r > 0 {
            date.day() as i32 == *r
        } else {
            let from_end = days_in_month(date.year(), date.month()) as i32 + 1 + r;
            date.day() as i32 == from_end
        }
    })
}

/// Expands one event's recurrence set (RRULE + RDATE, minus EXDATE) to the
/// occurrences whose UTC instant falls in `[window_from, window_to)`.
/// All-day events yield `AllDay` dates; timed events yield `Timed` instants.
/// `custom` resolves client-supplied tzids against stored VTIMEZONE
/// transitions; an unknown tzid is an error, never UTC (ADR-012).
#[allow(clippy::too_many_arguments)]
pub fn expand_occurrences(
    dtstart: DateOrDateTime,
    tzid: Option<&str>,
    custom: Option<&TzResolver>,
    rrule: Option<&str>,
    rdate: &[DateOrDateTime],
    exdate: &[DateOrDateTime],
    window_from: DateTime<Utc>,
    window_to: DateTime<Utc>,
) -> Result<Vec<DateOrDateTime>, RecurrenceError> {
    let all_day = matches!(dtstart, DateOrDateTime::AllDay(_));
    let zone = resolve_tz(tzid, custom)?;

    // Anchor in wall-clock space.
    let anchor: NaiveDateTime = match dtstart {
        DateOrDateTime::Timed(at) => zone.to_local(at),
        DateOrDateTime::AllDay(date) => date.and_hms_opt(0, 0, 0).expect("midnight is valid"),
    };

    let window_to_local = zone.to_local(window_to);

    let mut candidates: Vec<NaiveDateTime> = Vec::new();
    if let Some(rrule) = rrule {
        let rule = ParsedRRule::parse(rrule)?;
        candidates = expand_rrule(&rule, anchor, &zone, window_to_local)?;
    } else {
        candidates.push(anchor);
    }

    // RDATE adds explicit occurrences.
    for point in rdate {
        let naive = match point {
            DateOrDateTime::Timed(at) => zone.to_local(*at),
            DateOrDateTime::AllDay(date) => date.and_hms_opt(0, 0, 0).expect("midnight is valid"),
        };
        candidates.push(naive);
    }

    // EXDATE removes explicit occurrences; an all-day EXDATE removes the whole
    // local day.
    let ex_exact: Vec<NaiveDateTime> = exdate
        .iter()
        .filter_map(|p| match p {
            DateOrDateTime::Timed(at) => Some(zone.to_local(*at)),
            DateOrDateTime::AllDay(_) => None,
        })
        .collect();
    let ex_days: Vec<NaiveDate> = exdate
        .iter()
        .filter_map(|p| match p {
            DateOrDateTime::AllDay(date) => Some(*date),
            DateOrDateTime::Timed(_) => None,
        })
        .collect();

    let mut out: Vec<DateOrDateTime> = candidates
        .into_iter()
        .filter(|c| !ex_exact.contains(c) && !ex_days.contains(&c.date()))
        .filter_map(|candidate| {
            if all_day {
                NaiveDate::from_ymd_opt(candidate.year(), candidate.month(), candidate.day())
                    .map(DateOrDateTime::AllDay)
            } else {
                zone.from_local(candidate).map(DateOrDateTime::Timed)
            }
        })
        .collect();

    // Window filter and ordering.
    out.retain(|point| match point {
        DateOrDateTime::Timed(at) => *at >= window_from && *at < window_to,
        DateOrDateTime::AllDay(date) => {
            let Some(start_naive) = date.and_hms_opt(0, 0, 0) else {
                return false;
            };
            let end_naive = start_naive.checked_add_signed(chrono::Duration::days(1));
            let start = zone.from_local(start_naive).unwrap_or(window_from);
            let end = end_naive
                .and_then(|n| zone.from_local(n))
                .unwrap_or(window_to);
            end > window_from && start < window_to
        }
    });
    out.sort();
    out.dedup();
    Ok(out)
}

/// Candidate dates for one month of a MONTHLY/YEARLY rule: BYMONTHDAY
/// filters, BYDAY (with MONTHLY-style ordinals), or the anchor's day.
fn month_dates(rule: &ParsedRRule, year: i32, month: u32, anchor_day: u32) -> Vec<NaiveDate> {
    if !rule.by_monthday.is_empty() {
        (1..=days_in_month(year, month))
            .filter(|d| {
                NaiveDate::from_ymd_opt(year, month, *d)
                    .is_some_and(|date| matches_monthday(date, &rule.by_monthday))
            })
            .filter_map(|d| NaiveDate::from_ymd_opt(year, month, d))
            .collect()
    } else if !rule.by_day.is_empty() {
        let mut found: Vec<NaiveDate> = Vec::new();
        for b in &rule.by_day {
            match b.ordinal {
                Some(ordinal) => {
                    if let Some(date) = nth_weekday_of_month(year, month, b.weekday, ordinal) {
                        found.push(date);
                    }
                }
                None => {
                    // every matching weekday of the month
                    for d in 1..=days_in_month(year, month) {
                        if let Some(date) = NaiveDate::from_ymd_opt(year, month, d)
                            .filter(|date| date.weekday() == b.weekday)
                        {
                            found.push(date);
                        }
                    }
                }
            }
        }
        found
    } else {
        NaiveDate::from_ymd_opt(year, month, anchor_day)
            .into_iter()
            .collect()
    }
}

fn expand_rrule(
    rule: &ParsedRRule,
    anchor: NaiveDateTime,
    zone: &Zone,
    window_to_local: NaiveDateTime,
) -> Result<Vec<NaiveDateTime>, RecurrenceError> {
    let mut out: Vec<NaiveDateTime> = Vec::new();
    let mut emitted: u32 = 0;
    let horizon = 200_000u32; // ponytail: hard per-expansion step cap; config knob if abuse appears.
    let mut steps: u32 = 0;

    let accept =
        |candidate: NaiveDateTime, out: &mut Vec<NaiveDateTime>, emitted: &mut u32| -> bool {
            // Until/count checks; returns false to stop iteration.
            if rule.until_instant.is_some_and(|until| {
                zone.from_local(candidate)
                    .is_none_or(|instant| instant > until)
            }) {
                return false;
            }
            if rule
                .until_date
                .is_some_and(|until| candidate.date() > until)
            {
                return false;
            }
            *emitted += 1;
            out.push(candidate);
            rule.count.is_none_or(|c| *emitted < c)
        };

    // DTSTART is always the first occurrence (RFC 5545).
    if anchor <= window_to_local && rule.until_instant.is_none() && rule.until_date.is_none() {
        out.push(anchor);
        emitted += 1;
        if rule.count == Some(1) {
            return Ok(out);
        }
    } else if anchor <= window_to_local {
        let fits_until = rule.until_instant.is_none_or(|until| {
            zone.from_local(anchor)
                .is_none_or(|instant| instant <= until)
        });
        if fits_until {
            out.push(anchor);
            emitted += 1;
            if rule.count.is_some_and(|c| c <= 1) {
                return Ok(out);
            }
        }
    }

    let time_of_day = anchor.time();
    match rule.freq {
        Freq::Daily | Freq::Weekly => {
            // Weekly defaults to DTSTART's weekday when BYDAY is absent;
            // DAILY matches every day unless BYDAY/BYMONTHDAY filters exist.
            let weekday_filter: Option<Vec<Weekday>> = match rule.freq {
                Freq::Weekly => Some(if rule.by_day.is_empty() {
                    vec![anchor.weekday()]
                } else {
                    rule.by_day.iter().map(|b| b.weekday).collect()
                }),
                _ if rule.by_day.is_empty() => None,
                _ => Some(rule.by_day.iter().map(|b| b.weekday).collect()),
            };
            let start_week = week_index(anchor.date());
            let mut day = anchor.date();
            loop {
                steps += 1;
                if steps > horizon {
                    break;
                }
                let Some(next) = day.succ_opt() else { break };
                day = next;
                let candidate = day.and_time(time_of_day);
                if candidate > window_to_local {
                    break;
                }
                if !rule.by_month.is_empty() && !rule.by_month.contains(&day.month()) {
                    continue;
                }
                if !rule.by_monthday.is_empty() && !matches_monthday(day, &rule.by_monthday) {
                    continue;
                }
                let period_ok = match rule.freq {
                    Freq::Weekly => {
                        let week = week_index(day);
                        (week - start_week) % rule.interval as i64 == 0
                    }
                    _ => (day - anchor.date()).num_days() % rule.interval as i64 == 0,
                };
                if !period_ok
                    || weekday_filter
                        .as_ref()
                        .is_some_and(|weekdays| !weekdays.contains(&day.weekday()))
                {
                    continue;
                }
                if !accept(candidate, &mut out, &mut emitted) {
                    break;
                }
            }
        }
        Freq::Monthly => {
            let start_index = anchor.year() as i64 * 12 + anchor.month() as i64 - 1;
            let mut index = start_index;
            loop {
                steps += 1;
                if steps > horizon {
                    break;
                }
                index += rule.interval as i64;
                let year = (index / 12) as i32;
                let month = (index % 12) as u32 + 1;
                if year > window_to_local.year() + 1 {
                    break;
                }
                if !rule.by_month.is_empty() && !rule.by_month.contains(&month) {
                    continue;
                }
                for date in month_dates(rule, year, month, anchor.day()) {
                    let candidate = date.and_time(time_of_day);
                    if candidate > window_to_local {
                        return Ok(out);
                    }
                    if !accept(candidate, &mut out, &mut emitted) {
                        return Ok(out);
                    }
                }
                if year > window_to_local.year() {
                    break;
                }
            }
        }
        Freq::Yearly => {
            let mut year = anchor.year();
            loop {
                steps += 1;
                if steps > horizon {
                    break;
                }
                year += rule.interval as i32;
                if year > window_to_local.year() {
                    break;
                }
                let months: Vec<u32> = if rule.by_month.is_empty() {
                    vec![anchor.month()]
                } else {
                    rule.by_month.clone()
                };
                for month in months {
                    for date in month_dates(rule, year, month, anchor.day()) {
                        let candidate = date.and_time(time_of_day);
                        if candidate > window_to_local {
                            return Ok(out);
                        }
                        if !accept(candidate, &mut out, &mut emitted) {
                            return Ok(out);
                        }
                    }
                }
            }
        }
    }
    Ok(out)
}

/// Truncate a recurring rule at a split point ("this and following"):
/// keeps the occurrences in `before` (already expanded) and rewrites COUNT
/// or UNTIL so the rule yields exactly those. Other parts (BYDAY, INTERVAL,
/// …) are preserved verbatim. Returns None when nothing remains before the
/// split point — the master stops recurring and should become a plain
/// single event.
pub fn truncate_rrule(rrule: &str, before: &[DateOrDateTime], all_day: bool) -> Option<String> {
    let last = before.last()?;
    let mut kept: Vec<String> = Vec::new();
    let mut had_count = false;
    for part in rrule.split(';') {
        let key = part.split('=').next().unwrap_or(part);
        match key {
            "COUNT" => had_count = true,
            "UNTIL" => {}
            _ => kept.push(part.to_string()),
        }
    }
    if had_count {
        kept.push(format!("COUNT={}", before.len()));
    } else {
        let until = match last {
            DateOrDateTime::AllDay(date) => date.format("%Y%m%d").to_string(),
            // UNTIL is always UTC for DTSTART-with-time (RFC 5545 §3.8.5.3);
            // instants in `before` are already UTC.
            DateOrDateTime::Timed(at) => {
                if all_day {
                    at.format("%Y%m%d").to_string()
                } else {
                    at.format("%Y%m%dT%H%M%SZ").to_string()
                }
            }
        };
        kept.push(format!("UNTIL={until}"));
    }
    Some(kept.join(";"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Timelike};

    fn expand(
        rrule: &str,
        start: &str,
        tzid: Option<&str>,
        from: &str,
        to: &str,
    ) -> Vec<DateTime<Utc>> {
        let dtstart = DateOrDateTime::Timed(
            DateTime::parse_from_rfc3339(start)
                .unwrap()
                .with_timezone(&Utc),
        );
        let result = expand_occurrences(
            dtstart,
            tzid,
            None,
            Some(rrule),
            &[],
            &[],
            DateTime::parse_from_rfc3339(from)
                .unwrap()
                .with_timezone(&Utc),
            DateTime::parse_from_rfc3339(to)
                .unwrap()
                .with_timezone(&Utc),
        )
        .unwrap();
        result
            .into_iter()
            .map(|p| match p {
                DateOrDateTime::Timed(at) => at,
                _ => panic!("timed expected"),
            })
            .collect()
    }

    fn daily(start: &str, rrule_extra: &str, from: &str, to: &str) -> Vec<DateTime<Utc>> {
        expand(&format!("FREQ=DAILY{rrule_extra}"), start, None, from, to)
    }

    #[test]
    fn daily_every_interval() {
        let out = daily(
            "2026-01-01T09:00:00Z",
            "",
            "2026-01-01T00:00:00Z",
            "2026-01-06T00:00:00Z",
        );
        let hours: Vec<_> = out.iter().map(|d| d.day()).collect();
        assert_eq!(hours, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn daily_interval_2() {
        let out = daily(
            "2026-01-01T09:00:00Z",
            ";INTERVAL=2",
            "2026-01-01T00:00:00Z",
            "2026-01-08T00:00:00Z",
        );
        let days: Vec<_> = out.iter().map(|d| d.day()).collect();
        assert_eq!(days, vec![1, 3, 5, 7]);
    }

    #[test]
    fn count_limits() {
        let out = daily(
            "2026-01-01T09:00:00Z",
            ";COUNT=3",
            "2026-01-01T00:00:00Z",
            "2026-03-01T00:00:00Z",
        );
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn until_stops_inclusive() {
        let out = daily(
            "2026-01-01T09:00:00Z",
            ";UNTIL=2026-01-04T09:00:00Z",
            "2026-01-01T00:00:00Z",
            "2026-03-01T00:00:00Z",
        );
        assert_eq!(out.len(), 4);
        assert_eq!(out.last().unwrap().day(), 4);
    }

    #[test]
    fn weekly_byday_skips_weekend() {
        // 2026-01-05 is a Monday
        let out = expand(
            "FREQ=WEEKLY;BYDAY=MO,WE,FR",
            "2026-01-05T09:00:00Z",
            None,
            "2026-01-01T00:00:00Z",
            "2026-01-12T00:00:00Z",
        );
        let days: Vec<_> = out.iter().map(|d| d.day()).collect();
        assert_eq!(days, vec![5, 7, 9]);
    }

    #[test]
    fn weekly_interval_2() {
        // every second week, Mondays only
        let out = expand(
            "FREQ=WEEKLY;INTERVAL=2;BYDAY=MO",
            "2026-01-05T09:00:00Z",
            None,
            "2026-01-01T00:00:00Z",
            "2026-02-01T00:00:00Z",
        );
        let days: Vec<_> = out.iter().map(|d| d.day()).collect();
        assert_eq!(days, vec![5, 19]);
    }

    #[test]
    fn monthly_same_day() {
        let out = expand(
            "FREQ=MONTHLY",
            "2026-01-31T12:00:00Z",
            None,
            "2026-01-01T00:00:00Z",
            "2026-06-01T00:00:00Z",
        );
        let pairs: Vec<_> = out.iter().map(|d| (d.month(), d.day())).collect();
        // Jan 31, Mar 31, Apr 30? No: same day-of-month — Feb has no 31st, skipped.
        assert_eq!(pairs, vec![(1, 31), (3, 31), (5, 31)]);
    }

    #[test]
    fn monthly_ordinal_weekday() {
        // second Monday of each month
        let out = expand(
            "FREQ=MONTHLY;BYDAY=2MO",
            "2026-01-12T10:00:00Z",
            None,
            "2026-01-01T00:00:00Z",
            "2026-04-01T00:00:00Z",
        );
        let dates: Vec<_> = out.iter().map(|d| (d.month(), d.day())).collect();
        assert_eq!(dates, vec![(1, 12), (2, 9), (3, 9)]);
    }

    #[test]
    fn yearly_by_month() {
        let out = expand(
            "FREQ=YEARLY;BYMONTH=6;BYMONTHDAY=15",
            "2026-06-15T08:00:00Z",
            None,
            "2026-01-01T00:00:00Z",
            "2029-01-01T00:00:00Z",
        );
        let dates: Vec<_> = out.iter().map(|d| (d.year(), d.month(), d.day())).collect();
        assert_eq!(dates, vec![(2026, 6, 15), (2027, 6, 15), (2028, 6, 15)]);
    }

    #[test]
    fn negative_monthday() {
        let out = expand(
            "FREQ=MONTHLY;BYMONTHDAY=-1",
            "2026-01-31T09:00:00Z",
            None,
            "2026-01-01T00:00:00Z",
            "2026-04-01T00:00:00Z",
        );
        let days: Vec<_> = out.iter().map(|d| (d.month(), d.day())).collect();
        assert_eq!(days, vec![(1, 31), (2, 28), (3, 31)]);
    }

    #[test]
    fn dst_shifts_instant_not_wall_clock() {
        // 09:00 America/Denver wall clock (16:00Z in January, MST).
        let out = expand(
            "FREQ=MONTHLY",
            "2026-01-15T16:00:00Z",
            Some("America/Denver"),
            "2026-01-01T00:00:00Z",
            "2026-05-01T00:00:00Z",
        );
        // 09:00 local stays 09:00 local across the DST change (March 8, 2026).
        let offsets: Vec<i32> = out
            .iter()
            .map(|d| (Timelike::hour(&d.with_timezone(&chrono_tz::America::Denver)) == 9) as i32)
            .collect();
        assert_eq!(offsets, vec![1, 1, 1, 1]);
        let utc_hours: Vec<u32> = out.iter().map(Timelike::hour).collect();
        assert_eq!(utc_hours, vec![16, 16, 15, 15]); // UTC hour shifts with DST
    }

    #[test]
    fn unsupported_rrule_rejected() {
        assert!(ParsedRRule::parse("FREQ=MINUTELY").is_err());
        assert!(ParsedRRule::parse("INTERVAL=2").is_err());
        assert!(ParsedRRule::parse("FREQ=DAILY;BYSETPOS=1").is_err());
    }

    #[test]
    fn unbounded_rrule_bounded_by_window() {
        // No COUNT/UNTIL: still terminates and stays inside the window.
        let out = daily(
            "2026-01-01T09:00:00Z",
            "",
            "2026-01-01T00:00:00Z",
            "2026-01-02T00:00:00Z",
        );
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn all_day_expansion() {
        let dtstart = DateOrDateTime::AllDay(NaiveDate::from_ymd_opt(2026, 1, 1).unwrap());
        let out = expand_occurrences(
            dtstart,
            None,
            None,
            Some("FREQ=WEEKLY"),
            &[],
            &[],
            DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            DateTime::parse_from_rfc3339("2026-02-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        )
        .unwrap();
        assert_eq!(out.len(), 5);
    }

    #[test]
    fn rdate_and_exdate() {
        let start = DateTime::parse_from_rfc3339("2026-01-01T09:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        // distinct time so it adds a real occurrence rather than deduping
        let rdate = [DateOrDateTime::Timed(
            Utc.with_ymd_and_hms(2026, 1, 20, 10, 0, 0).unwrap(),
        )];
        let exdate = [DateOrDateTime::Timed(
            Utc.with_ymd_and_hms(2026, 1, 2, 9, 0, 0).unwrap(),
        )];
        let out = expand_occurrences(
            DateOrDateTime::Timed(start),
            None,
            None,
            Some("FREQ=DAILY"),
            &rdate,
            &exdate,
            Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            Utc.with_ymd_and_hms(2026, 1, 22, 0, 0, 0).unwrap(),
        )
        .unwrap();
        let days: Vec<u32> = out
            .iter()
            .map(|p| match p {
                DateOrDateTime::Timed(at) => at.day(),
                _ => 0,
            })
            .collect();
        // Jan 2 excluded by EXDATE; Jan 20 added by RDATE.
        assert!(!days.contains(&2), "{days:?}");
        assert_eq!(out.len(), 21);
    }

    #[test]
    fn unknown_tz_is_an_error_without_resolver_entry() {
        // No silent UTC fallback remains (ADR-012).
        assert!(matches!(
            resolve_tz(Some("Not/AZone"), None),
            Err(RecurrenceError::UnknownTimezone(_))
        ));
        assert!(matches!(
            resolve_tz(Some("Not/AZone"), Some(&TzResolver::default())),
            Err(RecurrenceError::UnknownTimezone(_))
        ));
        // Floating events (no tzid) are UTC by definition.
        assert!(matches!(resolve_tz(None, None), Ok(Zone::Tz(z)) if z == chrono_tz::UTC));
        // tzdb zones keep precedence over any resolver entry.
        assert!(matches!(
            resolve_tz(Some("America/Denver"), None),
            Ok(Zone::Tz(z)) if z == chrono_tz::America::Denver
        ));
        assert!(is_tzdb_tzid("America/Denver"));
        assert!(!is_tzdb_tzid("Custom/Test"));
    }

    /// Custom zone switching MST(-7) → MDT(-6) on the second Sunday of March
    /// (2026-03-08) and back on the first Sunday of November.
    fn custom_zone() -> TzResolver {
        let rules = vec![
            ZoneRule {
                dtstart: NaiveDateTime::parse_from_str("19700308T020000", "%Y%m%dT%H%M%S").unwrap(),
                offset_from_secs: -7 * 3600,
                offset_to_secs: -6 * 3600,
                rrule: Some("FREQ=YEARLY;BYMONTH=3;BYDAY=2SU".into()),
                rdates: vec![],
            },
            ZoneRule {
                dtstart: NaiveDateTime::parse_from_str("19701101T020000", "%Y%m%dT%H%M%S").unwrap(),
                offset_from_secs: -6 * 3600,
                offset_to_secs: -7 * 3600,
                rrule: Some("FREQ=YEARLY;BYMONTH=11;BYDAY=1SU".into()),
                rdates: vec![],
            },
        ];
        let from = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let to = Utc.with_ymd_and_hms(2028, 1, 1, 0, 0, 0).unwrap();
        let transitions = compile_zone(&rules, from, to).unwrap();
        let mut resolver = TzResolver::default();
        resolver.insert("Custom/Test", transitions);
        resolver
    }

    #[test]
    fn compile_zone_emits_dst_transitions() {
        let transitions = custom_zone().get("Custom/Test").unwrap().clone();
        // Mar 8 2026 02:00 local (offset_from -7) → 09:00Z switches to -6.
        assert!(
            transitions.contains(&(
                Utc.with_ymd_and_hms(2026, 3, 8, 9, 0, 0)
                    .unwrap()
                    .timestamp(),
                -6 * 3600
            ))
        );
        // Nov 1 2026 02:00 local (offset_from -6) → 08:00Z switches to -7.
        assert!(
            transitions.contains(&(
                Utc.with_ymd_and_hms(2026, 11, 1, 8, 0, 0)
                    .unwrap()
                    .timestamp(),
                -7 * 3600
            ))
        );
        // First entry is the base offset at the window start.
        assert_eq!(transitions[0].1, -7 * 3600);
    }

    #[test]
    fn custom_zone_expands_wall_clock_across_dst() {
        let resolver = custom_zone();
        // Daily 09:00 wall clock around the 2026 spring-forward (Mar 8).
        let dtstart = DateOrDateTime::Timed(
            DateTime::parse_from_rfc3339("2026-03-05T16:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        );
        let out = expand_occurrences(
            dtstart,
            Some("Custom/Test"),
            Some(&resolver),
            Some("FREQ=DAILY"),
            &[],
            &[],
            Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap(),
            Utc.with_ymd_and_hms(2026, 3, 15, 0, 0, 0).unwrap(),
        )
        .unwrap();
        let instants: Vec<DateTime<Utc>> = out
            .into_iter()
            .map(|p| match p {
                DateOrDateTime::Timed(at) => at,
                _ => panic!("timed expected"),
            })
            .collect();
        // UTC hour shifts with DST: 16:00Z before the transition (MST),
        // 15:00Z after (MDT), through Mar 8 itself.
        let utc: Vec<(u32, u32)> = instants
            .iter()
            .map(|d| (d.day(), Timelike::hour(d)))
            .collect();
        assert_eq!(
            utc,
            vec![
                (5, 16),
                (6, 16),
                (7, 16),
                (8, 15),
                (9, 15),
                (10, 15),
                (11, 15),
                (12, 15),
                (13, 15),
                (14, 15)
            ]
        );
        // Wall clock stays 09:00 on every occurrence.
        let zone = resolve_tz(Some("Custom/Test"), Some(&resolver)).unwrap();
        for at in &instants {
            assert_eq!(zone.to_local(*at).format("%H").to_string(), "09");
        }
    }

    #[test]
    fn unsupported_zone_rule_is_rejected() {
        let rules = vec![ZoneRule {
            dtstart: NaiveDateTime::parse_from_str("19700308T020000", "%Y%m%dT%H%M%S").unwrap(),
            offset_from_secs: 0,
            offset_to_secs: 3600,
            rrule: Some("FREQ=MINUTELY".into()),
            rdates: vec![],
        }];
        assert!(compile_zone(&rules, Utc::now() - chrono::Duration::days(1), Utc::now()).is_err());
    }

    #[test]
    fn yearly_byday_ordinal_month() {
        // FREQ=YEARLY;BYMONTH=3;BYDAY=2SU — the standard US DST-start rule.
        let out = expand(
            "FREQ=YEARLY;BYMONTH=3;BYDAY=2SU",
            "2026-03-08T12:00:00Z",
            None,
            "2026-01-01T00:00:00Z",
            "2028-01-01T00:00:00Z",
        );
        let dates: Vec<_> = out.iter().map(|d| (d.year(), d.month(), d.day())).collect();
        assert_eq!(dates, vec![(2026, 3, 8), (2027, 3, 14)]);
    }

    fn pts(times: &[&str]) -> Vec<DateOrDateTime> {
        times
            .iter()
            .map(|t| {
                DateOrDateTime::Timed(DateTime::parse_from_rfc3339(t).unwrap().with_timezone(&Utc))
            })
            .collect()
    }

    #[test]
    fn truncate_rrule_rewrites_until() {
        let before = pts(&["2026-01-01T09:00:00Z", "2026-01-02T09:00:00Z"]);
        assert_eq!(
            truncate_rrule("FREQ=DAILY", &before, false).as_deref(),
            Some("FREQ=DAILY;UNTIL=20260102T090000Z")
        );
        // INTERVAL and BYDAY survive; UNTIL is replaced in place.
        assert_eq!(
            truncate_rrule("FREQ=WEEKLY;INTERVAL=2;BYDAY=MO,WE", &before, false).as_deref(),
            Some("FREQ=WEEKLY;INTERVAL=2;BYDAY=MO,WE;UNTIL=20260102T090000Z")
        );
    }

    #[test]
    fn truncate_rrule_clamps_count() {
        let before = pts(&["2026-01-01T09:00:00Z", "2026-01-02T09:00:00Z"]);
        assert_eq!(
            truncate_rrule("FREQ=DAILY;COUNT=10", &before, false).as_deref(),
            Some("FREQ=DAILY;COUNT=2")
        );
    }

    #[test]
    fn truncate_rrule_all_day_uses_date_until() {
        let before = vec![DateOrDateTime::AllDay(
            chrono::NaiveDate::from_ymd_opt(2026, 3, 1).unwrap(),
        )];
        assert_eq!(
            truncate_rrule("FREQ=MONTHLY", &before, true).as_deref(),
            Some("FREQ=MONTHLY;UNTIL=20260301")
        );
    }

    #[test]
    fn truncate_rrule_nothing_before_stops_recurring() {
        assert_eq!(truncate_rrule("FREQ=DAILY", &[], false), None);
    }
}
