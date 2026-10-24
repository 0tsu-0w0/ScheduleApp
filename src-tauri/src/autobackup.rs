//! Automatic backups (issue #2): the same file as Settings → Export, written on a schedule
//! into a folder the user picks, keeping the newest few.
//!
//! A background thread checks once a minute whether a backup is due, so it also runs while
//! the window is hidden. It is off until the user turns it on.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use chrono::Local;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

use crate::AppState;
use crate::backup;
use crate::db::{self, Db, now_ts};

/// User choices, in `settings` under this key.
const CONFIG_KEY: &str = "autobackup";
/// Outcome of the last run. Kept apart from the config so the thread never overwrites a
/// change the user is making at the same moment.
const STATUS_KEY: &str = "autobackup.status";
const CHECK_EVERY: Duration = Duration::from_secs(60);
/// Our files start with this; pruning never touches anything else in the folder.
const PREFIX: &str = "ScheduleApp-auto-";
const EXT: &str = "nora";
pub const MIN_KEEP: u32 = 1;
pub const MAX_KEEP: u32 = 100;

type CmdResult<T> = Result<T, String>;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AutoBackupConfig {
    pub enabled: bool,
    /// Folder the backups go to; empty until chosen.
    pub dir: String,
    /// 1 = daily, 7 = weekly.
    pub interval_days: u32,
    /// How many automatic backups to keep (older ones are deleted).
    pub keep: u32,
}

