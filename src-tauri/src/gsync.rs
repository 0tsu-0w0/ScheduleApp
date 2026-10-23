//! Two-way sync between Nora's own events and one Google calendar (issue #4).
//!
//! - The user picks one writable Google calendar. Every Nora event is mirrored there, and
//!   events of that calendar become ordinary (editable) Nora events.
//! - Changes are found with SQLite triggers: any edit to the synced fields stamps
//!   `updated_at`; an event is "dirty" while `updated_at > gsync_at`. Deleting a linked
//!   event leaves a tombstone so the deletion reaches Google.
//! - A sync first pulls Google's changes (incrementally, with a sync token), then pushes
//!   Nora's. When both sides changed the same event, the newer change wins.
//! - Repeating events travel as RRULE/EXDATE. Google rules Nora can't represent (e.g.
//!   "second Tuesday") are not pulled; they stay visible read-only through the import cache.
//! - Writes made by the sync itself bump `gsync_rev`, which the triggers ignore.
//! - Whatever a sync replaces or deletes is kept in `gsync_history` for 30 days and can be
//!   restored (issue #11). On a full pull, an unlinked Nora event identical to a Google one
//!   is linked to it instead of being copied twice.

use chrono::{DateTime, Duration, Local, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use rusqlite::params;
use serde_json::{Value, json};

use crate::db::{Db, DbResult, Event, Scope, dates_json, event_from_row, repeat_json};
use crate::recurrence::{Freq, Repeat};

type CmdResult<T> = Result<T, String>;

const DT_FMT: &str = "%Y-%m-%d %H:%M";
/// Events from Google that ended longer ago than this are not pulled into Nora.
const KEEP_PAST_DAYS: i64 = 365;
/// Color of events that arrive from Google (Nora's default lavender).
const PULLED_COLOR: u32 = 0x7c74ff;

/// How long replaced or deleted content stays restorable.
const HISTORY_DAYS: i64 = 30;

/// Milliseconds since the epoch, as SQLite computes it.
const NOW_MS: &str = "CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER)";

pub fn migrate(conn: &rusqlite::Connection) -> DbResult<()> {
    for (column, ty) in [
        ("google_id", "TEXT"),
        ("google_series", "TEXT"),
        ("google_orig", "TEXT"),
        ("updated_at", "INTEGER NOT NULL DEFAULT 0"),
        ("gsync_at", "INTEGER NOT NULL DEFAULT 0"),
        ("gsync_rev", "INTEGER NOT NULL DEFAULT 0"),
    ] {
        let exists = conn.prepare("SELECT 1 FROM pragma_table_info('events') WHERE name = ?1")?.exists([column])?;
        if !exists {
            conn.execute_batch(&format!("ALTER TABLE events ADD COLUMN {column} {ty}"))?;
        }
    }
    conn.execute_batch(&format!(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_events_google ON events(google_id) WHERE google_id IS NOT NULL;
         CREATE TABLE IF NOT EXISTS gsync_tombstones (google_id TEXT PRIMARY KEY, deleted_at INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS gsync_history (
             id       INTEGER PRIMARY KEY,
             event_id INTEGER,
             title    TEXT    NOT NULL,
             reason   TEXT    NOT NULL,
             snapshot TEXT    NOT NULL,
             at       INTEGER NOT NULL
         );
         CREATE TRIGGER IF NOT EXISTS events_gsync_insert AFTER INSERT ON events WHEN NEW.gsync_rev = 0
         BEGIN UPDATE events SET updated_at = {NOW_MS} WHERE id = NEW.id; END;
         CREATE TRIGGER IF NOT EXISTS events_gsync_update
         AFTER UPDATE OF title, start, end, all_day, location, memo, repeat, exdates ON events
         WHEN NEW.gsync_rev = OLD.gsync_rev
         BEGIN UPDATE events SET updated_at = MAX({NOW_MS}, OLD.updated_at + 1) WHERE id = NEW.id; END;
         CREATE TRIGGER IF NOT EXISTS events_gsync_delete AFTER DELETE ON events WHEN OLD.google_id IS NOT NULL
         BEGIN INSERT OR REPLACE INTO gsync_tombstones(google_id, deleted_at) VALUES (OLD.google_id, {NOW_MS}); END;"
    ))
}

// ---- recurrence <-> RRULE ---------------------------------------------------------------

const DAYS: [&str; 7] = ["SU", "MO", "TU", "WE", "TH", "FR", "SA"];

/// The system time zone as an IANA name (Google needs it for repeating events).
pub fn time_zone() -> String {
    iana_time_zone::get_timezone().unwrap_or_else(|_| "UTC".into())
}

fn utc_stamp(local: NaiveDateTime) -> String {
    let utc = Local.from_local_datetime(&local).earliest().map(|t| t.with_timezone(&Utc)).unwrap_or_else(|| Utc.from_utc_datetime(&local));
    utc.format("%Y%m%dT%H%M%SZ").to_string()
}

/// RRULE and EXDATE lines for a Nora series. `skip` are dates that must not become
/// EXDATEs because Google keeps a modified instance there.
pub fn build_recurrence(e: &Event, tz: &str, skip: &[NaiveDate]) -> Vec<String> {
    let Some(r) = &e.repeat else { return vec![] };
    let freq = match r.freq {
        Freq::Daily => "DAILY",
        Freq::Weekly => "WEEKLY",
        Freq::Monthly => "MONTHLY",
        Freq::Yearly => "YEARLY",
    };
    let mut rule = format!("RRULE:FREQ={freq}");
    if r.interval > 1 {
        rule += &format!(";INTERVAL={}", r.interval);
    }
    if r.freq == Freq::Weekly && !r.weekdays.is_empty() {
        let mut days = r.weekdays.clone();
        days.sort();
        days.dedup();
        let names: Vec<&str> = days.iter().filter_map(|d| DAYS.get(*d as usize).copied()).collect();
        rule += &format!(";BYDAY={}", names.join(","));
    }
    if let Some(n) = r.count {
        rule += &format!(";COUNT={n}");
    } else if let Some(until) = r.until {
        rule += &if e.all_day {
            format!(";UNTIL={}", until.format("%Y%m%d"))
        } else {
            format!(";UNTIL={}", utc_stamp(until.and_time(NaiveTime::from_hms_opt(23, 59, 59).unwrap())))
        };
    }
    let mut lines = vec![rule];
    let ex: Vec<NaiveDate> = e.exdates.iter().copied().filter(|d| !skip.contains(d)).collect();
    if !ex.is_empty() {
        lines.push(if e.all_day {
            format!("EXDATE;VALUE=DATE:{}", ex.iter().map(|d| d.format("%Y%m%d").to_string()).collect::<Vec<_>>().join(","))
        } else {
            let time = e.start.time();
            format!(
                "EXDATE;TZID={tz}:{}",
                ex.iter().map(|d| d.and_time(time).format("%Y%m%dT%H%M%S").to_string()).collect::<Vec<_>>().join(",")
            )
        });
    }
    lines
}

/// A date from an iCalendar value: `20261007`, `20261007T090000` or `20261007T000000Z`.
fn ical_date(v: &str) -> Option<NaiveDate> {
    let v = v.trim();
    if let Some(utc) = v.strip_suffix('Z') {
        let t = NaiveDateTime::parse_from_str(utc, "%Y%m%dT%H%M%S").ok()?;
        return Some(Utc.from_utc_datetime(&t).with_timezone(&Local).date_naive());
    }
    NaiveDate::parse_from_str(v.get(..8)?, "%Y%m%d").ok()
}

/// Reads Google's `recurrence` lines. `Err(())` when Nora can't represent the rule.
pub fn parse_recurrence(lines: &[String], start: NaiveDate) -> Result<(Option<Repeat>, Vec<NaiveDate>), ()> {
    let mut repeat: Option<Repeat> = None;
    let mut exdates = vec![];
    for line in lines {
        let (head, value) = line.split_once(':').ok_or(())?;
        let name = head.split(';').next().unwrap_or("").to_ascii_uppercase();
        match name.as_str() {
            "RRULE" => {
                if repeat.is_some() {
                    return Err(());
                }
                let mut r = Repeat { freq: Freq::Daily, interval: 1, weekdays: vec![], until: None, count: None };
                let mut freq = None;
                for part in value.split(';') {
                    let (k, v) = part.split_once('=').ok_or(())?;
                    match k.to_ascii_uppercase().as_str() {
                        "FREQ" => {
                            freq = Some(match v {
                                "DAILY" => Freq::Daily,
                                "WEEKLY" => Freq::Weekly,
                                "MONTHLY" => Freq::Monthly,
                                "YEARLY" => Freq::Yearly,
                                _ => return Err(()),
                            })
                        }
                        "INTERVAL" => r.interval = v.parse().map_err(|_| ())?,
                        "COUNT" => r.count = Some(v.parse().map_err(|_| ())?),
                        "UNTIL" => r.until = Some(ical_date(v).ok_or(())?),
                        "WKST" => {}
                        "BYDAY" => {
                            for day in v.split(',') {
                                r.weekdays.push(DAYS.iter().position(|d| *d == day).ok_or(())? as u8);
                            }
                        }
                        // Accepted only when they just restate the start date.
                        "BYMONTHDAY" if v.parse::<u32>().ok() == Some(chrono::Datelike::day(&start)) => {}
                        "BYMONTH" if v.parse::<u32>().ok() == Some(chrono::Datelike::month(&start)) => {}
                        _ => return Err(()),
                    }
                }
                r.freq = freq.ok_or(())?;
                if !r.weekdays.is_empty() && r.freq != Freq::Weekly {
                    return Err(());
                }
                r.weekdays.sort();
                repeat = Some(r);
            }
            "EXDATE" => {
                for v in value.split(',') {
                    exdates.push(ical_date(v).ok_or(())?);
                }
            }
            _ => return Err(()),
        }
    }
    exdates.sort();
    exdates.dedup();
    Ok((repeat, exdates))
}

// ---- events <-> Google JSON --------------------------------------------------------------

/// The request body for an event (insert or patch).
pub fn to_google(e: &Event, tz: &str, skip: &[NaiveDate], instance: bool) -> Value {
    let (start, end) = if e.all_day {
        (json!({ "date": e.start.date().to_string() }), json!({ "date": (e.end.date() + Duration::days(1)).to_string() }))
    } else {
        let at = |t: NaiveDateTime| {
            let local = Local.from_local_datetime(&t).earliest().map(|d| d.to_rfc3339()).unwrap_or_else(|| t.format("%Y-%m-%dT%H:%M:%S").to_string());
            json!({ "dateTime": local, "timeZone": tz })
        };
        (at(e.start), at(e.end.max(e.start)))
    };
    let mut body = json!({
        "summary": e.title,
        "description": e.memo,
        "location": e.location,
        "start": start,
        "end": end,
    });
    // A modified instance of a Google series can't carry its own rule.
    if !instance {
        body["recurrence"] = json!(build_recurrence(e, tz, skip));
    }
    body
}

/// One item from Google's events list (with `singleEvents=false`).
#[derive(Clone, Debug)]
pub struct Pulled {
    pub id: String,
    pub cancelled: bool,
    /// When Google last changed it, in ms.
    pub updated_ms: i64,
    /// Set for a modified or cancelled instance of a series.
    pub series: Option<String>,
    pub original_date: Option<NaiveDate>,
    pub title: String,
    pub memo: String,
    pub location: String,
    pub start: Option<NaiveDateTime>,
    pub end: Option<NaiveDateTime>,
    pub all_day: bool,
    pub recurrence: Vec<String>,
}

pub fn parse_pulled(item: &Value) -> Option<Pulled> {
    let when = |v: &Value| crate::gcal::parse_when(v);
    let start = when(&item["start"]);
    let end = when(&item["end"]);
    let all_day = start.is_some_and(|(_, a)| a);
    let end = end.map(|(t, _)| if all_day { t - Duration::days(1) } else { t });
    let original_date = when(&item["originalStartTime"]).map(|(t, _)| t.date());
    Some(Pulled {
        id: item["id"].as_str()?.to_owned(),
        cancelled: item["status"].as_str() == Some("cancelled"),
        updated_ms: item["updated"].as_str().and_then(|s| DateTime::parse_from_rfc3339(s).ok()).map_or(0, |t| t.timestamp_millis()),
        series: item["recurringEventId"].as_str().map(str::to_owned),
        original_date,
        title: item["summary"].as_str().unwrap_or("(No title)").to_owned(),
        memo: item["description"].as_str().unwrap_or_default().to_owned(),
        location: item["location"].as_str().unwrap_or_default().to_owned(),
        start: start.map(|(t, _)| t),
        end: end.map(|e| e.max(start.map_or(e, |(s, _)| s))),
        all_day,
        recurrence: item["recurrence"].as_array().into_iter().flatten().filter_map(|v| v.as_str().map(str::to_owned)).collect(),
    })
}

// ---- local state ---------------------------------------------------------------------

/// A Nora event plus its sync bookkeeping.
#[derive(Clone, Debug)]
pub struct Linked {
    pub event: Event,
    pub google_id: Option<String>,
    pub google_series: Option<String>,
    pub google_orig: Option<NaiveDate>,
    pub updated_at: i64,
    pub gsync_at: i64,
}

impl Linked {
    pub fn dirty(&self) -> bool {
        self.google_id.is_none() || self.updated_at > self.gsync_at
    }
}

const EXTRA: &str = "google_id, google_series, google_orig, updated_at, gsync_at";

fn linked_from_row(r: &rusqlite::Row) -> DbResult<Linked> {
    Ok(Linked {
        event: event_from_row(r)?,
        google_id: r.get(14)?,
        google_series: r.get(15)?,
        google_orig: r.get::<_, Option<String>>(16)?.and_then(|s| NaiveDate::parse_from_str(&s, "%Y-%m-%d").ok()),
        updated_at: r.get(17)?,
        gsync_at: r.get(18)?,
    })
}

/// What a pull found that needs remembering.
#[derive(Debug, Default, PartialEq)]
pub struct PullOutcome {
    /// Series Nora can't represent (shown read-only instead).
    pub unsupported: Vec<String>,
    /// Ids that are now fine again (removed from the unsupported list).
    pub supported: Vec<String>,
}

/// Why a version was kept in the history.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// Nora's version was replaced by a newer change from Google.
    GoogleChanged,
    /// Nora's event was removed because Google deleted it (or Nora can't edit its rule).
    GoogleDeleted,
    /// Google's change was dropped because Nora's change was newer.
    NoraChanged,
}

impl Reason {
    fn key(self) -> &'static str {
        match self {
            Reason::GoogleChanged => "google_changed",
            Reason::GoogleDeleted => "google_deleted",
            Reason::NoraChanged => "nora_changed",
        }
    }
}

