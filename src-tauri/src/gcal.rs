//! Google Calendar import: read-only, Google → Nora (requirements FR-12.1–12.6).
//!
//! Nothing here talks to the network until the user connects an account (NFR-11).
//! The OAuth client secret and refresh token live in the OS credential store
//! (Windows Credential Manager / macOS Keychain), never in SQLite or backups (NFR-12).
//! Imported events are cached in `gcal_events`, so the calendar still shows them offline.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Mutex;
use std::time::{Duration as StdDuration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Duration, Local, NaiveDate, NaiveDateTime};
use rusqlite::{Row, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Manager};
use tauri_plugin_opener::OpenerExt;

use crate::AppState;
use crate::db::{Db, DbResult, Event, now_ts};

/// Key of the JSON config in the `settings` table.
const SETTING: &str = "gcal";
const KEYRING_SERVICE: &str = "com.otsu.scheduleapp.gcal";
const KEY_REFRESH: &str = "refresh-token";
const KEY_SECRET: &str = "client-secret";
/// Read-only access is all the import needs (NFR-13).
const SCOPE: &str = "https://www.googleapis.com/auth/calendar.readonly";
/// Added only when two-way sync is turned on (issue #4).
const SCOPE_WRITE: &str = "https://www.googleapis.com/auth/calendar.events";
const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const REVOKE_URL: &str = "https://oauth2.googleapis.com/revoke";
pub(crate) const API: &str = "https://www.googleapis.com/calendar/v3";
/// How long the browser sign-in may take before we give up.
const SIGN_IN_TIMEOUT: StdDuration = StdDuration::from_secs(300);
/// Lower bound for the auto-refresh interval, to stay well inside the API quota (NFR-15).
pub const MIN_INTERVAL_MIN: u32 = 5;
const DEFAULT_INTERVAL_MIN: u32 = 30;
/// Imported window around today.
const PAST_DAYS: i64 = 180;
const FUTURE_DAYS: i64 = 400;

const DT_FMT: &str = "%Y-%m-%d %H:%M";

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS gcal_events (
    id          INTEGER PRIMARY KEY,
    calendar_id TEXT    NOT NULL,
    event_id    TEXT    NOT NULL,
    title       TEXT    NOT NULL,
    start       TEXT    NOT NULL,
    end         TEXT    NOT NULL,
    all_day     INTEGER NOT NULL,
    location    TEXT    NOT NULL DEFAULT '',
    memo        TEXT    NOT NULL DEFAULT '',
    url         TEXT    NOT NULL DEFAULT '',
    reminder    INTEGER,
    stamp       INTEGER NOT NULL,
    UNIQUE(calendar_id, event_id)
);
CREATE INDEX IF NOT EXISTS idx_gcal_events_start ON gcal_events(start);
"#;

type CmdResult<T> = Result<T, String>;

/// Adds columns introduced after the table was first created.
pub fn migrate(conn: &rusqlite::Connection) -> DbResult<()> {
    let has_reminder = conn.prepare("SELECT 1 FROM pragma_table_info('gcal_events') WHERE name = 'reminder'")?.exists([])?;
    if !has_reminder {
        conn.execute_batch("ALTER TABLE gcal_events ADD COLUMN reminder INTEGER")?;
    }
    Ok(())
}

// ---- config -------------------------------------------------------------------

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GcalCalendar {
    pub id: String,
    pub name: String,
    /// 0xRRGGBB, from the calendar's color in Google.
    pub color: u32,
    pub primary: bool,
    /// Imported on sync.
    pub selected: bool,
    /// Shown on the Nora calendar (only meaningful when selected).
    pub visible: bool,
    /// The calendar's default pop-up reminder in Google, in minutes.
    #[serde(default)]
    pub default_reminder: Option<i64>,
    /// The account may add and change events here (owner or writer).
    #[serde(default)]
    pub writable: bool,
}

/// Which reminders imported events get (issue #1). Off until the user picks one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GcalReminder {
    #[default]
    Off,
    /// The pop-up reminder set on the event in Google (or the calendar's default).
    Google,
    /// The same number of minutes before every imported event.
    Before(i64),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct GcalConfig {
    /// OAuth client id of the user's own "Desktop app" client in Google Cloud.
    pub client_id: String,
    /// Label for the connected account (the primary calendar's id, usually the address).
    pub account: Option<String>,
    pub calendars: Vec<GcalCalendar>,
    pub interval_min: u32,
    pub reminder: GcalReminder,
    /// The calendar synced both ways with Nora's own events; `None` = import only.
    pub sync_calendar: Option<String>,
    /// Google's token for fetching only what changed since the last two-way sync.
    pub sync_token: Option<String>,
    /// Series in the synced calendar that Nora can't represent (shown read-only).
    pub sync_unsupported: Vec<String>,
    /// Scopes Google granted at the last sign-in.
    pub granted_scope: String,
    /// Makes this installation's Google event ids unique.
    pub device_tag: String,
    /// Unix seconds of the last successful sync.
    pub last_sync: Option<i64>,
    /// Message of the last failed sync, cleared on success.
    pub last_error: Option<String>,
}

