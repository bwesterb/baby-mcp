//! Time handling: everything is stored as Unix seconds (UTC) and shown in the
//! configured time zone (Europe/Amsterdam by default), DST-correct.

use jiff::civil::{Date, DateTime, Time};
use jiff::tz::TimeZone;
use jiff::{Timestamp, Zoned};

pub fn now() -> i64 {
    Timestamp::now().as_second()
}

#[derive(Clone)]
pub struct Clock {
    pub tz: TimeZone,
}

impl Clock {
    pub fn new(name: &str) -> Result<Clock, String> {
        TimeZone::get(name)
            .map(|tz| Clock { tz })
            .map_err(|e| format!("time zone {name:?}: {e}"))
    }

    pub fn zoned(&self, ts: i64) -> Zoned {
        Timestamp::from_second(ts)
            .unwrap_or(Timestamp::UNIX_EPOCH)
            .to_zoned(self.tz.clone())
    }

    /// "2026-11-05 03:10"
    pub fn fmt(&self, ts: i64) -> String {
        self.zoned(ts).strftime("%Y-%m-%d %H:%M").to_string()
    }

    /// "03:10"
    pub fn fmt_time(&self, ts: i64) -> String {
        self.zoned(ts).strftime("%H:%M").to_string()
    }

    /// "2026-11-05"
    pub fn day(&self, ts: i64) -> String {
        self.zoned(ts).date().to_string()
    }

    /// Start of the local day containing `ts`.
    pub fn day_start(&self, ts: i64) -> i64 {
        self.zoned(ts)
            .start_of_day()
            .map(|z| z.timestamp().as_second())
            .unwrap_or(ts)
    }

    /// Start of a local date given as "YYYY-MM-DD".
    pub fn date_start(&self, date: &str) -> Result<i64, String> {
        let d: Date = date
            .trim()
            .parse()
            .map_err(|_| format!("bad date {date:?}, want YYYY-MM-DD"))?;
        self.civil(d.to_datetime(Time::midnight()))
    }

    /// Start of the day after the local day containing `ts`.
    pub fn next_day_start(&self, ts: i64) -> i64 {
        let d = self.zoned(ts).date();
        d.tomorrow()
            .ok()
            .and_then(|t| self.civil(t.to_datetime(Time::midnight())).ok())
            .unwrap_or(ts + 86400)
    }

    fn civil(&self, dt: DateTime) -> Result<i64, String> {
        self.tz
            .to_zoned(dt)
            .map(|z| z.timestamp().as_second())
            .map_err(|e| format!("{dt}: {e}"))
    }

    /// Parses a user-given time. Bare clock times ("03:10") mean today, or
    /// yesterday if today's would be more than 15 minutes in the future.
    pub fn parse(&self, input: &str, now: i64) -> Result<i64, String> {
        self.parse_on(input, now, None)
    }

    /// Like `parse`, but a bare clock time falls on the local day of `on`
    /// when given (correcting an entry keeps it on its own day).
    pub fn parse_on(&self, input: &str, now: i64, on: Option<i64>) -> Result<i64, String> {
        let s = input.trim();
        if s.is_empty() || s.eq_ignore_ascii_case("now") {
            return Ok(now - now % 60);
        }
        if let Ok(ts) = s.parse::<Timestamp>() {
            return Ok(ts.as_second());
        }
        let norm = s.replacen(' ', "T", 1);
        if let Ok(dt) = norm.parse::<DateTime>() {
            return self.civil(dt);
        }
        let clock = s.replace('.', ":");
        if let Ok(t) = clock.parse::<Time>() {
            if let Some(on) = on {
                return self.civil(self.zoned(on).date().to_datetime(t));
            }
            let today = self.zoned(now).date();
            let mut ts = self.civil(today.to_datetime(t))?;
            if ts > now + 15 * 60 {
                let yesterday = today.yesterday().map_err(|e| e.to_string())?;
                ts = self.civil(yesterday.to_datetime(t))?;
            }
            return Ok(ts);
        }
        Err(format!(
            "cannot parse time {input:?}; use HH:MM, YYYY-MM-DD HH:MM or RFC 3339"
        ))
    }
}

/// "2h05m", "35m", "1d3h"
pub fn fmt_dur(secs: i64) -> String {
    let m = (secs.max(0) + 30) / 60;
    let (d, h, m) = (m / 1440, (m / 60) % 24, m % 60);
    if d > 0 {
        format!("{d}d{h}h")
    } else if h > 0 {
        format!("{h}h{m:02}m")
    } else {
        format!("{m}m")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clock() -> Clock {
        Clock::new("Europe/Amsterdam").unwrap()
    }

    #[test]
    fn parse_forms() {
        let c = clock();
        // 2026-11-05 10:00 Amsterdam (UTC+1) = 09:00 UTC
        let now = c.parse("2026-11-05 10:00", 0).unwrap();
        assert_eq!(c.fmt(now), "2026-11-05 10:00");
        assert_eq!(c.fmt(c.parse("03:10", now).unwrap()), "2026-11-05 03:10");
        // later today than now + 15 min: yesterday
        assert_eq!(c.fmt(c.parse("23:30", now).unwrap()), "2026-11-04 23:30");
        assert_eq!(c.fmt(c.parse("10:10", now).unwrap()), "2026-11-05 10:10");
        assert_eq!(c.fmt(c.parse("2026-11-05T08:00:00Z", now).unwrap()), "2026-11-05 09:00");
        assert!(c.parse("bogus", now).is_err());
        // On a given day: a bare time stays on that day.
        let earlier = c.parse("2026-11-02 18:00", now).unwrap();
        assert_eq!(c.fmt(c.parse_on("00:01", now, Some(earlier)).unwrap()), "2026-11-02 00:01");
        assert_eq!(c.fmt(c.parse_on("2026-11-03 07:00", now, Some(earlier)).unwrap()), "2026-11-03 07:00");
    }

    #[test]
    fn days_and_dst() {
        let c = clock();
        // 2026-10-25 is the DST change day: 25 hours long.
        let start = c.date_start("2026-10-25").unwrap();
        assert_eq!(c.next_day_start(start) - start, 25 * 3600);
        assert_eq!(c.day(start + 3600), "2026-10-25");
        assert_eq!(c.day_start(start + 5 * 3600), start);
    }

    #[test]
    fn durations() {
        assert_eq!(fmt_dur(35 * 60), "35m");
        assert_eq!(fmt_dur(2 * 3600 + 5 * 60), "2h05m");
        assert_eq!(fmt_dur(27 * 3600), "1d3h");
    }
}
