//! Notion export (issue #5): memos, books and TRPG records → databases in the user's own
//! Notion workspace. One way only: Nora → Notion. Off until the user connects.
//!
//! - The user makes an internal integration in Notion and pastes its token; the token
//!   lives in the OS credential store, never in SQLite or backups.
//! - For each kind of data the user picks an existing database and maps Nora's fields to
//!   its properties. The Markdown text (memo body, review, TRPG memo) can become the
//!   page's content.
//! - `notion_pages` remembers which page holds which item and hashes of what was sent,
//!   so unchanged items cost no requests and nothing is duplicated (FR-13.5).
//! - Items deleted in Nora are archived in Notion.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, Local, NaiveDate};
use pulldown_cmark::{CodeBlockKind, Event as Md, HeadingLevel, Options, Parser, Tag, TagEnd};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Manager};

use crate::AppState;
use crate::backup;
use crate::db::{self, Db, DbResult, now_ts};

type CmdResult<T> = Result<T, String>;

const CONFIG_KEY: &str = "notion";
const RUN_KEY: &str = "notion.status";
const KEYRING_SERVICE: &str = "com.nora.schedule.notion";
const KEY_TOKEN: &str = "token";
const API: &str = "https://api.notion.com/v1";
const VERSION: &str = "2022-06-28";
/// How often automatic export checks for changes.
const AUTO_EVERY: Duration = Duration::from_secs(5 * 60);
/// Notion's limit for one piece of rich text.
const TEXT_LIMIT: usize = 2000;
/// Notion accepts at most this many blocks per request.
const BLOCK_BATCH: usize = 100;

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS notion_pages (
    kind        TEXT    NOT NULL,
    item_id     INTEGER NOT NULL,
    database_id TEXT    NOT NULL,
    page_id     TEXT    NOT NULL,
    props_hash  TEXT    NOT NULL,
    body_hash   TEXT    NOT NULL,
    PRIMARY KEY (kind, item_id)
);";

// ---- kinds and fields --------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Memo,
    Book,
    Trpg,
}

impl Kind {
    pub const ALL: [Kind; 3] = [Kind::Memo, Kind::Book, Kind::Trpg];

    fn key(self) -> &'static str {
        match self {
            Kind::Memo => "memo",
            Kind::Book => "book",
            Kind::Trpg => "trpg",
        }
    }
}

/// What a Nora field holds, which decides the Notion property types it can go into.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ValueKind {
    Text,
    List,
    Number,
    Date,
    Url,
}