/// A restorable version, as listed in Settings.
#[derive(Clone, Debug, serde::Serialize)]
pub struct HistoryEntry {
    pub id: i64,
    pub title: String,
    pub reason: String,
    /// When it was replaced, in ms.
    pub at: i64,
    pub start: chrono::NaiveDateTime,
    pub all_day: bool,
}

/// What turning sync on would do (shown before it starts).
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct Preview {
    /// Nora events that would be sent to Google.
    pub send: usize,
    /// Google events that would become Nora events.
    pub receive: usize,
    /// Events found on both sides, linked instead of copied.
    pub linked: usize,
}

/// The synced fields of two versions are the same.
fn same_content(a: &Event, b: &Event) -> bool {
    a.title == b.title
        && a.start == b.start
        && a.end == b.end
        && a.all_day == b.all_day
        && a.location == b.location
        && a.memo == b.memo
        && a.repeat == b.repeat
        && a.exdates == b.exdates
}

/// Google's version of an item as a Nora event (for the history).
fn pulled_event(p: &Pulled, repeat: &Option<Repeat>, exdates: &[NaiveDate], base: &Event) -> Option<Event> {
    Some(Event {
        title: p.title.clone(),
        start: p.start?,
        end: p.end?,
        all_day: p.all_day,
        location: p.location.clone(),
        memo: p.memo.clone(),
        repeat: repeat.clone(),
        exdates: exdates.to_vec(),
        ..base.clone()
    })
}