impl Default for GcalConfig {
    fn default() -> Self {
        Self {
            client_id: String::new(),
            account: None,
            calendars: vec![],
            interval_min: DEFAULT_INTERVAL_MIN,
            reminder: GcalReminder::Off,
            sync_calendar: None,
            sync_token: None,
            sync_unsupported: vec![],
            granted_scope: String::new(),
            device_tag: String::new(),
            last_sync: None,
            last_error: None,
        }
    }
}

/// What the settings page and the calendar need to know.
#[derive(Clone, Debug, Serialize)]
pub struct GcalStatus {
    #[serde(flatten)]
    pub config: GcalConfig,
    /// A refresh token is stored on this machine. False after restoring a backup elsewhere.
    pub connected: bool,
    /// The sign-in allows changing events (needed for two-way sync).
    pub can_write: bool,
}

/// Marks an event that came from Google (read-only in Nora).
#[derive(Clone, Debug, Serialize)]
pub struct External {
    pub source: &'static str,
    pub calendar_id: String,
    pub calendar_name: String,
    /// Opens the event in Google Calendar.
    pub url: String,
}

/// An event as sent to the frontend: Nora's own (`external: None`) or imported.
#[derive(Clone, Debug, Serialize)]
pub struct CalItem {
    #[serde(flatten)]
    pub event: Event,
    pub external: Option<External>,
}

impl Db {
    pub fn gcal_config(&self) -> GcalConfig {
        self.setting(SETTING).ok().flatten().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    }

    fn set_gcal_config(&self, c: &GcalConfig) -> DbResult<()> {
        self.set_setting(SETTING, &serde_json::to_string(c).unwrap_or_default())
    }

    fn gcal_status(&self) -> GcalStatus {
        let config = self.gcal_config();
        let can_write = config.granted_scope.split_whitespace().any(|s| s == SCOPE_WRITE || s.ends_with("/auth/calendar"));
        GcalStatus { config, connected: secret(KEY_REFRESH).is_some(), can_write }
    }

    /// Imported events of visible calendars touching `from..=to`. Ids are negative so they
    /// never collide with Nora's own events.
    pub fn gcal_events_between(&self, from: NaiveDate, to: NaiveDate) -> DbResult<Vec<CalItem>> {
        let config = self.gcal_config();
        let shown: Vec<&GcalCalendar> = config.calendars.iter().filter(|c| c.selected && c.visible).collect();
        if shown.is_empty() {
            return Ok(vec![]);
        }
        let lo = from.and_hms_opt(0, 0, 0).unwrap().format(DT_FMT).to_string();
        let hi = (to + Duration::days(1)).and_hms_opt(0, 0, 0).unwrap().format(DT_FMT).to_string();
        let mut st = self.conn.prepare(
            "SELECT id, calendar_id, title, start, end, all_day, location, memo, url, reminder FROM gcal_events
             WHERE start < ?1 AND end >= ?2 ORDER BY all_day DESC, start",
        )?;
        let rows = st.query_map([hi, lo], |r: &Row| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, bool>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, String>(7)?,
                r.get::<_, String>(8)?,
                r.get::<_, Option<i64>>(9)?,
            ))
        })?;
        let mut out = vec![];
        for row in rows {
            let (id, cal, title, start, end, all_day, location, memo, url, google_reminder) = row?;
            let Some(c) = shown.iter().find(|c| c.id == cal) else { continue };
            let (Ok(start), Ok(end)) = (NaiveDateTime::parse_from_str(&start, DT_FMT), NaiveDateTime::parse_from_str(&end, DT_FMT)) else {
                continue;
            };
            let event = Event {
                id: -id,
                title,
                start,
                end,
                all_day,
                color: c.color,
                location,
                links: vec![],
                memo,
                repeat: None,
                exdates: vec![],
                occurrence: None,
                cancelled: false,
                tags: vec![],
                reminder: match config.reminder {
                    GcalReminder::Off => None,
                    GcalReminder::Google => google_reminder,
                    GcalReminder::Before(minutes) => Some(minutes),
                },
            };
            let (a, b) = event.date_span();
            if a <= to && b >= from {
                out.push(CalItem {
                    event,
                    external: Some(External { source: "google", calendar_id: c.id.clone(), calendar_name: c.name.clone(), url }),
                });
            }
        }
        Ok(out)
    }

    /// Replaces the cached events of `calendar_id` with `events`, keeping row ids stable.
    fn gcal_store(&self, calendar_id: &str, events: &[Imported]) -> DbResult<()> {
        let tx = self.conn.unchecked_transaction()?;
        // A fresh generation number; rows not touched by this sync are deleted below.
        let stamp: i64 = tx.query_row("SELECT COALESCE(MAX(stamp), 0) + 1 FROM gcal_events", [], |r| r.get(0))?;
        for e in events {
            tx.execute(
                "INSERT INTO gcal_events(calendar_id, event_id, title, start, end, all_day, location, memo, url, reminder, stamp)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT(calendar_id, event_id) DO UPDATE SET title=excluded.title, start=excluded.start,
                 end=excluded.end, all_day=excluded.all_day, location=excluded.location, memo=excluded.memo,
                 url=excluded.url, reminder=excluded.reminder, stamp=excluded.stamp",
                params![
                    calendar_id, e.event_id, e.title, e.start.format(DT_FMT).to_string(), e.end.format(DT_FMT).to_string(),
                    e.all_day, e.location, e.memo, e.url, e.reminder, stamp
                ],
            )?;
        }
        tx.execute("DELETE FROM gcal_events WHERE calendar_id=?1 AND stamp<>?2", params![calendar_id, stamp])?;
        tx.commit()
    }

    /// Drops cached events of calendars that are no longer imported (all of them for `keep = []`).
    fn gcal_prune(&self, keep: &[String]) -> DbResult<()> {
        let keep = serde_json::to_string(keep).unwrap_or_else(|_| "[]".into());
        self.conn.execute("DELETE FROM gcal_events WHERE calendar_id NOT IN (SELECT value FROM json_each(?1))", [keep])?;
        Ok(())
    }
}

