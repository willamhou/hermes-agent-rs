use std::str::FromStr;

use anyhow::bail;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronJob {
    pub id: String,
    pub name: String,
    pub prompt: String,
    pub schedule: JobSchedule,
    pub deliver: String,
    pub enabled: bool,
    pub created_at: String,
    pub next_run_at: Option<String>,
    pub last_run_at: Option<String>,
    pub last_status: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum JobSchedule {
    #[serde(rename = "once")]
    Once { run_at: String },
    #[serde(rename = "interval")]
    Interval { minutes: u64 },
    #[serde(rename = "cron")]
    Cron { expr: String },
}

/// Parse a human-friendly schedule string into a `JobSchedule`.
///
/// Supported formats:
/// - `<N>m` / `<N>min` → Interval { minutes: N }
/// - `<N>h`             → Interval { minutes: N * 60 }
/// - `<N>d`             → Interval { minutes: N * 1440 }
/// - 5-field cron expr  → Cron { expr }
/// - ISO 8601 timestamp → Once { run_at }
pub fn parse_schedule(input: &str) -> anyhow::Result<JobSchedule> {
    let s = input.trim();

    // --- interval shorthand ---
    if let Some(n) = strip_suffix_number(s, "min") {
        return Ok(JobSchedule::Interval { minutes: n });
    }
    if let Some(n) = strip_suffix_number(s, "m") {
        return Ok(JobSchedule::Interval { minutes: n });
    }
    if let Some(n) = strip_suffix_number(s, "h") {
        return Ok(JobSchedule::Interval { minutes: n * 60 });
    }
    if let Some(n) = strip_suffix_number(s, "d") {
        return Ok(JobSchedule::Interval { minutes: n * 1440 });
    }

    // --- cron expression (contains spaces → looks like multi-field) ---
    if s.contains(' ') {
        // Try to interpret as a 5-field standard cron.  The `cron` crate expects
        // 7 fields: sec min hour dom month dow year.
        // Convert 5-field ("min hour dom month dow") → 7-field by prepending "0 " and appending " *".
        let seven_field = format!("0 {} *", s);
        if cron::Schedule::from_str(&seven_field).is_ok() {
            return Ok(JobSchedule::Cron {
                expr: s.to_string(),
            });
        }
        // Maybe caller already supplied a 7-field expression.
        if cron::Schedule::from_str(s).is_ok() {
            return Ok(JobSchedule::Cron {
                expr: s.to_string(),
            });
        }
    }

    // --- ISO 8601 / RFC 3339 datetime ---
    if let Ok(dt) = s.parse::<DateTime<Utc>>() {
        return Ok(JobSchedule::Once {
            run_at: dt.to_rfc3339(),
        });
    }

    bail!("unrecognised schedule format: {:?}", input)
}

/// Returns the next fire time for a schedule after `after`.
pub fn compute_next_run(schedule: &JobSchedule, after: &DateTime<Utc>) -> Option<DateTime<Utc>> {
    match schedule {
        JobSchedule::Once { run_at } => {
            let dt = run_at.parse::<DateTime<Utc>>().ok()?;
            if dt > *after { Some(dt) } else { None }
        }
        JobSchedule::Interval { minutes } => Some(*after + Duration::minutes(*minutes as i64)),
        JobSchedule::Cron { expr } => {
            // Convert 5-field to 7-field for the `cron` crate.
            let seven_field = format!("0 {} *", expr);
            let schedule_expr = if let Ok(s) = cron::Schedule::from_str(&seven_field) {
                s
            } else {
                cron::Schedule::from_str(expr).ok()?
            };
            schedule_expr.after(after).next()
        }
    }
}

impl CronJob {
    pub fn new(name: String, prompt: String, schedule: JobSchedule, deliver: String) -> Self {
        let id = uuid::Uuid::new_v4().to_string()[..12].to_string();
        let now = Utc::now();
        let next = compute_next_run(&schedule, &now);
        Self {
            id,
            name,
            prompt,
            schedule,
            deliver,
            enabled: true,
            created_at: now.to_rfc3339(),
            next_run_at: next.map(|dt| dt.to_rfc3339()),
            last_run_at: None,
            last_status: None,
            last_error: None,
        }
    }
}

// ---- helpers ----------------------------------------------------------------

fn strip_suffix_number(s: &str, suffix: &str) -> Option<u64> {
    let lower = s.to_lowercase();
    let stripped = lower.strip_suffix(suffix)?;
    stripped.parse::<u64>().ok()
}

// ---- tests ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minutes() {
        let s = parse_schedule("30m").unwrap();
        assert!(matches!(s, JobSchedule::Interval { minutes: 30 }));
    }

    #[test]
    fn parse_hours() {
        let s = parse_schedule("2h").unwrap();
        assert!(matches!(s, JobSchedule::Interval { minutes: 120 }));
    }

    #[test]
    fn parse_days() {
        let s = parse_schedule("1d").unwrap();
        assert!(matches!(s, JobSchedule::Interval { minutes: 1440 }));
    }

    #[test]
    fn parse_cron_expr() {
        let s = parse_schedule("0 9 * * *").unwrap();
        assert!(matches!(s, JobSchedule::Cron { .. }));
    }

    #[test]
    fn parse_iso_timestamp() {
        let s = parse_schedule("2099-01-01T00:00:00Z").unwrap();
        assert!(matches!(s, JobSchedule::Once { .. }));
    }

    #[test]
    fn parse_invalid_returns_error() {
        assert!(parse_schedule("not-a-schedule-xyzzy").is_err());
    }

    #[test]
    fn compute_next_run_interval() {
        let after = Utc::now();
        let sched = JobSchedule::Interval { minutes: 10 };
        let next = compute_next_run(&sched, &after).unwrap();
        let diff = (next - after).num_minutes();
        assert_eq!(diff, 10);
    }

    #[test]
    fn compute_next_run_once_past_returns_none() {
        let past = "2000-01-01T00:00:00Z".to_string();
        let sched = JobSchedule::Once { run_at: past };
        let result = compute_next_run(&sched, &Utc::now());
        assert!(result.is_none());
    }
}