impl Db {
    fn linked_where(&self, condition: &str, args: impl rusqlite::Params) -> DbResult<Vec<Linked>> {
        let sql = format!("SELECT {}, {EXTRA} FROM events WHERE {condition}", Self::EVENT_COLUMNS);
        self.conn.prepare(&sql)?.query_map(args, linked_from_row)?.collect()
    }

    fn linked_by_google(&self, google_id: &str) -> DbResult<Option<Linked>> {
        Ok(self.linked_where("google_id = ?1", [google_id])?.into_iter().next())
    }

    /// Events that must be sent to Google, oldest change first.
    pub fn gsync_pending(&self) -> DbResult<Vec<Linked>> {
        let mut rows = self.linked_where("google_id IS NULL OR updated_at > gsync_at", [])?;
        rows.sort_by_key(|l| l.updated_at);
        Ok(rows)
    }

    pub fn gsync_tombstones(&self) -> DbResult<Vec<String>> {
        self.conn.prepare("SELECT google_id FROM gsync_tombstones ORDER BY deleted_at")?.query_map([], |r| r.get(0))?.collect()
    }

    pub fn gsync_forget_tombstone(&self, google_id: &str) -> DbResult<()> {
        self.conn.execute("DELETE FROM gsync_tombstones WHERE google_id = ?1", [google_id])?;
        Ok(())
    }

    /// Dates of a series where Google keeps a modified instance (never sent as EXDATE).
    pub fn gsync_exception_dates(&self, series: &str) -> DbResult<Vec<NaiveDate>> {
        Ok(self.linked_where("google_series = ?1", [series])?.into_iter().filter_map(|l| l.google_orig).collect())
    }

    /// Records a successful push of `row` (as it was when read) under `google_id`.
    pub fn gsync_mark_pushed(&self, row: &Linked, google_id: &str) -> DbResult<()> {
        // Only bookkeeping columns change, so the triggers stay quiet.
        self.conn.execute(
            "UPDATE events SET google_id = ?1, gsync_at = ?2 WHERE id = ?3",
            params![google_id, row.updated_at, row.event.id],
        )?;
        Ok(())
    }