// ---- credential store ---------------------------------------------------------------

fn entry(key: &str) -> Option<keyring::Entry> {
    keyring::Entry::new(KEYRING_SERVICE, key).ok()
}

fn secret(key: &str) -> Option<String> {
    entry(key)?.get_password().ok().filter(|s| !s.is_empty())
}

fn set_secret(key: &str, value: &str) -> CmdResult<()> {
    entry(key)
        .ok_or("credential store unavailable")?
        .set_password(value)
        .map_err(|e| format!("could not save to the credential store: {e}"))
}

fn forget_secret(key: &str) {
    if let Some(e) = entry(key) {
        let _ = e.delete_credential();
    }
}

// ---- HTTP -----------------------------------------------------------------------

pub(crate) fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(StdDuration::from_secs(30)))
        .http_status_as_error(false)
        .build()
        .into()
}

/// Error code the frontend shows as "sign in again": the sign-in expired or was revoked
/// (for example after 7 days while the Google project is in Testing).
pub const REAUTH: &str = "gcal_reauth";

/// Turns a non-2xx response into a short message for the settings page.
pub(crate) fn http_error(status: u16, body: &str) -> String {
    match status {
        401 => REAUTH.into(),
        400 if body.contains("invalid_grant") => REAUTH.into(),
        403 if body.contains("rateLimit") || body.contains("quota") => "Google API limit reached. Will retry later.".into(),
        429 => "Google API limit reached. Will retry later.".into(),
        _ => {
            let snippet: String = body.chars().take(200).collect();
            format!("Google returned {status}: {snippet}")
        }
    }
}

/// Retries rate-limit and server errors a few times with growing pauses (NFR-15).
fn with_retry(mut call: impl FnMut() -> Result<ureq::http::Response<ureq::Body>, ureq::Error>) -> CmdResult<serde_json::Value> {
    let mut wait = StdDuration::from_secs(1);
    for attempt in 0..4 {
        let mut resp = call().map_err(|e| format!("Could not reach Google: {e}"))?;
        let status = resp.status().as_u16();
        let body = resp.body_mut().read_to_string().map_err(|e| e.to_string())?;
        if (200..300).contains(&status) {
            return serde_json::from_str(&body).map_err(|e| e.to_string());
        }
        let retryable = status == 429 || status >= 500 || (status == 403 && body.contains("rateLimit"));
        if !retryable || attempt == 3 {
            return Err(http_error(status, &body));
        }
        std::thread::sleep(wait);
        wait *= 2;
    }
    unreachable!()
}

static ACCESS: Mutex<Option<(String, Instant)>> = Mutex::new(None);

/// A valid access token, refreshed with the stored refresh token when needed.
fn access_token(client_id: &str) -> CmdResult<String> {
    if let Some((token, until)) = ACCESS.lock().unwrap().clone()
        && Instant::now() < until
    {
        return Ok(token);
    }
    let refresh = secret(KEY_REFRESH).ok_or("Not connected to Google. Connect the account first.")?;
    let client_secret = secret(KEY_SECRET).unwrap_or_default();
    let json = with_retry(|| {
        agent().post(TOKEN_URL).send_form([
            ("client_id", client_id),
            ("client_secret", client_secret.as_str()),
            ("refresh_token", refresh.as_str()),
            ("grant_type", "refresh_token"),
        ])
    })?;
    remember_access(&json)
}

fn remember_access(json: &serde_json::Value) -> CmdResult<String> {
    let token = json["access_token"].as_str().ok_or("Google sent no access token")?.to_owned();
    let secs = json["expires_in"].as_u64().unwrap_or(3600).saturating_sub(60);
    *ACCESS.lock().unwrap() = Some((token.clone(), Instant::now() + StdDuration::from_secs(secs)));
    Ok(token)
}

pub(crate) fn get_json(token: &str, url: &str, query: &[(&str, &str)]) -> CmdResult<serde_json::Value> {
    with_retry(|| {
        let mut req = agent().get(url).header("Authorization", &format!("Bearer {token}"));
        for (k, v) in query {
            req = req.query(*k, *v);
        }
        req.call()
    })
}

fn parse_color(s: &str) -> u32 {
    u32::from_str_radix(s.trim_start_matches('#'), 16).unwrap_or(0x4aa8ff)
}