impl Default for AutoBackupConfig {
    fn default() -> Self {
        Self { enabled: false, dir: String::new(), interval_days: 1, keep: 7 }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AutoBackupRun {
    /// Unix seconds of the last successful backup.
    pub last_run: Option<i64>,
    /// File name of the last successful backup.
    pub last_file: Option<String>,
    /// Message of the last failure, cleared on success.
    pub last_error: Option<String>,
    /// Unix seconds of the last failed attempt (retried after an hour, not every minute).
    pub last_failure: Option<i64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct AutoBackupStatus {
    pub config: AutoBackupConfig,
    #[serde(flatten)]
    pub run: AutoBackupRun,
}

fn read<T: Default + for<'de> Deserialize<'de>>(db: &Db, key: &str) -> T {
    db.setting(key).ok().flatten().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

fn write<T: Serialize>(db: &Db, key: &str, value: &T) -> CmdResult<()> {
    db.set_setting(key, &serde_json::to_string(value).map_err(|e| e.to_string())?).map_err(|e| e.to_string())
}

fn status(db: &Db) -> AutoBackupStatus {
    AutoBackupStatus { config: read(db, CONFIG_KEY), run: read(db, STATUS_KEY) }
}

/// Whether a scheduled backup should run at `now`.
fn is_due(config: &AutoBackupConfig, run: &AutoBackupRun, now: i64) -> bool {
    if !config.enabled || config.dir.is_empty() {
        return false;
    }
    // After a failure, wait an hour before trying again (e.g. an unplugged drive).
    if run.last_failure.is_some_and(|t| now - t < 3600) {
        return false;
    }
    run.last_run.is_none_or(|t| now - t >= i64::from(config.interval_days.max(1)) * 86_400)
}

/// Our backups in `dir`, oldest first (the names sort by time).
fn our_backups(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    p.is_file() && name.starts_with(PREFIX) && name.ends_with(&format!(".{EXT}"))
                })
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files
}

/// Deletes our oldest backups so that at most `keep` remain.
fn prune(dir: &Path, keep: u32) {
    let files = our_backups(dir);
    let extra = files.len().saturating_sub(keep.clamp(MIN_KEEP, MAX_KEEP) as usize);
    for old in &files[..extra] {
        let _ = std::fs::remove_file(old);
    }
}

/// One backup run at a time, whether scheduled or started from Settings.
static RUNNING: Mutex<()> = Mutex::new(());

/// Writes one backup into the configured folder and records the outcome.
/// Uses its own database connection, so it never waits on the app's.
fn run_once(data_dir: &Path, images_dir: &Path) -> CmdResult<AutoBackupStatus> {
    let _guard = RUNNING.lock().map_err(|_| "backup lock poisoned".to_string())?;
    let db = Db { conn: db::open_conn(&data_dir.join(backup::DB_FILE)).map_err(|e| e.to_string())? };
    let config: AutoBackupConfig = read(&db, CONFIG_KEY);
    let mut run: AutoBackupRun = read(&db, STATUS_KEY);

    let result = (|| -> CmdResult<String> {
        if config.dir.is_empty() {
            return Err("No backup folder chosen.".into());
        }
        let dir = Path::new(&config.dir);
        if !dir.is_dir() {
            return Err(format!("Backup folder not found: {}", config.dir));
        }
        let name = format!("{PREFIX}{}.{EXT}", Local::now().format("%Y-%m-%d_%H%M%S"));
        let scratch = data_dir.join("autobackup-snapshot.tmp");
        let counts = backup::prepare_export(&db, &scratch)?;
        backup::write_backup(&scratch, images_dir, counts, &dir.join(&name))?;
        prune(dir, config.keep);
        Ok(name)
    })();

    let now = now_ts();
    match result {
        Ok(name) => {
            run = AutoBackupRun { last_run: Some(now), last_file: Some(name), last_error: None, last_failure: None };
        }
        Err(e) => {
            run.last_error = Some(e);
            run.last_failure = Some(now);
        }
    }
    write(&db, STATUS_KEY, &run)?;
    Ok(AutoBackupStatus { config, run })
}

pub fn start(data_dir: PathBuf, images_dir: PathBuf) {
    let spawned = std::thread::Builder::new().name("autobackup".into()).spawn(move || {
        loop {
            std::thread::sleep(CHECK_EVERY);
            let due = db::open_conn(&data_dir.join(backup::DB_FILE))
                .map(|conn| {
                    let db = Db { conn };
                    is_due(&read(&db, CONFIG_KEY), &read(&db, STATUS_KEY), now_ts())
                })
                .unwrap_or(false);
            if due && let Err(e) = run_once(&data_dir, &images_dir) {
                eprintln!("automatic backup failed: {e}");
            }
        }
    });
    if let Err(e) = spawned {
        eprintln!("failed to start automatic backups: {e}");
    }
}

// ---- commands -------------------------------------------------------------------------

fn with_db<T>(app: &AppHandle, f: impl FnOnce(&Db) -> T) -> CmdResult<T> {
    let state = app.state::<AppState>();
    let db = state.db.lock().map_err(|_| "database lock poisoned".to_string())?;
    Ok(f(&db))
}

#[tauri::command]
pub fn autobackup_status(app: AppHandle) -> CmdResult<AutoBackupStatus> {
    with_db(&app, status)
}

#[tauri::command]
pub fn autobackup_update(app: AppHandle, mut config: AutoBackupConfig) -> CmdResult<AutoBackupStatus> {
    config.dir = config.dir.trim().to_owned();
    config.interval_days = if config.interval_days >= 7 { 7 } else { 1 };
    config.keep = config.keep.clamp(MIN_KEEP, MAX_KEEP);
    with_db(&app, |db| {
        write(db, CONFIG_KEY, &config)?;
        // A new setup should not wait an hour because of an old failure.
        let mut run: AutoBackupRun = read(db, STATUS_KEY);
        run.last_failure = None;
        write(db, STATUS_KEY, &run)?;
        Ok(status(db))
    })?
}

/// Backs up right away into the configured folder.
#[tauri::command]
pub async fn autobackup_run_now(app: AppHandle) -> CmdResult<AutoBackupStatus> {
    let (data_dir, images_dir) = {
        let state = app.state::<AppState>();
        (state.data_dir.clone(), state.images_dir.clone())
    };
    tauri::async_runtime::spawn_blocking(move || run_once(&data_dir, &images_dir)).await.map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nora-autobackup-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn due_only_when_enabled_with_a_folder_and_the_interval_has_passed() {
        let on = AutoBackupConfig { enabled: true, dir: "x".into(), interval_days: 1, keep: 7 };
        let never = AutoBackupRun::default();
        assert!(is_due(&on, &never, 1_000_000));
        assert!(!is_due(&AutoBackupConfig { enabled: false, ..on.clone() }, &never, 1_000_000));
        assert!(!is_due(&AutoBackupConfig { dir: String::new(), ..on.clone() }, &never, 1_000_000));

        let ran = AutoBackupRun { last_run: Some(1_000_000), ..Default::default() };
        assert!(!is_due(&on, &ran, 1_000_000 + 86_399));
        assert!(is_due(&on, &ran, 1_000_000 + 86_400));
        let weekly = AutoBackupConfig { interval_days: 7, ..on.clone() };
        assert!(!is_due(&weekly, &ran, 1_000_000 + 86_400));

        // A failure pauses retries for an hour.
        let failed = AutoBackupRun { last_failure: Some(2_000_000), ..Default::default() };
        assert!(!is_due(&on, &failed, 2_000_000 + 600));
        assert!(is_due(&on, &failed, 2_000_000 + 3_600));
    }

    #[test]
    fn prune_keeps_the_newest_and_leaves_other_files_alone() {
        let dir = temp_dir("prune");
        for name in ["ScheduleApp-auto-2026-10-01_090000.nora", "ScheduleApp-auto-2026-10-02_090000.nora", "ScheduleApp-auto-2026-10-03_090000.nora", "my-backup.nora", "ScheduleApp-auto-notes.txt"] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        prune(&dir, 2);
        let mut left: Vec<String> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(left, ["ScheduleApp-auto-2026-10-02_090000.nora", "ScheduleApp-auto-2026-10-03_090000.nora", "ScheduleApp-auto-notes.txt", "my-backup.nora"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_run_writes_a_readable_backup_and_records_failures() {
        let data = temp_dir("data");
        let out = temp_dir("out");
        let images = data.join("images");
        std::fs::create_dir_all(&images).unwrap();
        let db = Db::open(&data.join(backup::DB_FILE)).unwrap();

        // No folder yet: recorded as an error, nothing written.
        let s = run_once(&data, &images).unwrap();
        assert!(s.run.last_error.is_some() && s.run.last_run.is_none());

        write(&db, CONFIG_KEY, &AutoBackupConfig { enabled: true, dir: out.to_string_lossy().into(), interval_days: 1, keep: 3 }).unwrap();
        let s = run_once(&data, &images).unwrap();
        assert!(s.run.last_error.is_none() && s.run.last_run.is_some());
        let files = our_backups(&out);
        assert_eq!(files.len(), 1);
        assert_eq!(backup::inspect(&files[0]).unwrap().format, backup::FORMAT);
        assert!(!data.join("autobackup-snapshot.tmp").exists());

        drop(db);
        let _ = std::fs::remove_dir_all(data);
        let _ = std::fs::remove_dir_all(out);
    }
}