    /// Turns a modified Google instance into a plain event (its series is gone in Google).
    pub fn gsync_detach(&self, id: i64) -> DbResult<()> {
        self.conn.execute("UPDATE events SET google_id = NULL, google_series = NULL, google_orig = NULL WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Drops every link to Google (sync turned off or moved to another calendar).
    pub fn gsync_unlink_all(&self) -> DbResult<()> {
        self.conn.execute_batch(
            "UPDATE events SET google_id = NULL, google_series = NULL, google_orig = NULL, gsync_at = 0;
             DELETE FROM gsync_tombstones;",
        )
    }

    /// Keeps `event` in the history and drops entries older than `HISTORY_DAYS`.
    fn remember(&self, reason: Reason, event_id: Option<i64>, event: &Event) -> DbResult<()> {
        let snapshot = serde_json::to_string(event).unwrap_or_default();
        self.conn.execute(
            &format!("INSERT INTO gsync_history(event_id, title, reason, snapshot, at) VALUES(?1, ?2, ?3, ?4, {NOW_MS})"),
            params![event_id, event.title, reason.key(), snapshot],
        )?;
        self.conn.execute(
            &format!("DELETE FROM gsync_history WHERE at < {NOW_MS} - ?1"),
            [HISTORY_DAYS * 86_400_000],
        )?;
        Ok(())
    }

    /// Restorable versions, newest first.
    pub fn gsync_history(&self) -> DbResult<Vec<HistoryEntry>> {
        let mut st = self.conn.prepare("SELECT id, title, reason, at, snapshot FROM gsync_history ORDER BY at DESC, id DESC")?;
        let rows = st.query_map([], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?, r.get::<_, String>(4)?))
        })?;
        let mut out = vec![];
        for row in rows {
            let (id, title, reason, at, snapshot) = row?;
            if let Ok(e) = serde_json::from_str::<Event>(&snapshot) {
                out.push(HistoryEntry { id, title, reason, at, start: e.start, all_day: e.all_day });
            }
        }
        Ok(out)
    }

    /// Puts a kept version back. It counts as an edit in Nora, so the next sync sends it
    /// to Google. Returns the id of the restored event.
    pub fn gsync_restore(&self, history_id: i64) -> DbResult<Option<i64>> {
        let found: Option<(Option<i64>, String)> = self
            .conn
            .query_row("SELECT event_id, snapshot FROM gsync_history WHERE id = ?1", [history_id], |r| Ok((r.get(0)?, r.get(1)?)))
            .ok();
        let Some((event_id, snapshot)) = found else { return Ok(None) };
        let Ok(old) = serde_json::from_str::<Event>(&snapshot) else { return Ok(None) };
        let current = match event_id {
            Some(id) => self.linked_where("id = ?1", [id])?.into_iter().next(),
            None => None,
        };
        let id = match current {
            // Still there: bring back the synced fields, keep Nora-only ones (color, tags…).
            Some(c) => {
                let restored = Event {
                    title: old.title.clone(),
                    start: old.start,
                    end: old.end,
                    all_day: old.all_day,
                    location: old.location.clone(),
                    memo: old.memo.clone(),
                    repeat: old.repeat.clone(),
                    occurrence: None,
                    ..c.event.clone()
                };
                self.save_event(&restored, Scope::All)?
            }
            None => self.save_event(&Event { id: 0, occurrence: None, ..old.clone() }, Scope::All)?,
        };
        // save_event keeps (or clears) skipped dates; set the kept ones explicitly.
        self.conn.execute("UPDATE events SET exdates = ?1 WHERE id = ?2", params![dates_json(&old.exdates), id])?;
        self.conn.execute("DELETE FROM gsync_history WHERE id = ?1", [history_id])?;
        Ok(Some(id))
    }

    /// An unlinked Nora event with exactly this content, if any.
    fn unlinked_twin(&self, p: &Pulled, repeat: &Option<Repeat>) -> DbResult<Option<Linked>> {
        let (Some(start), Some(end)) = (p.start, p.end) else { return Ok(None) };
        Ok(self
            .linked_where(
                "google_id IS NULL AND title = ?1 AND start = ?2 AND end = ?3 AND all_day = ?4 AND repeat IS ?5",
                params![p.title, start.format(DT_FMT).to_string(), end.format(DT_FMT).to_string(), p.all_day, repeat_json(repeat)],
            )?
            .into_iter()
            .next())
    }

    /// Counts what turning sync on with these Google items would do.
    pub fn gsync_preview(&self, items: &[Pulled], today: NaiveDate) -> DbResult<Preview> {
        let oldest = today - Duration::days(KEEP_PAST_DAYS);
        let mut linked = std::collections::HashSet::new();
        let mut receive = 0;
        for p in items.iter().filter(|p| !p.cancelled && p.series.is_none()) {
            let Ok((repeat, _)) = parse_recurrence(&p.recurrence, p.start.map_or(today, |s| s.date())) else { continue };
            if repeat.is_none() && p.end.is_none_or(|e| e.date() < oldest) {
                continue;
            }
            match self.unlinked_twin(p, &repeat)? {
                Some(twin) if linked.insert(twin.event.id) => {}
                _ => receive += 1,
            }
        }
        let total: i64 = self.conn.query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))?;
        Ok(Preview { send: total as usize - linked.len(), receive, linked: linked.len() })
    }

    /// Deletes a row because Google deleted it (no tombstone, so nothing is sent back).
    fn sync_delete(&self, id: i64, google_id: Option<&str>) -> DbResult<()> {
        if let Some(l) = self.linked_where("id = ?1", [id])?.into_iter().next() {
            self.remember(Reason::GoogleDeleted, None, &l.event)?;
        }
        self.conn.execute("DELETE FROM events WHERE id = ?1", [id])?;
        if let Some(g) = google_id {
            self.gsync_forget_tombstone(g)?;
        }
        Ok(())
    }

    /// Writes content that came from Google onto an existing row, leaving it clean.
    /// The replaced version is kept in the history when it differed.
    fn sync_overwrite(&self, local: &Linked, p: &Pulled, repeat: &Option<Repeat>, exdates: &[NaiveDate]) -> DbResult<()> {
        let (Some(start), Some(end)) = (p.start, p.end) else { return Ok(()) };
        let id = local.event.id;
        if let Some(google) = pulled_event(p, repeat, exdates, &local.event)
            && !same_content(&google, &local.event)
            && local.google_id.is_some()
        {
            self.remember(Reason::GoogleChanged, Some(id), &local.event)?;
        }
        self.conn.execute(
            &format!(
                "UPDATE events SET title = ?1, start = ?2, end = ?3, all_day = ?4, location = ?5, memo = ?6, repeat = ?7,
                 exdates = ?8, gsync_rev = gsync_rev + 1, updated_at = {NOW_MS}, gsync_at = {NOW_MS} WHERE id = ?9"
            ),
            params![
                p.title, start.format(DT_FMT).to_string(), end.format(DT_FMT).to_string(), p.all_day, p.location, p.memo,
                repeat_json(repeat), dates_json(exdates), id
            ],
        )?;
        Ok(())
    }

    fn sync_insert(&self, p: &Pulled, repeat: &Option<Repeat>, exdates: &[NaiveDate]) -> DbResult<()> {
        let (Some(start), Some(end)) = (p.start, p.end) else { return Ok(()) };
        self.conn.execute(
            &format!(
                "INSERT INTO events(title, start, end, all_day, color, location, links, memo, repeat, exdates, cancelled, tags,
                 reminder, google_id, google_series, google_orig, gsync_rev, updated_at, gsync_at)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, '', ?7, ?8, ?9, 0, '[]', NULL, ?10, ?11, ?12, 1, {NOW_MS}, {NOW_MS})"
            ),
            params![
                p.title, start.format(DT_FMT).to_string(), end.format(DT_FMT).to_string(), p.all_day, PULLED_COLOR, p.location,
                p.memo, repeat_json(repeat), dates_json(exdates), p.id, p.series, p.original_date.map(|d| d.to_string())
            ],
        )?;
        Ok(())
    }

    /// Adds `day` to a series' skipped dates without marking the series as changed.
    fn sync_add_exdate(&self, series: &Linked, day: NaiveDate) -> DbResult<()> {
        let mut exdates = series.event.exdates.clone();
        if exdates.contains(&day) {
            return Ok(());
        }
        exdates.push(day);
        exdates.sort();
        self.conn.execute(
            "UPDATE events SET exdates = ?1, gsync_rev = gsync_rev + 1 WHERE id = ?2",
            params![dates_json(&exdates), series.event.id],
        )?;
        Ok(())
    }

    /// Applies items pulled from Google. With `full`, rows linked to Google events that
    /// are no longer there are deleted too (unless they changed in Nora since).
    pub fn gsync_apply(&self, items: &[Pulled], full: bool, today: NaiveDate) -> DbResult<PullOutcome> {
        let tx = self.conn.unchecked_transaction()?;
        let mut out = PullOutcome::default();
        let oldest = today - Duration::days(KEEP_PAST_DAYS);

        // Series first, so modified instances find them.
        let mut ordered: Vec<&Pulled> = items.iter().collect();
        ordered.sort_by_key(|p| p.series.is_some());

        for p in ordered {
            let local = self.linked_by_google(&p.id)?;
            if p.cancelled {
                match (&p.series, p.original_date) {
                    // One occurrence of a series was deleted in Google.
                    (Some(series), Some(day)) => {
                        if let Some(s) = self.linked_by_google(series)? {
                            self.sync_add_exdate(&s, day)?;
                        }
                        if let Some(l) = local {
                            self.sync_delete(l.event.id, l.google_id.as_deref())?;
                        }
                    }
                    _ => {
                        if let Some(l) = local {
                            if l.dirty() && l.updated_at > p.updated_ms {
                                // Changed in Nora after Google deleted it: keep it and send it again.
                                self.conn.execute("UPDATE events SET google_id = NULL WHERE id = ?1", [l.event.id])?;
                            } else {
                                self.sync_delete(l.event.id, l.google_id.as_deref())?;
                            }
                        }
                        for inst in self.linked_where("google_series = ?1", [&p.id])? {
                            self.sync_delete(inst.event.id, inst.google_id.as_deref())?;
                        }
                        out.supported.push(p.id.clone());
                    }
                }
                continue;
            }

            let (repeat, mut exdates) = if p.series.is_some() {
                (None, vec![])
            } else {
                match parse_recurrence(&p.recurrence, p.start.map_or(today, |s| s.date())) {
                    Ok(r) => r,
                    Err(()) => {
                        // Nora can't edit this rule: drop any local copy and show it read-only.
                        if let Some(l) = local {
                            self.sync_delete(l.event.id, l.google_id.as_deref())?;
                        }
                        out.unsupported.push(p.id.clone());
                        continue;
                    }
                }
            };
            if p.series.is_none() {
                out.supported.push(p.id.clone());
                // Dates where Google keeps a modified instance stay skipped in the Nora series.
                exdates.extend(self.gsync_exception_dates(&p.id)?);
                exdates.sort();
                exdates.dedup();
            }
            if let (Some(series), Some(day)) = (&p.series, p.original_date)
                && let Some(s) = self.linked_by_google(series)?
            {
                self.sync_add_exdate(&s, day)?;
            }

            match local {
                Some(l) if l.dirty() && l.updated_at > p.updated_ms => {
                    // Nora's change is newer and is pushed next; keep Google's version restorable.
                    if let Some(google) = pulled_event(p, &repeat, &exdates, &l.event)
                        && !same_content(&google, &l.event)
                    {
                        self.remember(Reason::NoraChanged, Some(l.event.id), &google)?;
                    }
                }
                Some(l) => self.sync_overwrite(&l, p, &repeat, &exdates)?,
                None => {
                    let ended = p.end.is_none_or(|e| e.date() < oldest);
                    if repeat.is_none() && ended {
                        continue;
                    }
                    // First sync: the same event already in Nora is linked, not copied.
                    if full
                        && p.series.is_none()
                        && let Some(twin) = self.unlinked_twin(p, &repeat)?
                    {
                        self.conn.execute(
                            "UPDATE events SET google_id = ?1 WHERE id = ?2",
                            params![p.id, twin.event.id],
                        )?;
                        let linked = Linked { google_id: Some(p.id.clone()), ..twin };
                        self.sync_overwrite(&linked, p, &repeat, &exdates)?;
                        continue;
                    }
                    self.sync_insert(p, &repeat, &exdates)?;
                }
            }
        }

        if full {
            let seen: std::collections::HashSet<&str> = items.iter().filter(|p| !p.cancelled).map(|p| p.id.as_str()).collect();
            for l in self.linked_where("google_id IS NOT NULL", [])? {
                let g = l.google_id.as_deref().unwrap_or_default();
                if !seen.contains(g) && !l.dirty() {
                    self.sync_delete(l.event.id, Some(g))?;
                }
            }
        }
        tx.commit()?;
        Ok(out)
    }
}