impl ValueKind {
    /// Notion property types this value can be written to.
    pub fn accepts(self, ty: &str) -> bool {
        match self {
            ValueKind::Text => matches!(ty, "title" | "rich_text" | "select" | "multi_select" | "status"),
            ValueKind::List => matches!(ty, "multi_select" | "rich_text" | "select"),
            ValueKind::Number => matches!(ty, "number" | "rich_text"),
            ValueKind::Date => matches!(ty, "date" | "rich_text"),
            ValueKind::Url => matches!(ty, "url" | "rich_text"),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Field {
    pub id: &'static str,
    pub value: ValueKind,
}

pub fn fields(kind: Kind) -> Vec<Field> {
    use ValueKind::*;
    let list: &[(&'static str, ValueKind)] = match kind {
        Kind::Memo => &[("title", Text), ("group", Text), ("created", Date), ("updated", Date)],
        Kind::Book => &[
            ("title", Text),
            ("author", Text),
            ("publisher", Text),
            ("status", Text),
            ("pages", Number),
            ("current_page", Number),
            ("progress", Number),
            ("rating", Number),
            ("started", Date),
            ("finished", Date),
        ],
        Kind::Trpg => &[
            ("title", Text),
            ("kind", Text),
            ("writer", Text),
            ("system", Text),
            ("date", Date),
            ("roles", List),
            ("folder", Text),
            ("link", Url),
        ],
    };
    list.iter().map(|(id, value)| Field { id, value: *value }).collect()
}

#[derive(Clone, Debug, PartialEq)]
pub enum Val {
    Text(String),
    List(Vec<String>),
    Number(Option<f64>),
    Date(Option<NaiveDate>),
    Url(Option<String>),
}

impl Val {
    fn plain(&self) -> String {
        match self {
            Val::Text(t) => t.clone(),
            Val::List(l) => l.join(", "),
            Val::Number(n) => n.map(fmt_number).unwrap_or_default(),
            Val::Date(d) => d.map(|d| d.to_string()).unwrap_or_default(),
            Val::Url(u) => u.clone().unwrap_or_default(),
        }
    }
}

fn fmt_number(n: f64) -> String {
    if n.fract() == 0.0 { format!("{}", n as i64) } else { format!("{n}") }
}

/// One Nora item ready to export.
#[derive(Clone, Debug)]
pub struct Item {
    pub id: i64,
    pub values: HashMap<&'static str, Val>,
    /// Markdown written as the page's content.
    pub body: String,
}

fn label(locale: &str, en: &str, ko: &str, ja: &str) -> String {
    match locale {
        "ko" => ko,
        "ja" => ja,
        _ => en,
    }
    .to_owned()
}

fn ts_date(ts: i64) -> Option<NaiveDate> {
    DateTime::from_timestamp(ts, 0).map(|t| t.with_timezone(&Local).date_naive())
}

/// Reads the items of `kind`, with select-style values in the user's language.
pub fn items(db: &Db, kind: Kind, locale: &str) -> DbResult<Vec<Item>> {
    let mut out = vec![];
    match kind {
        Kind::Memo => {
            let groups: HashMap<i64, String> = db.memo_groups()?.into_iter().map(|g| (g.id, g.name)).collect();
            for m in db.memos()? {
                let values = HashMap::from([
                    ("title", Val::Text(m.title.clone())),
                    ("group", Val::Text(m.group_id.and_then(|g| groups.get(&g).cloned()).unwrap_or_default())),
                    ("created", Val::Date(ts_date(m.created_at))),
                    ("updated", Val::Date(ts_date(m.updated_at))),
                ]);
                out.push(Item { id: m.id, values, body: m.body });
            }
        }
        Kind::Book => {
            for b in db.books()? {
                let status = match b.status.as_str() {
                    "want" => label(locale, "Want to read", "읽고 싶은 책", "読みたい本"),
                    "reading" => label(locale, "Reading", "읽는 중", "読んでいる本"),
                    "read" => label(locale, "Read", "다 읽은 책", "読んだ本"),
                    other => other.to_owned(),
                };
                let progress = (b.total_pages > 0).then(|| (f64::from(b.current_page.min(b.total_pages)) * 100.0 / f64::from(b.total_pages)).round());
                let values = HashMap::from([
                    ("title", Val::Text(b.title.clone())),
                    ("author", Val::Text(b.author.clone())),
                    ("publisher", Val::Text(b.publisher.clone())),
                    ("status", Val::Text(status)),
                    ("pages", Val::Number((b.total_pages > 0).then_some(f64::from(b.total_pages)))),
                    ("current_page", Val::Number(Some(f64::from(b.current_page)))),
                    ("progress", Val::Number(progress)),
                    ("rating", Val::Number((b.rating > 0).then_some(f64::from(b.rating) / 2.0))),
                    ("started", Val::Date(b.started)),
                    ("finished", Val::Date(b.finished)),
                ]);
                out.push(Item { id: b.id, values, body: b.review });
            }
        }
        Kind::Trpg => {
            for e in db.trpg_entries()? {
                let kind = match e.kind.as_str() {
                    "rulebook" => label(locale, "Rule book", "룰북", "ルールブック"),
                    "scenario_book" => label(locale, "Scenario book", "시나리오북", "シナリオ集"),
                    "played" => label(locale, "Played", "플레이함", "遊んだ"),
                    "wishlist" => label(locale, "Want to play", "하고 싶은 시나리오", "遊びたい"),
                    other => other.to_owned(),
                };
                let roles = e
                    .role
                    .split(',')
                    .map(str::trim)
                    .filter(|r| !r.is_empty())
                    .map(|r| match r {
                        "gm" => "GM".to_owned(),
                        "pl" => "PL".to_owned(),
                        other => other.to_owned(),
                    })
                    .collect();
                let values = HashMap::from([
                    ("title", Val::Text(e.title.clone())),
                    ("kind", Val::Text(kind)),
                    ("writer", Val::Text(e.writer.clone())),
                    ("system", Val::Text(e.system.clone())),
                    ("date", Val::Date(e.date)),
                    ("roles", Val::List(roles)),
                    ("folder", Val::Text(e.folder.clone())),
                    ("link", Val::Url(e.links.first().cloned())),
                ]);
                out.push(Item { id: e.id, values, body: e.memo });
            }
        }
    }
    Ok(out)
}

// ---- values -> Notion properties ---------------------------------------------------------

fn rich_text(text: &str) -> Value {
    let chunks: Vec<Value> = chunk(text).into_iter().map(|c| json!({ "type": "text", "text": { "content": c } })).collect();
    json!(chunks)
}

fn chunk(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    chars.chunks(TEXT_LIMIT).map(|c| c.iter().collect()).collect()
}

/// Notion option names may not contain commas and are limited to 100 characters.
fn option_name(s: &str) -> String {
    s.replace(',', " ").trim().chars().take(100).collect()
}

/// The property value for `v` in a property of Notion type `ty`; `None` if it can't go there.
pub fn prop_value(v: &Val, ty: &str) -> Option<Value> {
    let text = v.plain();
    Some(match ty {
        "title" => json!({ "title": rich_text(&text) }),
        "rich_text" => json!({ "rich_text": rich_text(&text) }),
        "select" => {
            let first = match v {
                Val::List(l) => l.first().cloned().unwrap_or_default(),
                _ => text,
            };
            let name = option_name(&first);
            if name.is_empty() { json!({ "select": null }) } else { json!({ "select": { "name": name } }) }
        }
        "multi_select" => {
            let names: Vec<String> = match v {
                Val::List(l) => l.iter().map(|s| option_name(s)).collect(),
                _ => vec![option_name(&text)],
            };
            let names: Vec<Value> = names.into_iter().filter(|n| !n.is_empty()).map(|n| json!({ "name": n })).collect();
            json!({ "multi_select": names })
        }
        "status" => {
            let name = option_name(&text);
            if name.is_empty() { json!({ "status": null }) } else { json!({ "status": { "name": name } }) }
        }
        "number" => match v {
            Val::Number(n) => json!({ "number": n }),
            _ => return None,
        },
        "date" => match v {
            Val::Date(d) => match d {
                Some(d) => json!({ "date": { "start": d.to_string() } }),
                None => json!({ "date": null }),
            },
            _ => return None,
        },
        "url" => json!({ "url": if text.is_empty() { Value::Null } else { json!(text) } }),
        _ => return None,
    })
}

// ---- Markdown -> Notion blocks -----------------------------------------------------------

#[derive(Default)]
struct Style {
    bold: u32,
    italic: u32,
    strike: u32,
    link: Option<String>,
}

enum OpenKind {
    Paragraph,
    Heading(u8),
    Quote,
    Code(String),
    Item { ordered: bool, checked: Option<bool> },
}

struct Open {
    kind: OpenKind,
    spans: Vec<Value>,
    children: Vec<Value>,
}

fn span(text: &str, style: &Style, code: bool) -> Vec<Value> {
    chunk(text)
        .into_iter()
        .map(|c| {
            json!({
                "type": "text",
                "text": { "content": c, "link": style.link.as_ref().map(|u| json!({ "url": u })) },
                "annotations": { "bold": style.bold > 0, "italic": style.italic > 0, "strikethrough": style.strike > 0, "code": code },
            })
        })
        .collect()
}

fn code_language(lang: &str) -> &'static str {
    match lang.trim().to_ascii_lowercase().as_str() {
        "rust" | "rs" => "rust",
        "js" | "javascript" => "javascript",
        "ts" | "typescript" => "typescript",
        "py" | "python" => "python",
        "json" => "json",
        "sh" | "bash" | "shell" | "zsh" => "shell",
        "powershell" | "ps1" => "powershell",
        "html" => "html",
        "css" => "css",
        "sql" => "sql",
        "java" => "java",
        "c" => "c",
        "cpp" | "c++" => "c++",
        "go" => "go",
        "yaml" | "yml" => "yaml",
        "markdown" | "md" => "markdown",
        _ => "plain text",
    }
}

fn block(ty: &str, content: Value) -> Value {
    json!({ "object": "block", "type": ty, ty: content })
}

/// Converts Markdown to Notion blocks. Returns the blocks and how many elements (images,
/// tables, raw HTML) could not be converted (FR-13.6). Lists nest one level at most.
pub fn markdown_to_blocks(md: &str) -> (Vec<Value>, u32) {
    let mut out: Vec<Value> = vec![];
    let mut skipped = 0;
    let mut stack: Vec<Open> = vec![];
    let mut lists: Vec<bool> = vec![];
    let mut style = Style::default();
    // Inside an image or table, whose contents are dropped.
    let mut dropping = 0u32;

    fn emit(stack: &mut [Open], out: &mut Vec<Value>, b: Value) {
        // Nested list items hang under the outermost open item (Notion takes limited depth).
        match stack.iter_mut().find(|o| matches!(o.kind, OpenKind::Item { .. })) {
            Some(parent) => parent.children.push(b),
            None => out.push(b),
        }
    }
    fn push_spans(stack: &mut Vec<Open>, out: &mut Vec<Value>, spans: Vec<Value>) {
        match stack.last_mut() {
            Some(top) => top.spans.extend(spans),
            None => {
                // Loose inline text outside any block becomes its own paragraph.
                out.push(block("paragraph", json!({ "rich_text": spans })));
            }
        }
    }

    let parser = Parser::new_ext(md, Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS | Options::ENABLE_TABLES);
    for ev in parser {
        if dropping > 0 {
            match ev {
                Md::Start(Tag::Image { .. } | Tag::Table(_)) => dropping += 1,
                Md::End(TagEnd::Image | TagEnd::Table) => dropping -= 1,
                _ => {}
            }
            continue;
        }
        match ev {
            Md::Start(Tag::Paragraph) => {
                let inside = matches!(stack.last().map(|o| &o.kind), Some(OpenKind::Item { .. } | OpenKind::Quote));
                if inside {
                    if let Some(top) = stack.last_mut()
                        && !top.spans.is_empty()
                    {
                        top.spans.extend(span("\n", &Style::default(), false));
                    }
                } else {
                    stack.push(Open { kind: OpenKind::Paragraph, spans: vec![], children: vec![] });
                }
            }
            Md::End(TagEnd::Paragraph) => {
                if matches!(stack.last().map(|o| &o.kind), Some(OpenKind::Paragraph)) {
                    let o = stack.pop().unwrap();
                    if !o.spans.is_empty() {
                        emit(&mut stack, &mut out, block("paragraph", json!({ "rich_text": o.spans })));
                    }
                }
            }
            Md::Start(Tag::Heading { level, .. }) => {
                let n = match level {
                    HeadingLevel::H1 => 1,
                    HeadingLevel::H2 => 2,
                    _ => 3,
                };
                stack.push(Open { kind: OpenKind::Heading(n), spans: vec![], children: vec![] });
            }
            Md::End(TagEnd::Heading(_)) => {
                if let Some(Open { kind: OpenKind::Heading(n), spans, .. }) = stack.pop() {
                    let ty = format!("heading_{n}");
                    emit(&mut stack, &mut out, block(&ty, json!({ "rich_text": spans })));
                }
            }
            Md::Start(Tag::BlockQuote(_)) => stack.push(Open { kind: OpenKind::Quote, spans: vec![], children: vec![] }),
            Md::End(TagEnd::BlockQuote(_)) => {
                if let Some(Open { kind: OpenKind::Quote, spans, .. }) = stack.pop() {
                    emit(&mut stack, &mut out, block("quote", json!({ "rich_text": spans })));
                }
            }
            Md::Start(Tag::CodeBlock(kind)) => {
                let lang = match kind {
                    CodeBlockKind::Fenced(l) => l.to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                stack.push(Open { kind: OpenKind::Code(lang), spans: vec![], children: vec![] });
            }
            Md::End(TagEnd::CodeBlock) => {
                if let Some(Open { kind: OpenKind::Code(lang), spans, .. }) = stack.pop() {
                    emit(&mut stack, &mut out, block("code", json!({ "rich_text": spans, "language": code_language(&lang) })));
                }
            }
            Md::Start(Tag::List(start)) => lists.push(start.is_some()),
            Md::End(TagEnd::List(_)) => {
                lists.pop();
            }
            Md::Start(Tag::Item) => {
                let ordered = lists.last().copied().unwrap_or(false);
                stack.push(Open { kind: OpenKind::Item { ordered, checked: None }, spans: vec![], children: vec![] });
            }
            Md::TaskListMarker(done) => {
                if let Some(Open { kind: OpenKind::Item { checked, .. }, .. }) = stack.last_mut() {
                    *checked = Some(done);
                }
            }
            Md::End(TagEnd::Item) => {
                if let Some(Open { kind: OpenKind::Item { ordered, checked }, spans, children }) = stack.pop() {
                    let mut content = json!({ "rich_text": spans });
                    if !children.is_empty() {
                        content["children"] = json!(children);
                    }
                    let b = match checked {
                        Some(done) => {
                            content["checked"] = json!(done);
                            block("to_do", content)
                        }
                        None if ordered => block("numbered_list_item", content),
                        None => block("bulleted_list_item", content),
                    };
                    emit(&mut stack, &mut out, b);
                }
            }
            Md::Rule => emit(&mut stack, &mut out, block("divider", json!({}))),
            Md::Start(Tag::Emphasis) => style.italic += 1,
            Md::End(TagEnd::Emphasis) => style.italic = style.italic.saturating_sub(1),
            Md::Start(Tag::Strong) => style.bold += 1,
            Md::End(TagEnd::Strong) => style.bold = style.bold.saturating_sub(1),
            Md::Start(Tag::Strikethrough) => style.strike += 1,
            Md::End(TagEnd::Strikethrough) => style.strike = style.strike.saturating_sub(1),
            Md::Start(Tag::Link { dest_url, .. }) => {
                let url = dest_url.to_string();
                style.link = (url.starts_with("http://") || url.starts_with("https://")).then_some(url);
            }
            Md::End(TagEnd::Link) => style.link = None,
            Md::Start(Tag::Image { .. } | Tag::Table(_)) => {
                skipped += 1;
                dropping = 1;
            }
            Md::Html(_) | Md::InlineHtml(_) => skipped += 1,
            Md::Text(t) => {
                let in_code = matches!(stack.last().map(|o| &o.kind), Some(OpenKind::Code(_)));
                let plain = Style::default();
                let spans = span(&t, if in_code { &plain } else { &style }, false);
                push_spans(&mut stack, &mut out, spans);
            }
            Md::Code(t) => {
                let spans = span(&t, &style, true);
                push_spans(&mut stack, &mut out, spans);
            }
            Md::SoftBreak => push_spans(&mut stack, &mut out, span(" ", &style, false)),
            Md::HardBreak => push_spans(&mut stack, &mut out, span("\n", &style, false)),
            _ => {}
        }
    }
    (out, skipped)
}

// ---- config ------------------------------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Target {
    pub database_id: String,
    pub database_title: String,
    /// Nora field id → Notion property name.
    pub mapping: BTreeMap<String, String>,
    /// Notion property name → type, as read when the mapping was made.
    pub types: BTreeMap<String, String>,
    /// Write the Markdown text as the page's content.
    pub body: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NotionConfig {
    pub targets: BTreeMap<Kind, Target>,
    /// Export changes automatically every few minutes.
    pub auto: bool,
    /// Workspace the token belongs to (shown in Settings).
    pub workspace: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Report {
    pub created: u32,
    pub updated: u32,
    pub unchanged: u32,
    pub archived: u32,
    /// Images, tables and raw HTML that could not be converted.
    pub skipped_elements: u32,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NotionRun {
    pub last_run: Option<i64>,
    pub last_error: Option<String>,
    pub last_report: Option<Report>,
}

#[derive(Clone, Debug, Serialize)]
pub struct KindFields {
    pub kind: Kind,
    pub fields: Vec<Field>,
}

#[derive(Clone, Debug, Serialize)]
pub struct NotionStatus {
    pub config: NotionConfig,
    #[serde(flatten)]
    pub run: NotionRun,
    pub connected: bool,
    pub fields: Vec<KindFields>,
}

fn read<T: Default + for<'de> Deserialize<'de>>(db: &Db, key: &str) -> T {
    db.setting(key).ok().flatten().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

fn write<T: Serialize>(db: &Db, key: &str, value: &T) -> CmdResult<()> {
    db.set_setting(key, &serde_json::to_string(value).map_err(|e| e.to_string())?).map_err(|e| e.to_string())
}

fn status(db: &Db) -> NotionStatus {
    NotionStatus {
        config: read(db, CONFIG_KEY),
        run: read(db, RUN_KEY),
        connected: token().is_some(),
        fields: Kind::ALL.iter().map(|k| KindFields { kind: *k, fields: fields(*k) }).collect(),
    }
}

fn entry() -> Option<keyring::Entry> {
    keyring::Entry::new(KEYRING_SERVICE, KEY_TOKEN).ok()
}

fn token() -> Option<String> {
    entry()?.get_password().ok().filter(|s| !s.is_empty())
}

/// The 32-hex-digit id inside a Notion database URL or id.
pub fn database_id(reference: &str) -> Option<String> {
    let path = reference.split(['?', '#']).next().unwrap_or("");
    let hex: String = path.chars().filter(|c| c.is_ascii_hexdigit() || *c == '-').collect();
    // The id is the last 32 hex digits (dashes optional).
    let digits: String = hex.chars().filter(|c| *c != '-').collect();
    (digits.len() >= 32).then(|| digits[digits.len() - 32..].to_ascii_lowercase())
}

// ---- local bookkeeping -------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
struct PageRow {
    database_id: String,
    page_id: String,
    props_hash: String,
    body_hash: String,
}

impl Db {
    fn notion_page(&self, kind: Kind, item: i64) -> DbResult<Option<PageRow>> {
        self.conn
            .query_row(
                "SELECT database_id, page_id, props_hash, body_hash FROM notion_pages WHERE kind = ?1 AND item_id = ?2",
                params![kind.key(), item],
                |r| Ok(PageRow { database_id: r.get(0)?, page_id: r.get(1)?, props_hash: r.get(2)?, body_hash: r.get(3)? }),
            )
            .optional()
    }

    fn notion_save_page(&self, kind: Kind, item: i64, row: &PageRow) -> DbResult<()> {
        self.conn.execute(
            "INSERT INTO notion_pages(kind, item_id, database_id, page_id, props_hash, body_hash) VALUES(?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(kind, item_id) DO UPDATE SET database_id = excluded.database_id, page_id = excluded.page_id,
             props_hash = excluded.props_hash, body_hash = excluded.body_hash",
            params![kind.key(), item, row.database_id, row.page_id, row.props_hash, row.body_hash],
        )?;
        Ok(())
    }

    fn notion_forget_page(&self, kind: Kind, item: i64) -> DbResult<()> {
        self.conn.execute("DELETE FROM notion_pages WHERE kind = ?1 AND item_id = ?2", params![kind.key(), item])?;
        Ok(())
    }

    fn notion_item_ids(&self, kind: Kind) -> DbResult<Vec<(i64, String)>> {
        self.conn
            .prepare("SELECT item_id, page_id FROM notion_pages WHERE kind = ?1")?
            .query_map([kind.key()], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect()
    }
}

fn hash(v: &impl Serialize) -> String {
    let bytes = serde_json::to_vec(v).unwrap_or_default();
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// The page properties for `item` under `target`. The database's title property is
/// always filled (with the item's title if nothing is mapped to it).
pub fn page_properties(item: &Item, target: &Target) -> Value {
    let mut props = serde_json::Map::new();
    for (field, prop) in &target.mapping {
        let (Some(ty), Some(v)) = (target.types.get(prop), item.values.get(field.as_str())) else { continue };
        if let Some(value) = prop_value(v, ty) {
            props.insert(prop.clone(), value);
        }
    }
    if let Some((title_prop, _)) = target.types.iter().find(|(_, t)| *t == "title")
        && !props.contains_key(title_prop)
        && let Some(v) = item.values.get("title")
    {
        props.insert(title_prop.clone(), prop_value(v, "title").unwrap_or(Value::Null));
    }
    Value::Object(props)
}

// ---- network -----------------------------------------------------------------------------

fn send(mut call: impl FnMut() -> Result<ureq::http::Response<ureq::Body>, ureq::Error>) -> CmdResult<(u16, Value)> {
    let mut wait = Duration::from_secs(1);
    for attempt in 0..5 {
        let mut resp = call().map_err(|e| format!("Could not reach Notion: {e}"))?;
        let status = resp.status().as_u16();
        let retry_after = resp.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
        let body = resp.body_mut().read_to_string().unwrap_or_default();
        if (status == 429 || status >= 500) && attempt < 4 {
            std::thread::sleep(retry_after.map(Duration::from_secs).unwrap_or(wait));
            wait *= 2;
            continue;
        }
        // Notion allows about three requests a second.
        std::thread::sleep(Duration::from_millis(340));
        let json = serde_json::from_str(&body).unwrap_or(Value::Null);
        return Ok((status, json));
    }
    unreachable!()
}

fn notion_error(status: u16, json: &Value) -> String {
    let msg = json["message"].as_str().unwrap_or("");
    match status {
        401 => "Notion rejected the token. Check it or connect again.".into(),
        404 => format!("Notion can't find it. Add the integration to the database's connections. ({msg})"),
        429 => "Notion's request limit was reached. Try again later.".into(),
        _ => format!("Notion returned {status}: {msg}"),
    }
}

struct Client {
    auth: String,
}

impl Client {
    fn new(token: &str) -> Self {
        Self { auth: format!("Bearer {token}") }
    }

    fn get(&self, path: &str) -> CmdResult<(u16, Value)> {
        let url = format!("{API}{path}");
        send(|| crate::gcal::agent().get(&url).header("Authorization", &self.auth).header("Notion-Version", VERSION).call())
    }

    fn with_body(&self, method: &str, path: &str, body: &Value) -> CmdResult<(u16, Value)> {
        let url = format!("{API}{path}");
        let text = body.to_string();
        send(|| {
            let agent = crate::gcal::agent();
            let req = match method {
                "POST" => agent.post(&url),
                _ => agent.patch(&url),
            };
            req.header("Authorization", &self.auth)
                .header("Notion-Version", VERSION)
                .header("Content-Type", "application/json")
                .send(text.as_str())
        })
    }

    fn delete(&self, path: &str) -> CmdResult<(u16, Value)> {
        let url = format!("{API}{path}");
        send(|| crate::gcal::agent().delete(&url).header("Authorization", &self.auth).header("Notion-Version", VERSION).call())
    }

    fn ok(&self, (status, json): (u16, Value)) -> CmdResult<Value> {
        if (200..300).contains(&status) { Ok(json) } else { Err(notion_error(status, &json)) }
    }

    fn append(&self, page: &str, blocks: &[Value]) -> CmdResult<()> {
        for batch in blocks.chunks(BLOCK_BATCH) {
            self.ok(self.with_body("PATCH", &format!("/blocks/{page}/children"), &json!({ "children": batch }))?)?;
        }
        Ok(())
    }

    /// Removes a page's content so it can be written again.
    fn clear(&self, page: &str) -> CmdResult<()> {
        let mut ids = vec![];
        let mut cursor: Option<String> = None;
        loop {
            let q = cursor.as_ref().map(|c| format!("&start_cursor={c}")).unwrap_or_default();
            let json = self.ok(self.get(&format!("/blocks/{page}/children?page_size=100{q}"))?)?;
            ids.extend(json["results"].as_array().into_iter().flatten().filter_map(|b| b["id"].as_str().map(str::to_owned)));
            match json["next_cursor"].as_str() {
                Some(c) if json["has_more"].as_bool() == Some(true) => cursor = Some(c.to_owned()),
                _ => break,
            }
        }
        for id in ids {
            let (status, json) = self.delete(&format!("/blocks/{id}"))?;
            if !(200..300).contains(&status) && status != 404 {
                return Err(notion_error(status, &json));
            }
        }
        Ok(())
    }
}

/// Reads a database's title and properties.
fn fetch_database(client: &Client, id: &str) -> CmdResult<(String, BTreeMap<String, String>)> {
    let json = client.ok(client.get(&format!("/databases/{id}"))?)?;
    let title = json["title"].as_array().into_iter().flatten().filter_map(|t| t["plain_text"].as_str()).collect::<String>();
    let types = json["properties"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(name, p)| p["type"].as_str().map(|t| (name.clone(), t.to_owned())))
        .collect();
    Ok((title, types))
}

/// Exports one kind. Unchanged items are skipped without any request.
fn export_kind(client: &Client, db: &Db, kind: Kind, target: &Target, locale: &str, report: &mut Report) -> CmdResult<()> {
    let items = items(db, kind, locale).map_err(|e| e.to_string())?;
    let db_id = target.database_id.as_str();
    for item in &items {
        let props = page_properties(item, target);
        let (blocks, skipped) = if target.body && !item.body.trim().is_empty() { markdown_to_blocks(&item.body) } else { (vec![], 0) };
        let props_hash = hash(&(db_id, &props));
        let body_hash = hash(&(target.body, &blocks));
        let existing = db.notion_page(kind, item.id).map_err(|e| e.to_string())?.filter(|r| r.database_id == db_id);

        if let Some(row) = &existing {
            if row.props_hash == props_hash && row.body_hash == body_hash {
                report.unchanged += 1;
                continue;
            }
            let (status, json) = client.with_body("PATCH", &format!("/pages/{}", row.page_id), &json!({ "properties": props }))?;
            let gone = status == 404 || json["archived"].as_bool() == Some(true) || json["in_trash"].as_bool() == Some(true);
            if (200..300).contains(&status) && !gone {
                if row.body_hash != body_hash {
                    client.clear(&row.page_id)?;
                    client.append(&row.page_id, &blocks)?;
                }
                db.notion_save_page(kind, item.id, &PageRow { props_hash, body_hash, ..row.clone() }).map_err(|e| e.to_string())?;
                report.updated += 1;
                report.skipped_elements += skipped;
                continue;
            }
            if !gone {
                return Err(notion_error(status, &json));
            }
            // The page was deleted in Notion: make a new one.
        }

        let first: Vec<Value> = blocks.iter().take(BLOCK_BATCH).cloned().collect();
        let json = client.ok(client.with_body(
            "POST",
            "/pages",
            &json!({ "parent": { "database_id": db_id }, "properties": props, "children": first }),
        )?)?;
        let page_id = json["id"].as_str().ok_or("Notion sent no page id")?.to_owned();
        if blocks.len() > BLOCK_BATCH {
            client.append(&page_id, &blocks[BLOCK_BATCH..])?;
        }
        db.notion_save_page(kind, item.id, &PageRow { database_id: db_id.to_owned(), page_id, props_hash, body_hash })
            .map_err(|e| e.to_string())?;
        report.created += 1;
        report.skipped_elements += skipped;
    }

    // Items deleted in Nora: archive their pages.
    let alive: std::collections::HashSet<i64> = items.iter().map(|i| i.id).collect();
    for (item_id, page_id) in db.notion_item_ids(kind).map_err(|e| e.to_string())? {
        if alive.contains(&item_id) {
            continue;
        }
        let (status, json) = client.with_body("PATCH", &format!("/pages/{page_id}"), &json!({ "archived": true }))?;
        if !(200..300).contains(&status) && status != 404 {
            return Err(notion_error(status, &json));
        }
        db.notion_forget_page(kind, item_id).map_err(|e| e.to_string())?;
        report.archived += 1;
    }
    Ok(())
}

/// One export at a time (manual and automatic).
static RUNNING: Mutex<()> = Mutex::new(());

/// Exports `only` (or every configured kind) and records the outcome.
fn run_export(data_dir: &Path, only: Option<Kind>, refresh_schema: bool) -> CmdResult<NotionStatus> {
    let _guard = RUNNING.lock().map_err(|_| "export lock poisoned".to_string())?;
    let db = Db { conn: db::open_conn(&data_dir.join(backup::DB_FILE)).map_err(|e| e.to_string())? };
    let mut config: NotionConfig = read(&db, CONFIG_KEY);
    let locale = db.setting("ui.locale").ok().flatten().unwrap_or_else(|| "en".into());
    let mut report = Report::default();

    let result = (|| -> CmdResult<()> {
        let token = token().ok_or("Not connected to Notion.")?;
        let client = Client::new(&token);
        for kind in Kind::ALL {
            if only.is_some_and(|k| k != kind) {
                continue;
            }
            let Some(target) = config.targets.get_mut(&kind) else { continue };
            if refresh_schema {
                // Pick up renamed or retyped properties before writing.
                let (title, types) = fetch_database(&client, &target.database_id)?;
                target.database_title = title;
                target.types = types;
            }
            export_kind(&client, &db, kind, target, &locale, &mut report)?;
        }
        Ok(())
    })();

    if refresh_schema {
        write(&db, CONFIG_KEY, &config)?;
    }
    let mut run: NotionRun = read(&db, RUN_KEY);
    run.last_run = Some(now_ts());
    run.last_report = Some(report);
    run.last_error = result.err();
    write(&db, RUN_KEY, &run)?;
    Ok(status(&db))
}

pub fn start(data_dir: PathBuf) {
    let spawned = std::thread::Builder::new().name("notion".into()).spawn(move || {
        loop {
            std::thread::sleep(AUTO_EVERY);
            let auto = db::open_conn(&data_dir.join(backup::DB_FILE))
                .map(|conn| {
                    let config: NotionConfig = read(&Db { conn }, CONFIG_KEY);
                    config.auto && !config.targets.is_empty()
                })
                .unwrap_or(false);
            if auto && token().is_some() && let Err(e) = run_export(&data_dir, None, false) {
                eprintln!("notion export failed: {e}");
            }
        }
    });
    if let Err(e) = spawned {
        eprintln!("failed to start notion export: {e}");
    }
}

// ---- commands ------------------------------------------------------------------------------

fn with_db<T>(app: &AppHandle, f: impl FnOnce(&Db) -> T) -> CmdResult<T> {
    let state = app.state::<AppState>();
    let db = state.db.lock().map_err(|_| "database lock poisoned".to_string())?;
    Ok(f(&db))
}

fn data_dir(app: &AppHandle) -> PathBuf {
    app.state::<AppState>().data_dir.clone()
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> CmdResult<T> + Send + 'static) -> CmdResult<T> {
    tauri::async_runtime::spawn_blocking(f).await.map_err(|e| e.to_string())?
}

#[tauri::command]
pub fn notion_status(app: AppHandle) -> CmdResult<NotionStatus> {
    with_db(&app, status)
}

/// Checks the integration token with Notion and stores it in the credential store.
#[tauri::command]
pub async fn notion_connect(app: AppHandle, token: String) -> CmdResult<NotionStatus> {
    let token = token.trim().to_owned();
    if token.is_empty() {
        return Err("Paste the integration's secret.".into());
    }
    blocking(move || {
        let client = Client::new(&token);
        let me = client.ok(client.get("/users/me")?)?;
        entry()
            .ok_or("credential store unavailable")?
            .set_password(&token)
            .map_err(|e| format!("could not save to the credential store: {e}"))?;
        with_db(&app, |db| {
            let mut config: NotionConfig = read(db, CONFIG_KEY);
            config.workspace = me["bot"]["workspace_name"].as_str().map(str::to_owned);
            write(db, CONFIG_KEY, &config)?;
            Ok(status(db))
        })?
    })
    .await
}

/// Forgets the token. Settings and the page list stay, so reconnecting updates the same pages.
#[tauri::command]
pub fn notion_disconnect(app: AppHandle) -> CmdResult<NotionStatus> {
    if let Some(e) = entry() {
        let _ = e.delete_credential();
    }
    with_db(&app, status)
}

#[derive(Clone, Debug, Serialize)]
pub struct DatabaseInfo {
    pub id: String,
    pub title: String,
    pub properties: BTreeMap<String, String>,
}

/// Reads a database (by URL or id) so its properties can be mapped.
#[tauri::command]
pub async fn notion_database(reference: String) -> CmdResult<DatabaseInfo> {
    let id = database_id(&reference).ok_or("That doesn't look like a Notion database link.")?;
    blocking(move || {
        let token = token().ok_or("Not connected to Notion.")?;
        let (title, properties) = fetch_database(&Client::new(&token), &id)?;
        Ok(DatabaseInfo { id, title, properties })
    })
    .await
}

/// Saves (or with `None`, removes) where `kind` is exported to.
#[tauri::command]
pub fn notion_set_target(app: AppHandle, kind: Kind, target: Option<Target>) -> CmdResult<NotionStatus> {
    with_db(&app, |db| {
        let mut config: NotionConfig = read(db, CONFIG_KEY);
        match target {
            Some(mut t) => {
                let known = fields(kind);
                // Keep only mappings to existing properties of a fitting type.
                t.mapping.retain(|field, prop| {
                    let value = known.iter().find(|f| f.id == field).map(|f| f.value);
                    matches!((value, t.types.get(prop)), (Some(v), Some(ty)) if v.accepts(ty))
                });
                config.targets.insert(kind, t);
            }
            None => {
                config.targets.remove(&kind);
            }
        }
        write(db, CONFIG_KEY, &config)?;
        Ok(status(db))
    })?
}

#[tauri::command]
pub fn notion_set_auto(app: AppHandle, auto: bool) -> CmdResult<NotionStatus> {
    with_db(&app, |db| {
        let mut config: NotionConfig = read(db, CONFIG_KEY);
        config.auto = auto;
        write(db, CONFIG_KEY, &config)?;
        Ok(status(db))
    })?
}

/// Exports now (one kind, or all configured ones). Failures land in `last_error`.
#[tauri::command]
pub async fn notion_export(app: AppHandle, kind: Option<Kind>) -> CmdResult<NotionStatus> {
    let dir = data_dir(&app);
    blocking(move || run_export(&dir, kind, true)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(b: &Value) -> String {
        let ty = b["type"].as_str().unwrap();
        b[ty]["rich_text"].as_array().unwrap().iter().map(|t| t["text"]["content"].as_str().unwrap()).collect()
    }

    #[test]
    fn markdown_becomes_blocks() {
        let md = "# Title\n\nSome **bold** and `code` with [a link](https://example.com).\n\n- one\n- two\n  - nested\n\n1. first\n\n- [x] done\n- [ ] todo\n\n> quoted\n\n```rust\nfn main() {}\n```\n\n---\n";
        let (blocks, skipped) = markdown_to_blocks(md);
        let types: Vec<&str> = blocks.iter().map(|b| b["type"].as_str().unwrap()).collect();
        assert_eq!(
            types,
            ["heading_1", "paragraph", "bulleted_list_item", "bulleted_list_item", "numbered_list_item", "to_do", "to_do", "quote", "code", "divider"]
        );
        assert_eq!(skipped, 0);
        assert_eq!(texts(&blocks[1]), "Some bold and code with a link.");
        let spans = blocks[1]["paragraph"]["rich_text"].as_array().unwrap();
        assert!(spans.iter().any(|s| s["annotations"]["bold"] == true && s["text"]["content"] == "bold"));
        assert!(spans.iter().any(|s| s["annotations"]["code"] == true));
        assert!(spans.iter().any(|s| s["text"]["link"]["url"] == "https://example.com"));
        // "nested" hangs under "two".
        assert_eq!(texts(&blocks[3]["bulleted_list_item"]["children"][0]), "nested");
        assert_eq!(blocks[5]["to_do"]["checked"], true);
        assert_eq!(blocks[8]["code"]["language"], "rust");
        assert_eq!(texts(&blocks[8]), "fn main() {}\n");
    }

    #[test]
    fn images_tables_and_html_are_counted_not_sent() {
        let md = "Before\n\n![cat](cat.png)\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n<div>x</div>\n\nAfter";
        let (blocks, skipped) = markdown_to_blocks(md);
        assert_eq!(skipped, 3);
        let all: Vec<String> = blocks.iter().map(texts).collect();
        assert_eq!(all, ["Before", "After"]);
    }

    #[test]
    fn long_text_is_split_into_pieces_notion_accepts() {
        let long = "あ".repeat(4500);
        let (blocks, _) = markdown_to_blocks(&long);
        let spans = blocks[0]["paragraph"]["rich_text"].as_array().unwrap();
        assert_eq!(spans.len(), 3);
        assert!(spans.iter().all(|s| s["text"]["content"].as_str().unwrap().chars().count() <= TEXT_LIMIT));
    }

    #[test]
    fn values_fit_the_property_types() {
        assert_eq!(prop_value(&Val::Text("Fantasy, Sci-Fi".into()), "select").unwrap(), json!({ "select": { "name": "Fantasy  Sci-Fi" } }));
        assert_eq!(prop_value(&Val::Text(String::new()), "select").unwrap(), json!({ "select": null }));
        assert_eq!(
            prop_value(&Val::List(vec!["GM".into(), "HO1".into()]), "multi_select").unwrap(),
            json!({ "multi_select": [{ "name": "GM" }, { "name": "HO1" }] })
        );
        assert_eq!(prop_value(&Val::List(vec!["GM".into(), "HO1".into()]), "rich_text").unwrap()["rich_text"][0]["text"]["content"], "GM, HO1");
        assert_eq!(prop_value(&Val::Number(Some(3.5)), "number").unwrap(), json!({ "number": 3.5 }));
        assert_eq!(prop_value(&Val::Number(Some(4.0)), "rich_text").unwrap()["rich_text"][0]["text"]["content"], "4");
        assert_eq!(prop_value(&Val::Date(NaiveDate::from_ymd_opt(2026, 10, 7)), "date").unwrap(), json!({ "date": { "start": "2026-10-07" } }));
        assert_eq!(prop_value(&Val::Url(None), "url").unwrap(), json!({ "url": null }));
        assert!(prop_value(&Val::Text("x".into()), "number").is_none());
        assert!(prop_value(&Val::Text("x".into()), "checkbox").is_none());
        assert!(ValueKind::Date.accepts("date") && !ValueKind::Date.accepts("number"));
    }

    #[test]
    fn title_is_always_filled_and_unknown_properties_ignored() {
        let item = Item {
            id: 1,
            values: HashMap::from([("title", Val::Text("Dune".into())), ("author", Val::Text("Herbert".into()))]),
            body: String::new(),
        };
        let target = Target {
            mapping: BTreeMap::from([("author".into(), "Author".into()), ("missing".into(), "Nope".into())]),
            types: BTreeMap::from([("Name".into(), "title".into()), ("Author".into(), "rich_text".into())]),
            ..Default::default()
        };
        let props = page_properties(&item, &target);
        assert_eq!(props["Name"]["title"][0]["text"]["content"], "Dune");
        assert_eq!(props["Author"]["rich_text"][0]["text"]["content"], "Herbert");
        assert!(props.get("Nope").is_none());
    }

    #[test]
    fn database_ids_come_from_links_or_ids() {
        let id = "0123456789abcdef0123456789abcdef";
        assert_eq!(database_id(&format!("https://www.notion.so/me/Reading-{id}?v=fedcba9876543210fedcba9876543210")).as_deref(), Some(id));
        assert_eq!(database_id("01234567-89ab-cdef-0123-456789abcdef").as_deref(), Some(id));
        assert_eq!(database_id("https://www.notion.so/me/Short"), None);
    }

    #[test]
    fn items_read_books_and_trpg_in_the_users_language() {
        let path = std::env::temp_dir().join(format!("nora-notion-{}.sqlite", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let db = Db::open(&path).unwrap();
        db.conn
            .execute(
                "INSERT INTO books(title, author, publisher, status, total_pages, current_page, rating, review, created_at)
                 VALUES('Dune', 'Herbert', '', 'reading', 400, 100, 7, '**Good**', 0)",
                [],
            )
            .unwrap();
        let books = items(&db, Kind::Book, "ja").unwrap();
        assert_eq!(books[0].values["status"], Val::Text("読んでいる本".into()));
        assert_eq!(books[0].values["progress"], Val::Number(Some(25.0)));
        assert_eq!(books[0].values["rating"], Val::Number(Some(3.5)));
        assert_eq!(books[0].body, "**Good**");
        drop(db);
        let _ = std::fs::remove_file(path);
    }
}