fn fetch_calendars(token: &str, previous: &[GcalCalendar]) -> CmdResult<Vec<GcalCalendar>> {
    let mut out = vec![];
    let mut page: Option<String> = None;
    loop {
        let mut query = vec![("maxResults", "250")];
        if let Some(p) = &page {
            query.push(("pageToken", p));
        }
        let json = get_json(token, &format!("{API}/users/me/calendarList"), &query)?;
        for item in json["items"].as_array().into_iter().flatten() {
            let id = item["id"].as_str().unwrap_or_default().to_owned();
            let primary = item["primary"].as_bool().unwrap_or(false);
            let old = previous.iter().find(|c| c.id == id);
            out.push(GcalCalendar {
                name: item["summaryOverride"].as_str().or(item["summary"].as_str()).unwrap_or(&id).to_owned(),
                color: parse_color(item["backgroundColor"].as_str().unwrap_or_default()),
                primary,
                // New calendars: import only the primary one until the user picks more.
                selected: old.map_or(primary, |c| c.selected),
                visible: old.is_none_or(|c| c.visible),
                default_reminder: popup_minutes(&item["defaultReminders"]),
                writable: matches!(item["accessRole"].as_str(), Some("owner" | "writer")),
                id,
            });
        }
        match json["nextPageToken"].as_str() {
            Some(p) => page = Some(p.to_owned()),
            None => break,
        }
    }
    out.sort_by_key(|c| (!c.primary, c.name.to_lowercase()));
    Ok(out)
}

/// One event instance as returned by Google, converted to Nora's local-time convention.
#[derive(Debug, PartialEq)]
struct Imported {
    event_id: String,
    title: String,
    start: NaiveDateTime,
    /// Exclusive end for timed events; the last day at 00:00 for all-day ones.
    end: NaiveDateTime,
    all_day: bool,
    location: String,
    memo: String,
    url: String,
    /// Pop-up reminder in Google, in minutes before the start.
    reminder: Option<i64>,
    /// The series this instance belongs to (not stored).
    series: Option<String>,
}

/// The earliest pop-up reminder in a Google reminder list (`[{method, minutes}]`).
fn popup_minutes(list: &serde_json::Value) -> Option<i64> {
    list.as_array()?.iter().filter(|r| r["method"].as_str() == Some("popup")).filter_map(|r| r["minutes"].as_i64()).max()
}

pub(crate) fn parse_when(v: &serde_json::Value) -> Option<(NaiveDateTime, bool)> {
    if let Some(dt) = v["dateTime"].as_str() {
        let local = DateTime::parse_from_rfc3339(dt).ok()?.with_timezone(&Local).naive_local();
        return Some((local, false));
    }
    let date = NaiveDate::parse_from_str(v["date"].as_str()?, "%Y-%m-%d").ok()?;
    Some((date.and_hms_opt(0, 0, 0)?, true))
}

/// Converts one Google event; `calendar_reminder` is the calendar's default pop-up reminder.
fn convert(item: &serde_json::Value, calendar_reminder: Option<i64>) -> Option<Imported> {
    if item["status"].as_str() == Some("cancelled") {
        return None;
    }
    let (start, all_day) = parse_when(&item["start"])?;
    let (end, _) = parse_when(&item["end"])?;
    // Google's all-day end date is exclusive; Nora stores the last day.
    let end = if all_day { (end - Duration::days(1)).max(start) } else { end.max(start) };
    Some(Imported {
        event_id: item["id"].as_str()?.to_owned(),
        title: item["summary"].as_str().unwrap_or("(No title)").to_owned(),
        start,
        end,
        all_day,
        location: item["location"].as_str().unwrap_or_default().to_owned(),
        memo: item["description"].as_str().unwrap_or_default().to_owned(),
        url: item["htmlLink"].as_str().unwrap_or_default().to_owned(),
        reminder: if item["reminders"]["useDefault"].as_bool().unwrap_or(true) {
            calendar_reminder
        } else {
            popup_minutes(&item["reminders"]["overrides"])
        },
        series: item["recurringEventId"].as_str().map(str::to_owned),
    })
}

fn fetch_events(token: &str, calendar: &GcalCalendar) -> CmdResult<Vec<Imported>> {
    let calendar_id = calendar.id.as_str();
    let now = Local::now();
    let time_min = (now - Duration::days(PAST_DAYS)).to_rfc3339();
    let time_max = (now + Duration::days(FUTURE_DAYS)).to_rfc3339();
    let url = format!("{API}/calendars/{}/events", urlencoding::encode(calendar_id));
    let mut out = vec![];
    let mut page: Option<String> = None;
    loop {
        // singleEvents expands repeating events into instances for us.
        let mut query = vec![
            ("singleEvents", "true"),
            ("maxResults", "2500"),
            ("timeMin", time_min.as_str()),
            ("timeMax", time_max.as_str()),
        ];
        if let Some(p) = &page {
            query.push(("pageToken", p));
        }
        let json = get_json(token, &url, &query)?;
        out.extend(json["items"].as_array().into_iter().flatten().filter_map(|item| convert(item, calendar.default_reminder)));
        match json["nextPageToken"].as_str() {
            Some(p) => page = Some(p.to_owned()),
            None => break,
        }
    }
    Ok(out)
}

// ---- sign-in (OAuth 2.0 for installed apps: loopback redirect + PKCE) --------------------

fn random_token(bytes: usize) -> CmdResult<String> {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).map_err(|e| e.to_string())?;
    Ok(URL_SAFE_NO_PAD.encode(buf))
}