// ---- network ------------------------------------------------------------------------

/// Sends a request, retrying rate limits and server errors; returns the status and body.
fn send(mut call: impl FnMut() -> Result<ureq::http::Response<ureq::Body>, ureq::Error>) -> CmdResult<(u16, String)> {
    let mut wait = std::time::Duration::from_secs(1);
    for attempt in 0..4 {
        let mut resp = call().map_err(|e| format!("Could not reach Google: {e}"))?;
        let status = resp.status().as_u16();
        let body = resp.body_mut().read_to_string().unwrap_or_default();
        let retryable = status == 429 || status >= 500 || (status == 403 && body.contains("rateLimit"));
        if !retryable || attempt == 3 {
            return Ok((status, body));
        }
        std::thread::sleep(wait);
        wait *= 2;
    }
    unreachable!()
}

fn ok(status: u16) -> bool {
    (200..300).contains(&status)
}

fn events_url(calendar: &str) -> String {
    format!("{}/calendars/{}/events", crate::gcal::API, urlencoding::encode(calendar))
}

/// Google's changes since `sync_token`, or everything when it is `None` (or expired).
/// Returns the items, the next sync token and whether this was a full listing.
pub fn pull(token: &str, calendar: &str, sync_token: Option<&str>) -> CmdResult<(Vec<Pulled>, String, bool)> {
    let auth = format!("Bearer {token}");
    let url = events_url(calendar);
    let mut items = vec![];
    let mut page: Option<String> = None;
    loop {
        let (status, body) = send(|| {
            let mut req = crate::gcal::agent()
                .get(&url)
                .header("Authorization", &auth)
                .query("singleEvents", "false")
                .query("showDeleted", "true")
                .query("maxResults", "2500");
            if let Some(t) = sync_token {
                req = req.query("syncToken", t);
            }
            if let Some(p) = &page {
                req = req.query("pageToken", p);
            }
            req.call()
        })?;
        // The sync token expired: start over with a full listing.
        if status == 410 && sync_token.is_some() {
            return pull(token, calendar, None);
        }
        if !ok(status) {
            return Err(crate::gcal::http_error(status, &body));
        }
        let json: Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
        items.extend(json["items"].as_array().into_iter().flatten().filter_map(parse_pulled));
        if let Some(p) = json["nextPageToken"].as_str() {
            page = Some(p.to_owned());
            continue;
        }
        let next = json["nextSyncToken"].as_str().ok_or("Google sent no sync token")?.to_owned();
        return Ok((items, next, sync_token.is_none()));
    }
}