const DONE_PAGE: &str = "<!doctype html><meta charset=utf-8><title>ScheduleApp</title>\
<body style=\"font-family:system-ui;padding:3em;text-align:center\"><h2>ScheduleApp</h2>\
<p>Google Calendar is connected. You can close this tab.</p>\
<p>Googleカレンダーと接続しました。このタブを閉じてください。</p></body>";

fn respond(stream: &mut TcpStream, status: &str, body: &str) {
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

/// Waits for Google to redirect the browser back to us and returns the `code`.
fn wait_for_code(listener: &TcpListener, state: &str) -> CmdResult<String> {
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let deadline = Instant::now() + SIGN_IN_TIMEOUT;
    loop {
        let mut stream = match listener.accept() {
            Ok((s, _)) => s,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() > deadline {
                    return Err("Sign-in timed out. Try connecting again.".into());
                }
                std::thread::sleep(StdDuration::from_millis(200));
                continue;
            }
            Err(e) => return Err(e.to_string()),
        };
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_read_timeout(Some(StdDuration::from_secs(5)));
        let mut line = String::new();
        if BufReader::new(&stream).read_line(&mut line).is_err() {
            continue;
        }
        // "GET /?code=…&state=… HTTP/1.1"
        let path = line.split_whitespace().nth(1).unwrap_or("/");
        let Ok(url) = url::Url::parse(&format!("http://127.0.0.1{path}")) else { continue };
        let get = |k: &str| url.query_pairs().find(|(key, _)| key == k).map(|(_, v)| v.into_owned());
        if let Some(err) = get("error") {
            respond(&mut stream, "200 OK", "<p>Sign-in was cancelled. You can close this tab.</p>");
            return Err(format!("Google sign-in was not completed ({err})."));
        }
        let Some(code) = get("code") else {
            respond(&mut stream, "404 Not Found", "");
            continue;
        };
        if get("state").as_deref() != Some(state) {
            respond(&mut stream, "400 Bad Request", "<p>State mismatch.</p>");
            return Err("Sign-in response did not match the request.".into());
        }
        respond(&mut stream, "200 OK", DONE_PAGE);
        return Ok(code);
    }
}

/// Runs the browser sign-in and returns the scopes Google granted.
fn sign_in(app: &AppHandle, client_id: &str, client_secret: &str, scope: &str) -> CmdResult<String> {
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    let redirect = format!("http://127.0.0.1:{}", listener.local_addr().map_err(|e| e.to_string())?.port());
    let verifier = random_token(48)?;
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = random_token(16)?;
    let url = url::Url::parse_with_params(
        AUTH_URL,
        &[
            ("client_id", client_id),
            ("redirect_uri", redirect.as_str()),
            ("response_type", "code"),
            ("scope", scope),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
            ("state", state.as_str()),
            ("access_type", "offline"),
            ("prompt", "consent"),
        ],
    )
    .map_err(|e| e.to_string())?;
    app.opener().open_url(url.as_str(), None::<&str>).map_err(|e| e.to_string())?;

    let code = wait_for_code(&listener, &state)?;
    let json = with_retry(|| {
        agent().post(TOKEN_URL).send_form([
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("code", code.as_str()),
            ("code_verifier", verifier.as_str()),
            ("redirect_uri", redirect.as_str()),
            ("grant_type", "authorization_code"),
        ])
    })?;
    let refresh = json["refresh_token"].as_str().ok_or("Google sent no refresh token. Try connecting again.")?;
    set_secret(KEY_SECRET, client_secret)?;
    set_secret(KEY_REFRESH, refresh)?;
    remember_access(&json)?;
    Ok(json["scope"].as_str().unwrap_or(scope).to_owned())
}

// ---- sync ---------------------------------------------------------------------------

pub(crate) fn with_db<T>(app: &AppHandle, f: impl FnOnce(&Db) -> T) -> CmdResult<T> {
    let state = app.state::<AppState>();
    let db = state.db.lock().map_err(|_| "database lock poisoned".to_string())?;
    Ok(f(&db))
}

/// Fetches every selected calendar and records the outcome in the config.
/// The database lock is not held while talking to Google.
fn sync_now(app: &AppHandle) -> CmdResult<GcalStatus> {
    let _guard = SYNCING.lock().map_err(|_| "sync lock poisoned".to_string())?;
    let config = with_db(app, |db| db.gcal_config())?;
    let result = (|| -> CmdResult<Vec<(String, Vec<Imported>)>> {
        let token = access_token(&config.client_id)?;
        // Two-way first: it decides which series of the synced calendar stay read-only.
        let unsupported = match &config.sync_calendar {
            Some(cal) => two_way(app, &token, cal, &config)?,
            None => vec![],
        };
        let mut fetched = vec![];
        for c in config.calendars.iter().filter(|c| c.selected) {
            let mut events = fetch_events(&token, c)?;
            if config.sync_calendar.as_ref() == Some(&c.id) {
                // Everything else in the synced calendar is already a Nora event.
                events.retain(|e| e.series.as_ref().is_some_and(|s| unsupported.contains(s)));
            }
            fetched.push((c.id.clone(), events));
        }
        Ok(fetched)
    })();
    with_db(app, |db| {
        let mut config = db.gcal_config();
        match result {
            Ok(fetched) => {
                for (id, events) in &fetched {
                    db.gcal_store(id, events).map_err(|e| e.to_string())?;
                }
                db.gcal_prune(&fetched.into_iter().map(|(id, _)| id).collect::<Vec<_>>()).map_err(|e| e.to_string())?;
                config.last_sync = Some(now_ts());
                config.last_error = None;
            }
            // Keep the cached events on failure; the page shows the error and the last sync time.
            Err(e) => config.last_error = Some(e),
        }
        db.set_gcal_config(&config).map_err(|e| e.to_string())?;
        Ok(db.gcal_status())
    })?
}

/// One sync at a time (timer, manual button and edits can overlap).
static SYNCING: Mutex<()> = Mutex::new(());

/// Pulls Google's changes into Nora, then pushes Nora's. Returns the series left read-only.
fn two_way(app: &AppHandle, token: &str, calendar: &str, config: &GcalConfig) -> CmdResult<Vec<String>> {
    let (items, next_token, full) = crate::gsync::pull(token, calendar, config.sync_token.as_deref())?;
    let today = Local::now().date_naive();
    let outcome = with_db(app, |db| db.gsync_apply(&items, full, today).map_err(|e| e.to_string()))??;
    let mut unsupported = if full { vec![] } else { config.sync_unsupported.clone() };
    unsupported.retain(|id| !outcome.supported.contains(id));
    for id in outcome.unsupported {
        if !unsupported.contains(&id) {
            unsupported.push(id);
        }
    }
    with_db(app, |db| {
        let mut c = db.gcal_config();
        c.sync_token = Some(next_token);
        c.sync_unsupported = unsupported.clone();
        db.set_gcal_config(&c).map_err(|e| e.to_string())
    })??;

    let tz = crate::gsync::time_zone();
    crate::gsync::push(token, calendar, &tz, &config.device_tag, |f| with_db(app, |db| f(db).map_err(|e| e.to_string()))?)?;
    Ok(unsupported)
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> CmdResult<T> + Send + 'static) -> CmdResult<T> {
    tauri::async_runtime::spawn_blocking(f).await.map_err(|e| e.to_string())?
}

// ---- commands -------------------------------------------------------------------------

#[tauri::command]
pub fn gcal_status(app: AppHandle) -> CmdResult<GcalStatus> {
    with_db(&app, |db| db.gcal_status())
}

/// Opens Google's consent page in the browser, waits for the user to allow access,
/// then loads the calendar list and runs a first sync.
/// `write` also asks for permission to change events (two-way sync). An empty secret
/// reuses the one stored at the last sign-in.
#[tauri::command]
pub async fn gcal_connect(app: AppHandle, client_id: String, client_secret: String, write: Option<bool>) -> CmdResult<GcalStatus> {
    let client_id = client_id.trim().to_owned();
    let client_secret = Some(client_secret.trim().to_owned()).filter(|s| !s.is_empty()).or_else(|| secret(KEY_SECRET)).unwrap_or_default();
    if client_id.is_empty() || client_secret.is_empty() {
        return Err("Enter the OAuth client ID and client secret.".into());
    }
    blocking(move || {
        let previous = with_db(&app, |db| db.gcal_config())?;
        let write = write.unwrap_or(previous.sync_calendar.is_some());
        let scope = if write { format!("{SCOPE} {SCOPE_WRITE}") } else { SCOPE.to_owned() };
        let granted_scope = sign_in(&app, &client_id, &client_secret, &scope)?;
        let token = access_token(&client_id)?;
        let calendars = fetch_calendars(&token, &previous.calendars)?;
        with_db(&app, |db| {
            let config = GcalConfig {
                client_id,
                account: calendars.iter().find(|c| c.primary).map(|c| c.id.clone()),
                calendars,
                granted_scope,
                last_error: None,
                ..previous
            };
            db.set_gcal_config(&config).map_err(|e| e.to_string())
        })??;
        sync_now(&app)
    })
    .await
}

/// Revokes the token, removes it from the credential store and deletes imported events.
#[tauri::command]
pub async fn gcal_disconnect(app: AppHandle) -> CmdResult<GcalStatus> {
    blocking(move || {
        if let Some(token) = secret(KEY_REFRESH) {
            let _ = agent().post(REVOKE_URL).send_form([("token", token.as_str())]);
        }
        forget_secret(KEY_REFRESH);
        forget_secret(KEY_SECRET);
        *ACCESS.lock().unwrap() = None;
        with_db(&app, |db| {
            let previous = db.gcal_config();
            // Keep the client id so reconnecting only needs the secret again.
            db.set_gcal_config(&GcalConfig { client_id: previous.client_id, interval_min: previous.interval_min, ..Default::default() })
                .map_err(|e| e.to_string())?;
            db.gcal_prune(&[]).map_err(|e| e.to_string())?;
            db.gsync_unlink_all().map_err(|e| e.to_string())?;
            Ok(db.gcal_status())
        })?
    })
    .await
}

/// What turning two-way sync on with `calendar_id` would send and receive (asked before starting).
#[tauri::command]
pub async fn gcal_sync_preview(app: AppHandle, calendar_id: String) -> CmdResult<crate::gsync::Preview> {
    blocking(move || {
        let config = with_db(&app, |db| db.gcal_config())?;
        let token = access_token(&config.client_id)?;
        let (items, _, _) = crate::gsync::pull(&token, &calendar_id, None)?;
        let today = Local::now().date_naive();
        with_db(&app, |db| db.gsync_preview(&items, today).map_err(|e| e.to_string()))?
    })
    .await
}