/// Sends Nora's deletions and changes to Google. `db` runs a closure under the database lock.
pub fn push<D>(token: &str, calendar: &str, tz: &str, device: &str, db: D) -> CmdResult<usize>
where
    D: Fn(&mut dyn FnMut(&Db) -> DbResult<()>) -> CmdResult<()>,
{
    let auth = format!("Bearer {token}");
    let url = events_url(calendar);
    let item_url = |id: &str| format!("{url}/{}", urlencoding::encode(id));
    let mut sent = 0;

    let mut tombstones = vec![];
    db(&mut |d| {
        tombstones = d.gsync_tombstones()?;
        Ok(())
    })?;
    for g in tombstones {
        let (status, body) = send(|| crate::gcal::agent().delete(&item_url(&g)).header("Authorization", &auth).call())?;
        // Already gone in Google is fine too.
        if !(ok(status) || status == 404 || status == 410) {
            return Err(crate::gcal::http_error(status, &body));
        }
        db(&mut |d| d.gsync_forget_tombstone(&g))?;
        sent += 1;
    }

    let mut pending = vec![];
    db(&mut |d| {
        pending = d.gsync_pending()?;
        Ok(())
    })?;
    for mut row in pending {
        let patch = |id: &str, body: &Value| {
            send(|| {
                crate::gcal::agent()
                    .patch(&item_url(id))
                    .header("Authorization", &auth)
                    .header("Content-Type", "application/json")
                    .send(body.to_string())
            })
        };
        let body_for = |row: &Linked| -> CmdResult<Value> {
            let mut skip = vec![];
            if row.event.repeat.is_some()
                && let Some(g) = &row.google_id
            {
                db(&mut |d| {
                    skip = d.gsync_exception_dates(g)?;
                    Ok(())
                })?;
            }
            Ok(to_google(&row.event, tz, &skip, row.google_series.is_some()))
        };

        if let Some(g) = row.google_id.clone() {
            let (status, body) = patch(&g, &body_for(&row)?)?;
            if ok(status) {
                db(&mut |d| d.gsync_mark_pushed(&row, &g))?;
                sent += 1;
                continue;
            }
            if !(status == 404 || status == 410) {
                return Err(crate::gcal::http_error(status, &body));
            }
            // Deleted in Google meanwhile but changed in Nora since: send it as a new event.
            if row.google_series.is_some() {
                db(&mut |d| d.gsync_detach(row.event.id))?;
                row.google_series = None;
                row.google_orig = None;
            }
            row.google_id = None;
        }

        let id = google_id_for(device, &row);
        let mut body = body_for(&row)?;
        body["id"] = json!(id);
        let (status, text) = send(|| {
            crate::gcal::agent()
                .post(&url)
                .header("Authorization", &auth)
                .header("Content-Type", "application/json")
                .send(body.to_string())
        })?;
        let status = if status == 409 { patch(&id, &body)?.0 } else { status };
        if !ok(status) {
            return Err(crate::gcal::http_error(status, &text));
        }
        db(&mut |d| d.gsync_mark_pushed(&row, &id))?;
        sent += 1;
    }
    Ok(sent)
}

/// A Google event id for a Nora row: only `0-9a-v`, unique per device and edit.
pub fn google_id_for(device: &str, row: &Linked) -> String {
    fn b32(mut n: u64) -> String {
        const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuv";
        let mut s = vec![];
        loop {
            s.push(DIGITS[(n % 32) as usize]);
            n /= 32;
            if n == 0 {
                break;
            }
        }
        s.reverse();
        String::from_utf8(s).unwrap_or_default()
    }
    format!("nora{device}{}0{}", b32(row.event.id as u64), b32(row.updated_at as u64))
}

/// Random 8-character tag in Google's id alphabet, made once per installation.
pub fn new_device_tag() -> String {
    let mut buf = [0u8; 8];
    let _ = getrandom::fill(&mut buf);
    buf.iter().map(|b| b"0123456789abcdefghijklmnopqrstuv"[(*b % 32) as usize] as char).collect()
}

/// Looks up the row a pulled item belongs to.
#[cfg(test)]
fn find_linked(db: &Db, google_id: &str) -> DbResult<Option<Linked>> {
    db.linked_by_google(google_id)
}

/// Reads a single row with its bookkeeping (tests).
#[cfg(test)]
fn linked(db: &Db, id: i64) -> Linked {
    db.linked_where("id = ?1", [id]).unwrap().into_iter().next().unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Scope;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }
    fn dt(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, DT_FMT).unwrap()
    }
    fn open(name: &str) -> (Db, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!("nora-gsync-{name}-{}.sqlite", std::process::id()));
        let _ = std::fs::remove_file(&path);
        (Db::open(&path).unwrap(), path)
    }
    fn event(title: &str, start: &str, end: &str) -> Event {
        Event {
            id: 0, title: title.into(), start: dt(start), end: dt(end), all_day: false, color: 1, location: String::new(),
            links: vec![], memo: String::new(), repeat: None, exdates: vec![], occurrence: None, cancelled: false, tags: vec![],
            reminder: None,
        }
    }
    fn pulled(id: &str, title: &str, start: &str, end: &str, updated_ms: i64) -> Pulled {
        Pulled {
            id: id.into(), cancelled: false, updated_ms, series: None, original_date: None, title: title.into(),
            memo: String::new(), location: String::new(), start: Some(dt(start)), end: Some(dt(end)), all_day: false,
            recurrence: vec![],
        }
    }

    #[test]
    fn rules_round_trip_through_rrule() {
        let mut e = event("Gym", "2026-10-05 07:00", "2026-10-05 08:00");
        e.repeat = Some(Repeat { freq: Freq::Weekly, interval: 2, weekdays: vec![3, 1], until: None, count: Some(10) });
        e.exdates = vec![d("2026-10-19")];
        let lines = build_recurrence(&e, "Asia/Tokyo", &[]);
        assert_eq!(lines[0], "RRULE:FREQ=WEEKLY;INTERVAL=2;BYDAY=MO,WE;COUNT=10");
        assert_eq!(lines[1], "EXDATE;TZID=Asia/Tokyo:20261019T070000");
        let (repeat, exdates) = parse_recurrence(&lines, e.start.date()).unwrap();
        assert_eq!(repeat, Some(Repeat { weekdays: vec![1, 3], ..e.repeat.clone().unwrap() }));
        assert_eq!(exdates, vec![d("2026-10-19")]);

        let mut all_day = event("Trip", "2026-10-05 00:00", "2026-10-06 00:00");
        all_day.all_day = true;
        all_day.repeat = Some(Repeat { freq: Freq::Yearly, interval: 1, weekdays: vec![], until: Some(d("2030-10-05")), count: None });
        all_day.exdates = vec![d("2027-10-05"), d("2028-10-05")];
        let lines = build_recurrence(&all_day, "Asia/Tokyo", &[d("2028-10-05")]);
        assert_eq!(lines, ["RRULE:FREQ=YEARLY;UNTIL=20301005", "EXDATE;VALUE=DATE:20271005"]);
        let (repeat, _) = parse_recurrence(&lines, all_day.start.date()).unwrap();
        assert_eq!(repeat, all_day.repeat);
    }

    #[test]
    fn rules_nora_cannot_show_are_refused() {
        let start = d("2026-10-13");
        assert!(parse_recurrence(&["RRULE:FREQ=MONTHLY;BYDAY=2TU".into()], start).is_err());
        assert!(parse_recurrence(&["RRULE:FREQ=YEARLY;BYMONTH=3".into()], start).is_err());
        assert!(parse_recurrence(&["RRULE:FREQ=DAILY".into(), "RRULE:FREQ=WEEKLY".into()], start).is_err());
        assert!(parse_recurrence(&["RDATE:20261020".into()], start).is_err());
        // Restating the start date is fine.
        assert!(parse_recurrence(&["RRULE:FREQ=MONTHLY;BYMONTHDAY=13".into()], start).is_ok());
    }

    #[test]
    fn edits_mark_rows_dirty_and_deletes_leave_tombstones() {
        let (db, path) = open("dirty");
        let id = db.save_event(&event("Lunch", "2026-10-07 12:00", "2026-10-07 13:00"), Scope::All).unwrap();
        let row = linked(&db, id);
        assert!(row.dirty() && row.updated_at > 0);

        db.gsync_mark_pushed(&row, "g1").unwrap();
        assert!(!linked(&db, id).dirty());

        // Local-only fields (color, tags) don't need a push; synced fields do.
        db.conn.execute("UPDATE events SET color = 5 WHERE id = ?1", [id]).unwrap();
        assert!(!linked(&db, id).dirty());
        db.save_event(&Event { id, title: "Lunch with A".into(), ..event("", "2026-10-07 12:00", "2026-10-07 13:00") }, Scope::All).unwrap();
        assert!(linked(&db, id).dirty());

        db.delete_event(id, None, Scope::All).unwrap();
        assert_eq!(db.gsync_tombstones().unwrap(), ["g1"]);
        drop(db);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn pulled_changes_apply_unless_nora_changed_later() {
        let (db, path) = open("pull");
        let today = d("2026-10-07");
        // New from Google.
        db.gsync_apply(&[pulled("a", "Dentist", "2026-10-08 10:00", "2026-10-08 11:00", 1_000)], false, today).unwrap();
        let a = find_linked(&db, "a").unwrap().unwrap();
        assert_eq!(a.event.title, "Dentist");
        assert!(!a.dirty(), "pulled rows are clean");

        // Google changed it: applied.
        db.gsync_apply(&[pulled("a", "Dentist (moved)", "2026-10-09 10:00", "2026-10-09 11:00", 2_000)], false, today).unwrap();
        assert_eq!(find_linked(&db, "a").unwrap().unwrap().event.title, "Dentist (moved)");

        // Nora changed it after Google's change: Nora wins.
        let id = a.event.id;
        db.save_event(&Event { id, ..event("Dentist (Nora)", "2026-10-09 10:00", "2026-10-09 11:00") }, Scope::All).unwrap();
        db.gsync_apply(&[pulled("a", "Dentist (old Google edit)", "2026-10-09 10:00", "2026-10-09 11:00", 3_000)], false, today).unwrap();
        let row = linked(&db, id);
        assert_eq!(row.event.title, "Dentist (Nora)");
        assert!(row.dirty());

        // Google changed it again later than Nora: Google wins.
        db.gsync_apply(&[pulled("a", "Dentist (Google)", "2026-10-09 10:00", "2026-10-09 11:00", i64::MAX / 2)], false, today).unwrap();
        assert_eq!(linked(&db, id).event.title, "Dentist (Google)");

        // Deleted in Google: deleted here without a tombstone.
        db.gsync_apply(&[Pulled { cancelled: true, ..pulled("a", "", "2026-10-09 10:00", "2026-10-09 11:00", i64::MAX / 2) }], false, today).unwrap();
        assert!(find_linked(&db, "a").unwrap().is_none());
        assert!(db.gsync_tombstones().unwrap().is_empty());
        drop(db);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn series_instances_and_unsupported_rules() {
        let (db, path) = open("series");
        let today = d("2026-10-07");
        let series = Pulled { recurrence: vec!["RRULE:FREQ=DAILY;COUNT=5".into()], ..pulled("s", "Standup", "2026-10-05 09:00", "2026-10-05 09:15", 1) };
        let moved = Pulled {
            series: Some("s".into()), original_date: Some(d("2026-10-07")),
            ..pulled("s_20261007", "Standup (late)", "2026-10-07 11:00", "2026-10-07 11:15", 2)
        };
        let dropped = Pulled { cancelled: true, series: Some("s".into()), original_date: Some(d("2026-10-08")), ..pulled("s_20261008", "", "2026-10-08 09:00", "2026-10-08 09:15", 3) };
        db.gsync_apply(&[moved, series, dropped], true, today).unwrap();

        let s = find_linked(&db, "s").unwrap().unwrap();
        assert_eq!(s.event.repeat.as_ref().unwrap().count, Some(5));
        assert_eq!(s.event.exdates, vec![d("2026-10-07"), d("2026-10-08")]);
        assert!(!s.dirty(), "exdates from Google don't count as Nora changes");
        let m = find_linked(&db, "s_20261007").unwrap().unwrap();
        assert_eq!(m.google_series.as_deref(), Some("s"));
        // The modified instance's date is not sent back as an EXDATE.
        let lines = build_recurrence(&s.event, "UTC", &db.gsync_exception_dates("s").unwrap());
        assert_eq!(lines[1], "EXDATE;TZID=UTC:20261008T090000");

        let odd = Pulled { recurrence: vec!["RRULE:FREQ=MONTHLY;BYDAY=2TU".into()], ..pulled("odd", "Club", "2026-10-13 19:00", "2026-10-13 20:00", 1) };
        let out = db.gsync_apply(&[odd], false, today).unwrap();
        assert_eq!(out.unsupported, ["odd"]);
        assert!(find_linked(&db, "odd").unwrap().is_none());
        drop(db);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn full_pull_removes_rows_google_no_longer_has_and_skips_old_events() {
        let (db, path) = open("full");
        let today = d("2026-10-07");
        db.gsync_apply(
            &[pulled("keep", "Keep", "2026-10-08 10:00", "2026-10-08 11:00", 1), pulled("gone", "Gone", "2026-10-09 10:00", "2026-10-09 11:00", 1)],
            false,
            today,
        )
        .unwrap();
        db.gsync_apply(
            &[pulled("keep", "Keep", "2026-10-08 10:00", "2026-10-08 11:00", 1), pulled("ancient", "Old", "2020-01-01 10:00", "2020-01-01 11:00", 1)],
            true,
            today,
        )
        .unwrap();
        assert!(find_linked(&db, "keep").unwrap().is_some());
        assert!(find_linked(&db, "gone").unwrap().is_none());
        assert!(find_linked(&db, "ancient").unwrap().is_none());
        drop(db);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn google_ids_use_the_allowed_alphabet() {
        let (db, path) = open("ids");
        let id = db.save_event(&event("x", "2026-10-07 12:00", "2026-10-07 13:00"), Scope::All).unwrap();
        let g = google_id_for(&new_device_tag(), &linked(&db, id));
        assert!(g.len() >= 5 && g.chars().all(|c| c.is_ascii_digit() || ('a'..='v').contains(&c)));
        drop(db);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn bodies_carry_time_zone_and_exclusive_all_day_end() {
        let mut e = event("Trip", "2026-10-05 00:00", "2026-10-07 00:00");
        e.all_day = true;
        let b = to_google(&e, "Asia/Tokyo", &[], false);
        assert_eq!(b["start"]["date"], "2026-10-05");
        assert_eq!(b["end"]["date"], "2026-10-08");
        assert_eq!(b["recurrence"], json!([]));
        let t = to_google(&event("Call", "2026-10-05 09:00", "2026-10-05 09:30"), "Asia/Tokyo", &[], true);
        assert_eq!(t["start"]["timeZone"], "Asia/Tokyo");
        assert!(t.get("recurrence").is_none());
    }

    #[test]
    fn replaced_and_deleted_versions_can_be_restored() {
        let (db, path) = open("history");
        let today = d("2026-10-07");
        db.gsync_apply(&[pulled("a", "Lunch", "2026-10-08 12:00", "2026-10-08 13:00", 1)], false, today).unwrap();
        let id = find_linked(&db, "a").unwrap().unwrap().event.id;

        // Google changes it: Nora's old version is kept.
        db.gsync_apply(&[pulled("a", "Lunch with B", "2026-10-08 12:00", "2026-10-08 13:00", i64::MAX / 4)], false, today).unwrap();
        let h = db.gsync_history().unwrap();
        assert_eq!(h.len(), 1);
        assert_eq!((h[0].title.as_str(), h[0].reason.as_str()), ("Lunch", "google_changed"));

        // Restoring brings the title back and marks it to be sent to Google.
        assert_eq!(db.gsync_restore(h[0].id).unwrap(), Some(id));
        let row = linked(&db, id);
        assert_eq!(row.event.title, "Lunch");
        assert!(row.dirty());
        assert!(db.gsync_history().unwrap().is_empty());

        // Nora's edit is newer than Google's: Google's version is kept instead.
        db.gsync_apply(&[pulled("a", "Lunch (Google edit)", "2026-10-08 12:00", "2026-10-08 13:00", 5)], false, today).unwrap();
        assert_eq!(linked(&db, id).event.title, "Lunch");
        assert_eq!(db.gsync_history().unwrap()[0].reason, "nora_changed");

        // Deleted in Google: the event comes back as a new one when restored.
        db.gsync_mark_pushed(&linked(&db, id), "a").unwrap();
        db.gsync_apply(&[Pulled { cancelled: true, ..pulled("a", "", "2026-10-08 12:00", "2026-10-08 13:00", i64::MAX / 2) }], false, today).unwrap();
        let deleted = db.gsync_history().unwrap().into_iter().find(|h| h.reason == "google_deleted").unwrap();
        let new_id = db.gsync_restore(deleted.id).unwrap().unwrap();
        assert_eq!(linked(&db, new_id).event.title, "Lunch");
        assert!(linked(&db, new_id).google_id.is_none());
        drop(db);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn first_sync_links_identical_events_instead_of_copying() {
        let (db, path) = open("twin");
        let today = d("2026-10-07");
        let mine = db.save_event(&event("Dentist", "2026-10-09 10:00", "2026-10-09 11:00"), Scope::All).unwrap();
        db.save_event(&event("Only in Nora", "2026-10-10 10:00", "2026-10-10 11:00"), Scope::All).unwrap();
        let items = [
            pulled("g1", "Dentist", "2026-10-09 10:00", "2026-10-09 11:00", 1),
            pulled("g2", "Only in Google", "2026-10-11 10:00", "2026-10-11 11:00", 1),
        ];
        assert_eq!(db.gsync_preview(&items, today).unwrap(), Preview { send: 1, receive: 1, linked: 1 });

        db.gsync_apply(&items, true, today).unwrap();
        let twin = find_linked(&db, "g1").unwrap().unwrap();
        assert_eq!(twin.event.id, mine, "linked, not copied");
        assert!(!twin.dirty());
        let n: i64 = db.conn.query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 3);
        assert!(db.gsync_history().unwrap().is_empty(), "identical content is not history");
        drop(db);
        let _ = std::fs::remove_file(path);
    }
}