/// Versions replaced or deleted by two-way sync in the last 30 days, newest first.
#[tauri::command]
pub fn gcal_sync_history(app: AppHandle) -> CmdResult<Vec<crate::gsync::HistoryEntry>> {
    with_db(&app, |db| db.gsync_history().map_err(|e| e.to_string()))?
}

/// Puts a kept version back; the next sync sends it to Google.
#[tauri::command]
pub fn gcal_restore_history(app: AppHandle, id: i64) -> CmdResult<Option<i64>> {
    let restored = with_db(&app, |db| db.gsync_restore(id).map_err(|e| e.to_string()))??;
    crate::commands::events_changed(&app);
    Ok(restored)
}

/// Turns two-way sync on with `calendar_id`, moves it to another calendar, or (with `None`)
/// turns it off. Nora's events stay in Nora either way; links to the old calendar are dropped.
#[tauri::command]
pub async fn gcal_set_sync_calendar(app: AppHandle, calendar_id: Option<String>) -> CmdResult<GcalStatus> {
    blocking(move || {
        let changed = with_db(&app, |db| -> CmdResult<bool> {
            let status = db.gcal_status();
            let mut config = status.config;
            if config.sync_calendar == calendar_id {
                return Ok(false);
            }
            if let Some(id) = &calendar_id {
                let Some(cal) = config.calendars.iter_mut().find(|c| &c.id == id) else {
                    return Err("That calendar is not in the list. Reload the calendar list.".into());
                };
                if !cal.writable {
                    return Err("This calendar is read-only in Google.".into());
                }
                if !status.can_write {
                    return Err("Allow ScheduleApp to change events first (connect again).".into());
                }
                // Its read-only leftovers (unsupported series) are shown from the import.
                cal.selected = true;
                cal.visible = true;
            }
            if config.device_tag.is_empty() {
                config.device_tag = crate::gsync::new_device_tag();
            }
            config.sync_calendar = calendar_id.clone();
            config.sync_token = None;
            config.sync_unsupported = vec![];
            db.gsync_unlink_all().map_err(|e| e.to_string())?;
            db.set_gcal_config(&config).map_err(|e| e.to_string())?;
            Ok(true)
        })??;
        if changed { sync_now(&app) } else { with_db(&app, |db| db.gcal_status()) }
    })
    .await
}

#[tauri::command]
pub async fn gcal_sync(app: AppHandle) -> CmdResult<GcalStatus> {
    blocking(move || sync_now(&app)).await
}

/// Re-reads the account's calendar list (new or renamed calendars).
#[tauri::command]
pub async fn gcal_refresh_calendars(app: AppHandle) -> CmdResult<GcalStatus> {
    blocking(move || {
        let config = with_db(&app, |db| db.gcal_config())?;
        let token = access_token(&config.client_id)?;
        let calendars = fetch_calendars(&token, &config.calendars)?;
        with_db(&app, |db| {
            db.set_gcal_config(&GcalConfig { calendars, ..db.gcal_config() }).map_err(|e| e.to_string())?;
            Ok(db.gcal_status())
        })?
    })
    .await
}

/// Saves which calendars are imported/shown and the auto-refresh interval.
#[tauri::command]
pub fn gcal_update(app: AppHandle, calendars: Vec<GcalCalendar>, interval_min: u32, reminder: Option<GcalReminder>) -> CmdResult<GcalStatus> {
    with_db(&app, |db| {
        let mut config = db.gcal_config();
        for c in &mut config.calendars {
            if let Some(n) = calendars.iter().find(|n| n.id == c.id) {
                c.selected = n.selected;
                c.visible = n.visible;
            }
        }
        config.interval_min = interval_min.max(MIN_INTERVAL_MIN);
        if let Some(r) = reminder {
            config.reminder = match r {
                GcalReminder::Before(m) => GcalReminder::Before(m.clamp(0, crate::reminders::MAX_REMINDER_DAYS * 24 * 60)),
                other => other,
            };
        }
        let keep: Vec<String> = config.calendars.iter().filter(|c| c.selected).map(|c| c.id.clone()).collect();
        db.set_gcal_config(&config).map_err(|e| e.to_string())?;
        db.gcal_prune(&keep).map_err(|e| e.to_string())?;
        Ok(db.gcal_status())
    })?
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn dt(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, DT_FMT).unwrap()
    }

    #[test]
    fn expired_or_revoked_sign_ins_ask_to_reconnect() {
        assert_eq!(http_error(401, ""), REAUTH);
        assert_eq!(http_error(400, r#"{"error": "invalid_grant", "error_description": "Token has been expired or revoked."}"#), REAUTH);
        assert!(http_error(400, r#"{"error": "invalid_request"}"#).starts_with("Google returned 400"));
        assert!(http_error(429, "").contains("limit"));
    }

    #[test]
    fn all_day_end_becomes_last_day() {
        let e = convert(&json!({
            "id": "a", "summary": "Trip", "start": {"date": "2026-10-07"}, "end": {"date": "2026-10-10"}
        }), None)
        .unwrap();
        assert!(e.all_day);
        assert_eq!(e.start, dt("2026-10-07 00:00"));
        assert_eq!(e.end, dt("2026-10-09 00:00"));
    }

    #[test]
    fn timed_event_is_converted_to_local_time() {
        let e = convert(&json!({
            "id": "b", "start": {"dateTime": "2026-10-07T09:00:00Z"}, "end": {"dateTime": "2026-10-07T10:30:00Z"}
        }), None)
        .unwrap();
        assert!(!e.all_day);
        assert_eq!(e.title, "(No title)");
        assert_eq!(e.end - e.start, Duration::minutes(90));
        let utc = DateTime::parse_from_rfc3339("2026-10-07T09:00:00Z").unwrap();
        assert_eq!(e.start, utc.with_timezone(&Local).naive_local());
    }

    #[test]
    fn cancelled_and_malformed_items_are_skipped() {
        assert!(convert(&json!({"id": "c", "status": "cancelled", "start": {"date": "2026-10-07"}, "end": {"date": "2026-10-08"}}), None).is_none());
        assert!(convert(&json!({"id": "d"}), None).is_none());
    }

    #[test]
    fn cache_is_replaced_per_calendar_and_shown_with_negative_ids() {
        let path = std::env::temp_dir().join(format!("nora-gcal-test-{}.sqlite", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let db = Db::open(&path).unwrap();
        let ev = |id: &str, day: &str| Imported {
            event_id: id.into(),
            title: id.into(),
            start: dt(&format!("{day} 09:00")),
            end: dt(&format!("{day} 10:00")),
            all_day: false,
            location: String::new(),
            memo: String::new(),
            url: String::new(),
            reminder: Some(15),
            series: None,
        };
        db.set_gcal_config(&GcalConfig {
            calendars: vec![GcalCalendar {
                id: "cal".into(),
                name: "Work".into(),
                color: 0x123456,
                primary: true,
                selected: true,
                visible: true,
                default_reminder: None,
                writable: true,
            }],
            ..Default::default()
        })
        .unwrap();
        let d = |s: &str| NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap();

        db.gcal_store("cal", &[ev("x", "2026-10-07"), ev("y", "2026-10-08")]).unwrap();
        let first = db.gcal_events_between(d("2026-10-01"), d("2026-10-31")).unwrap();
        assert_eq!(first.len(), 2);
        assert!(first.iter().all(|i| i.event.id < 0 && i.event.color == 0x123456 && i.external.is_some()));

        // "y" was deleted in Google; "x" keeps its id.
        db.gcal_store("cal", &[ev("x", "2026-10-07")]).unwrap();
        let second = db.gcal_events_between(d("2026-10-01"), d("2026-10-31")).unwrap();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].event.id, first[0].event.id);

        // Hidden calendars are not shown; unselected ones are pruned.
        let mut c = db.gcal_config();
        c.calendars[0].visible = false;
        db.set_gcal_config(&c).unwrap();
        assert!(db.gcal_events_between(d("2026-10-01"), d("2026-10-31")).unwrap().is_empty());
        db.gcal_prune(&[]).unwrap();
        let n: i64 = db.conn.query_row("SELECT COUNT(*) FROM gcal_events", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0);
        drop(db);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn reminders_follow_the_event_or_the_calendar_default() {
        let base = json!({"id": "r", "start": {"date": "2026-10-07"}, "end": {"date": "2026-10-08"}});
        // useDefault (or no reminders field) falls back to the calendar's default.
        assert_eq!(convert(&base, Some(30)).unwrap().reminder, Some(30));
        let mut own = base.clone();
        own["reminders"] = json!({"useDefault": false, "overrides": [
            {"method": "email", "minutes": 1440}, {"method": "popup", "minutes": 10}, {"method": "popup", "minutes": 60}
        ]});
        // Only pop-ups count; the earliest one wins.
        assert_eq!(convert(&own, Some(30)).unwrap().reminder, Some(60));
        own["reminders"] = json!({"useDefault": false});
        assert_eq!(convert(&own, Some(30)).unwrap().reminder, None);
    }

    #[test]
    fn reminder_mode_decides_what_imported_events_carry() {
        let path = std::env::temp_dir().join(format!("nora-gcal-remind-{}.sqlite", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let db = Db::open(&path).unwrap();
        let d = |s: &str| NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap();
        let mut config = GcalConfig {
            calendars: vec![GcalCalendar { id: "cal".into(), selected: true, visible: true, ..Default::default() }],
            ..Default::default()
        };
        let ev = Imported {
            event_id: "x".into(),
            title: "x".into(),
            start: dt("2026-10-07 09:00"),
            end: dt("2026-10-07 10:00"),
            all_day: false,
            location: String::new(),
            memo: String::new(),
            url: String::new(),
            reminder: Some(15),
            series: None,
        };
        db.gcal_store("cal", &[ev]).unwrap();
        let reminder = |db: &Db| db.gcal_events_between(d("2026-10-07"), d("2026-10-07")).unwrap()[0].event.reminder;
        for (mode, want) in [(GcalReminder::Off, None), (GcalReminder::Google, Some(15)), (GcalReminder::Before(5), Some(5))] {
            config.reminder = mode;
            db.set_gcal_config(&config).unwrap();
            assert_eq!(reminder(&db), want);
        }
        drop(db);
        let _ = std::fs::remove_file(path);
    }
}
