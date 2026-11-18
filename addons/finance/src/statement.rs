//! Reading a bank statement's text into lines, as a `StatementFormat` record describes it
//! (docs/statement-formats.md). Three readers know no standard: Delimited (rows of columns),
//! Markup (elements: XML, or SGML whose leaf tags are not closed) and Tagged (lines that start with
//! `:TAG:`). What a standard looks like is a record's `reading`. Pure: no database.
//!
//! The text is outside input: the caller caps its size; here the lines are capped, an element
//! deeper than 64 levels or a file that is not well formed is one error, no DTD or entity is read,
//! patterns are linear-time and size-limited, and every value kept is cut to the field it goes to.

use crate::rules;
use async_trait::async_trait;
use chrono::NaiveDate;
use ferrum_kernel::{Doc, DocHook, Error, HookCtx, Result};
use quick_xml::Reader;
use quick_xml::events::Event as Xml;
use regex::{Regex, RegexBuilder};
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

/// Lines in one statement file, at most.
pub const MAX_LINES: usize = 10_000;
/// How deep a Markup file's elements may nest.
const MAX_DEPTH: usize = 64;
/// How long a Tagged format's line pattern may be.
const MAX_PATTERN: usize = 500;
/// How much of the text a format's `sniff` looks at.
const SNIFF_CHARS: usize = 4096;
/// A description is cut to this many characters, a reference or the bank's reference to this many.
const MAX_DESCRIPTION: usize = 1000;
const MAX_REFERENCE: usize = 140;

/// One statement line as read.
#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub date: NaiveDate,
    pub deposit: Decimal,
    pub withdrawal: Decimal,
    pub description: String,
    pub reference: String,
    pub bank_reference: String,
}

impl Line {
    fn new(date: NaiveDate, amount: Decimal, description: &str, reference: Option<String>, bank_reference: Option<String>) -> Line {
        let cut = |s: &str, n: usize| s.trim().chars().take(n).collect::<String>().trim().to_string();
        Line {
            date,
            deposit: amount.max(Decimal::ZERO),
            withdrawal: (-amount).max(Decimal::ZERO),
            description: cut(description, MAX_DESCRIPTION),
            reference: cut(reference.as_deref().unwrap_or_default(), MAX_REFERENCE),
            bank_reference: cut(bank_reference.as_deref().unwrap_or_default(), MAX_REFERENCE),
        }
    }
}

/// What a file holds: each statement's account (when the file names one) and lines; the lines
/// that cannot be read; how many were left out (no amount, or not booked).
#[derive(Debug, Default)]
pub struct Read {
    pub statements: Vec<(Option<String>, Vec<Line>)>,
    pub errors: Vec<String>,
    pub skipped: usize,
}

impl Read {
    fn error(msg: impl Into<String>) -> Read {
        Read { errors: vec![msg.into()], ..Read::default() }
    }
}

// ───────────────────────────── the reading ─────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Delimited {
    delimiter: Option<String>,
    header_row: Option<usize>,
    columns: Map<String, Value>,
    date_format: Option<String>,
    #[serde(default)]
    decimal_comma: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sides {
    #[serde(rename = "in", default)]
    into: Vec<String>,
    #[serde(default)]
    out: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignAt {
    at: Vec<String>,
    #[serde(rename = "in", default)]
    into: Vec<String>,
    #[serde(default)]
    out: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Only {
    at: Vec<String>,
    is: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Markup {
    #[serde(default)]
    sniff: Vec<String>,
    #[serde(default)]
    loose: bool,
    #[serde(default)]
    statement: Vec<String>,
    #[serde(default)]
    account: Vec<String>,
    entry: String,
    date: Vec<String>,
    amount: Vec<String>,
    #[serde(default)]
    description: Vec<String>,
    #[serde(default)]
    reference: Vec<String>,
    #[serde(default)]
    bank_reference: Vec<String>,
    counterparty: Option<Sides>,
    sign: Option<SignAt>,
    only: Option<Only>,
    #[serde(default)]
    empty: Vec<String>,
    date_format: Option<String>,
    date_chars: Option<usize>,
    #[serde(default)]
    decimal_comma: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tag {
    tag: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LineTag {
    tag: String,
    pattern: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Detail {
    tag: String,
    strip: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tagged {
    #[serde(default)]
    sniff: Vec<String>,
    account: Option<Tag>,
    line: LineTag,
    detail: Option<Detail>,
    sign: Option<Sides>,
    #[serde(default)]
    empty: Vec<String>,
    date_format: Option<String>,
    date_chars: Option<usize>,
    #[serde(default)]
    decimal_comma: bool,
    #[serde(skip)]
    patterns: Option<(Regex, Option<Regex>)>,
}

/// A format's `reading`, checked.
#[derive(Debug)]
pub enum Reading {
    Delimited(Delimited),
    Markup(Markup),
    Tagged(Tagged),
}

const COLUMNS: [&str; 7] = ["date", "amount", "deposit", "withdrawal", "description", "reference", "bank_reference"];

fn pattern(p: &str) -> std::result::Result<Regex, String> {
    if p.chars().count() > MAX_PATTERN {
        return Err(format!("a pattern is longer than {MAX_PATTERN} characters"));
    }
    RegexBuilder::new(p).size_limit(1 << 20).build().map_err(|e| format!("the pattern does not compile: {e}"))
}

/// The `reading` of a format of `kind`, or what is wrong with it. An unknown key is refused.
pub fn reading(kind: &str, v: &Value) -> std::result::Result<Reading, String> {
    let parsed;
    let v = match v {
        Value::String(s) => {
            parsed = serde_json::from_str::<Value>(s).map_err(|e| format!("is not JSON: {e}"))?;
            &parsed
        }
        v => v,
    };
    let bad = |e: serde_json::Error| e.to_string();
    match kind {
        "Delimited" => {
            let r: Delimited = serde_json::from_value(v.clone()).map_err(bad)?;
            if let Some(k) = r.columns.keys().find(|k| !COLUMNS.contains(&k.as_str())) {
                return Err(format!("columns: '{k}' is none of {}", COLUMNS.join(", ")));
            }
            if r.columns.get("date").is_none_or(Value::is_null) {
                return Err("columns: name the date column".into());
            }
            if ["amount", "deposit", "withdrawal"].iter().all(|k| r.columns.get(*k).is_none_or(Value::is_null)) {
                return Err("columns: name an amount column, or deposit and withdrawal columns".into());
            }
            if r.delimiter.as_deref().is_some_and(|d| d.chars().count() != 1) {
                return Err("delimiter: one character".into());
            }
            Ok(Reading::Delimited(r))
        }
        "Markup" => {
            let r: Markup = serde_json::from_value(v.clone()).map_err(bad)?;
            if r.entry.trim().is_empty() || r.date.is_empty() || r.amount.is_empty() {
                return Err("name the entry element and where its date and amount are".into());
            }
            Ok(Reading::Markup(r))
        }
        "Tagged" => {
            let mut r: Tagged = serde_json::from_value(v.clone()).map_err(bad)?;
            let line = pattern(&r.line.pattern)?;
            for g in ["date", "sign", "amount"] {
                if !line.capture_names().flatten().any(|n| n == g) {
                    return Err(format!("line.pattern: name a group '{g}'"));
                }
            }
            let strip = match r.detail.as_ref().and_then(|d| d.strip.as_deref()) {
                Some(s) => Some(pattern(s)?),
                None => None,
            };
            r.patterns = Some((line, strip));
            Ok(Reading::Tagged(r))
        }
        other => Err(format!("'{other}' is not a kind of format (Delimited, Markup or Tagged)")),
    }
}

impl Reading {
    /// Whether this format fits `text`: one of its `sniff` strings is in the first characters.
    pub fn fits(&self, text: &str) -> bool {
        let sniff = match self {
            Reading::Delimited(_) => return false,
            Reading::Markup(r) => &r.sniff,
            Reading::Tagged(r) => &r.sniff,
        };
        let head: String = text.chars().take(SNIFF_CHARS).collect();
        sniff.iter().any(|s| !s.is_empty() && head.contains(s.as_str()))
    }

    /// The statement's lines. `Err` when it has more than `MAX_LINES`.
    pub fn read(&self, text: &str) -> std::result::Result<Read, String> {
        match self {
            Reading::Delimited(r) => read_delimited(text, r),
            Reading::Markup(r) => read_markup(text, r),
            Reading::Tagged(r) => read_tagged(text, r),
        }
    }
}

fn too_many() -> String {
    format!("the statement has more than {MAX_LINES} lines; split it")
}

/// A date as read, cut to `chars` characters and parsed with `format`.
fn date(raw: &str, chars: Option<usize>, format: Option<&str>) -> std::result::Result<NaiveDate, String> {
    let cut: String = match chars {
        Some(n) => raw.trim().chars().take(n).collect(),
        None => raw.trim().to_string(),
    };
    rules::parse_statement_date(&cut, format)
        .ok_or_else(|| format!("'{raw}' is not a date{}", format.map(|f| format!(" in the format {f}")).unwrap_or_default()))
}

fn squeeze(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ───────────────────────────── delimited ─────────────────────────────

/// The 0-based column a `columns` entry names: a header (any case) or a 1-based column number.
fn column(header: &[String], key: &str, columns: &Map<String, Value>) -> std::result::Result<Option<usize>, String> {
    let Some(v) = columns.get(key).filter(|v| !v.is_null()) else { return Ok(None) };
    let by_number = match v {
        Value::Number(n) => n.as_u64().map(|i| i as usize),
        Value::String(t) => t.trim().parse::<usize>().ok(),
        _ => None,
    };
    if let Some(i) = by_number {
        return if (1..=header.len()).contains(&i) {
            Ok(Some(i - 1))
        } else {
            Err(format!("{key}: column {i} does not exist (the statement has {})", header.len()))
        };
    }
    let name = v.as_str().unwrap_or_default().trim();
    header.iter().position(|h| h.trim().eq_ignore_ascii_case(name)).map(Some).ok_or_else(|| format!("the statement has no column '{name}' (mapped to {key})"))
}

/// Amount columns are signed (positive is a deposit) unless deposits and withdrawals have columns of their own.
fn read_delimited(text: &str, r: &Delimited) -> std::result::Result<Read, String> {
    let delimiter = r.delimiter.as_deref().and_then(|d| d.chars().next()).unwrap_or_else(|| rules::detect_delimiter(text));
    let header_row = r.header_row.unwrap_or(1).max(1);
    let records = rules::parse_csv(text, delimiter);
    if records.len() < header_row {
        return Ok(Read::error("the statement has no header row"));
    }
    let header = &records[header_row - 1];
    let data = &records[header_row..];
    if data.len() > MAX_LINES {
        return Err(too_many());
    }
    let col = |key| column(header, key, &r.columns);
    let cols = (|| {
        Ok::<_, String>((col("date")?, col("amount")?, col("deposit")?, col("withdrawal")?, col("description")?, col("reference")?, col("bank_reference")?))
    })();
    let (col_date, col_amount, col_deposit, col_withdrawal, col_description, col_reference, col_bank) = match cols {
        Ok(c) => c,
        Err(m) => return Ok(Read::error(m)),
    };
    let date_format = r.date_format.as_deref();
    let cell = |row: &Vec<String>, c: Option<usize>| c.and_then(|i| row.get(i)).map(|v| v.trim().to_string()).unwrap_or_default();
    let mut out = Read::default();
    let mut lines = Vec::new();
    for (i, row) in data.iter().enumerate() {
        let line_no = header_row + i + 1;
        let amount = |c: Option<usize>| rules::parse_amount(&cell(row, c), r.decimal_comma);
        let parsed = (|| -> std::result::Result<Option<Decimal>, String> {
            let (dep, wd) = match col_amount {
                Some(_) => {
                    let a = amount(col_amount)?.unwrap_or_default();
                    (a.max(Decimal::ZERO), (-a).max(Decimal::ZERO))
                }
                None => (amount(col_deposit)?.unwrap_or_default().abs(), amount(col_withdrawal)?.unwrap_or_default().abs()),
            };
            if dep.is_zero() && wd.is_zero() {
                return Ok(None);
            }
            if !dep.is_zero() && !wd.is_zero() {
                return Err("has both a deposit and a withdrawal".into());
            }
            Ok(Some(dep - wd))
        })();
        match parsed {
            Err(m) => out.errors.push(format!("line {line_no}: {m}")),
            Ok(None) => out.skipped += 1,
            Ok(Some(amount)) => match date(&cell(row, col_date), None, date_format) {
                Err(m) => out.errors.push(format!("line {line_no}: {m}")),
                Ok(d) => lines.push(Line::new(d, amount, &cell(row, col_description), Some(cell(row, col_reference)), Some(cell(row, col_bank)))),
            },
        }
    }
    out.statements.push((None, lines));
    Ok(out)
}

// ───────────────────────────── markup ─────────────────────────────

/// SGML to XML: each leaf tag whose text is not followed by its end tag is closed after the text;
/// what precedes the first `<` is dropped; a bare `&` becomes `&amp;`.
fn close_leaves(text: &str) -> String {
    let Some(start) = text.find('<') else { return String::new() };
    let parts: Vec<&str> = text[start..].split('<').skip(1).collect();
    let mut out = String::with_capacity(text.len() + text.len() / 4);
    for (i, part) in parts.iter().enumerate() {
        out.push('<');
        let Some((tag, rest)) = part.split_once('>') else {
            out.push_str(part);
            continue;
        };
        out.push_str(tag);
        out.push('>');
        for (j, c) in rest.char_indices() {
            let after = &rest[j + 1..];
            let reference = after.find(';').is_some_and(|k| k > 0 && k <= 10 && after[..k].chars().all(|c| c.is_ascii_alphanumeric() || c == '#'));
            if c == '&' && !reference {
                out.push_str("&amp;");
            } else {
                out.push(c);
            }
        }
        let name = tag.split_whitespace().next().unwrap_or_default();
        let leaf = !name.is_empty() && !tag.starts_with(['/', '?', '!']) && !tag.ends_with('/') && !rest.trim().is_empty();
        let closed = parts.get(i + 1).and_then(|n| n.strip_prefix('/')).and_then(|n| n.strip_prefix(name)).is_some_and(|n| n.trim_start().starts_with('>'));
        if leaf && !closed {
            out.push_str("</");
            out.push_str(name);
            out.push('>');
        }
    }
    out
}

fn read_markup(text: &str, r: &Markup) -> std::result::Result<Read, String> {
    let closed;
    let text = if r.loose {
        closed = close_leaves(text);
        closed.as_str()
    } else {
        text
    };
    let mut reader = Reader::from_str(text);
    let mut out = Read::default();
    let mut path: Vec<String> = Vec::new();
    let mut buf = String::new();
    // The open statement: where it starts in `path`, its values by path, its lines.
    let mut statement: Option<(usize, HashMap<String, String>, Vec<Line>)> = None;
    // The open entry: where it starts in `path` and its values by path.
    let mut entry: Option<(usize, HashMap<String, Vec<String>>)> = None;
    let mut entries = 0usize;
    loop {
        let event = match reader.read_event() {
            Ok(e) => e,
            Err(e) => return Ok(Read::error(format!("the file is not well formed: {e}"))),
        };
        match event {
            Xml::Start(e) => {
                if path.len() >= MAX_DEPTH {
                    return Ok(Read::error(format!("the file nests elements deeper than {MAX_DEPTH} levels")));
                }
                let name = e.local_name().as_ref().to_string();
                let opens_statement = statement.is_none() && if r.statement.is_empty() { path.is_empty() } else { r.statement.contains(&name) };
                let opens_entry = statement.is_some() && entry.is_none() && name == r.entry;
                path.push(name);
                buf.clear();
                if opens_statement {
                    statement = Some((path.len(), HashMap::new(), Vec::new()));
                } else if opens_entry {
                    entries += 1;
                    if entries > MAX_LINES {
                        return Err(too_many());
                    }
                    entry = Some((path.len(), HashMap::new()));
                }
            }
            Xml::Text(t) => buf.push_str(&t.xml10_content()),
            Xml::CData(c) => buf.push_str(&c.xml10_content()),
            // Only character references and the five predefined entities: a declared one reads as nothing.
            Xml::GeneralRef(g) => {
                if let Ok(Some(ch)) = g.resolve_char_ref() {
                    buf.push(ch);
                } else if let Some(v) = quick_xml::escape::resolve_predefined_entity(&g.xml10_content()) {
                    buf.push_str(v);
                }
            }
            Xml::End(_) => {
                let value = std::mem::take(&mut buf).trim().to_string();
                if !value.is_empty() {
                    if let Some((at, values)) = entry.as_mut() {
                        values.entry(path[*at..].join("/")).or_default().push(value);
                    } else if let Some((at, values, _)) = statement.as_mut() {
                        values.entry(path[*at..].join("/")).or_insert(value);
                    }
                }
                if let Some((_, values)) = entry.take_if(|(at, _)| *at == path.len()) {
                    match entry_line(r, &values) {
                        Ok(Some(line)) => {
                            if let Some((_, _, lines)) = statement.as_mut() {
                                lines.push(line);
                            }
                        }
                        Ok(None) => out.skipped += 1,
                        Err(m) => out.errors.push(format!("entry {entries}: {m}")),
                    }
                }
                if let Some((_, values, lines)) = statement.take_if(|(at, ..)| *at == path.len()) {
                    out.statements.push((r.account.iter().find_map(|p| values.get(p)).cloned(), lines));
                }
                path.pop();
            }
            Xml::Eof => {
                if let Some(open) = path.last() {
                    return Ok(Read::error(format!("the file is not well formed: it ends inside <{open}>")));
                }
                break;
            }
            _ => {}
        }
    }
    Ok(out)
}

fn entry_line(r: &Markup, values: &HashMap<String, Vec<String>>) -> std::result::Result<Option<Line>, String> {
    let keep = |v: &&String| !r.empty.iter().any(|e| e == *v);
    let first = |paths: &[String]| paths.iter().find_map(|p| values.get(p)?.iter().find(keep)).cloned();
    if let Some(only) = &r.only
        && let Some(v) = first(&only.at)
        && !only.is.contains(&v)
    {
        return Ok(None);
    }
    let d = date(&first(&r.date).ok_or("no date")?, r.date_chars, r.date_format.as_deref())?;
    let amount = rules::parse_amount(&first(&r.amount).ok_or("no amount")?, r.decimal_comma)?.unwrap_or_default();
    let amount = match &r.sign {
        Some(s) => {
            let v = first(&s.at).unwrap_or_default();
            if s.into.contains(&v) {
                amount.abs()
            } else if s.out.contains(&v) {
                -amount.abs()
            } else {
                return Err(format!("'{v}' says neither money in nor money out"));
            }
        }
        None => amount,
    };
    if amount.is_zero() {
        return Ok(None);
    }
    let mut parts: Vec<&String> = Vec::new();
    for v in r.description.iter().filter_map(|p| values.get(p)).flatten().filter(keep) {
        if !parts.contains(&v) {
            parts.push(v);
        }
    }
    let mut description = squeeze(&parts.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" "));
    let name = r.counterparty.as_ref().and_then(|c| first(if amount > Decimal::ZERO { &c.into } else { &c.out }));
    if let Some(name) = name.map(|n| squeeze(&n))
        && !description.to_lowercase().contains(&name.to_lowercase())
    {
        description = if description.is_empty() { name } else { format!("{name} {description}") };
    }
    Ok(Some(Line::new(d, amount, &description, first(&r.reference), first(&r.bank_reference))))
}

// ───────────────────────────── tagged ─────────────────────────────

/// `:TAG:rest` for a line that starts a field (a tag of 1 to 3 letters or digits).
fn field_start(line: &str) -> Option<(&str, &str)> {
    let rest = line.strip_prefix(':')?;
    let end = rest.find(':')?;
    let tag = &rest[..end];
    ((1..=3).contains(&tag.len()) && tag.chars().all(|c| c.is_ascii_alphanumeric())).then(|| (tag, &rest[end + 1..]))
}

fn read_tagged(text: &str, r: &Tagged) -> std::result::Result<Read, String> {
    let (line_re, strip) = match &r.patterns {
        Some((l, s)) => (l, s.as_ref()),
        None => return Ok(Read::error("the format's pattern was not checked")),
    };
    let account_tag = r.account.as_ref().map(|a| a.tag.as_str());
    // Each statement's fields: (line number, tag, text). A continuation line is kept as a new line of its field.
    let mut statements: Vec<Vec<(usize, &str, String)>> = vec![Vec::new()];
    for (i, raw) in text.lines().enumerate() {
        let l = raw.trim_end_matches('\r');
        let open = statements.last().is_some_and(|s| !s.is_empty());
        if matches!(l.trim(), "-" | "-}") {
            if open {
                statements.push(Vec::new());
            }
        } else if let Some((tag, rest)) = field_start(l) {
            // A second account field starts another statement, when no `-` came between.
            if Some(tag) == account_tag && statements.last().is_some_and(|s| s.iter().any(|(_, t, _)| Some(*t) == account_tag)) {
                statements.push(Vec::new());
            }
            if let Some(s) = statements.last_mut() {
                s.push((i + 1, tag, rest.to_string()));
            }
        } else if let Some((_, _, t)) = statements.last_mut().and_then(|s| s.last_mut()) {
            t.push('\n');
            t.push_str(l);
        }
    }
    let nothing = |v: &str| -> Option<String> {
        let v = v.trim();
        (!v.is_empty() && !r.empty.iter().any(|e| e == v)).then(|| v.to_string())
    };
    let mut out = Read::default();
    let mut count = 0usize;
    for fields in statements.into_iter().filter(|f| !f.is_empty()) {
        let account = account_tag.and_then(|a| fields.iter().find(|(_, t, _)| *t == a)).and_then(|(_, _, v)| nothing(v.lines().next().unwrap_or_default()));
        let mut lines = Vec::new();
        for (k, (no, tag, value)) in fields.iter().enumerate() {
            if *tag != r.line.tag {
                continue;
            }
            count += 1;
            if count > MAX_LINES {
                return Err(too_many());
            }
            let Some(caps) = line_re.captures(value) else {
                out.errors.push(format!("line {no}: not as the format describes"));
                continue;
            };
            let group = |g: &str| caps.name(g).map(|m| m.as_str()).unwrap_or_default();
            let parsed = (|| -> std::result::Result<Option<Line>, String> {
                let d = date(group("date"), r.date_chars, r.date_format.as_deref())?;
                let amount = rules::parse_amount(group("amount"), r.decimal_comma)?.ok_or("no amount")?;
                let amount = match &r.sign {
                    Some(s) if s.into.iter().any(|v| v == group("sign")) => amount.abs(),
                    Some(s) if s.out.iter().any(|v| v == group("sign")) => -amount.abs(),
                    Some(_) => return Err(format!("'{}' says neither money in nor money out", group("sign"))),
                    None => amount,
                };
                if amount.is_zero() {
                    return Ok(None);
                }
                let detail = r.detail.as_ref().and_then(|d| fields.get(k + 1).filter(|(_, t, _)| *t == d.tag)).map(|(_, _, v)| match strip {
                    Some(s) => squeeze(&s.replace_all(v, " ")),
                    None => squeeze(v),
                });
                let description = detail.unwrap_or_else(|| squeeze(group("description")));
                Ok(Some(Line::new(d, amount, &description, nothing(group("reference")), nothing(group("bank_reference")))))
            })();
            match parsed {
                Ok(Some(l)) => lines.push(l),
                Ok(None) => out.skipped += 1,
                Err(m) => out.errors.push(format!("line {no}: {m}")),
            }
        }
        out.statements.push((account, lines));
    }
    Ok(out)
}

// ───────────────────────────── identity ─────────────────────────────

/// Each line's identity on `bank` and whether an earlier line of the same file already has it. A
/// line with the bank's reference is that reference; another is its content, identical lines of one
/// file numbered so each keeps its own identity.
pub fn identities(bank: Uuid, lines: &[Line]) -> Vec<(String, bool)> {
    let mut seen: HashMap<String, u32> = HashMap::new();
    let mut taken: HashSet<String> = HashSet::new();
    lines
        .iter()
        .map(|l| {
            if !l.bank_reference.is_empty() {
                let h = rules::reference_hash(bank, &l.bank_reference);
                let again = !taken.insert(h.clone());
                return (h, again);
            }
            let amount = l.deposit - l.withdrawal;
            let base = rules::statement_hash(bank, l.date, amount, &l.description, &l.reference, 0);
            let n = seen.entry(base).or_insert(0);
            *n += 1;
            (rules::statement_hash(bank, l.date, amount, &l.description, &l.reference, *n), false)
        })
        .collect()
}

// ───────────────────────────── the record ─────────────────────────────

pub struct StatementFormatHook;

#[async_trait]
impl DocHook for StatementFormatHook {
    async fn validate(&self, _hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let kind = doc.get("kind").and_then(Value::as_str).unwrap_or_default();
        reading(kind, doc.get("reading").unwrap_or(&Value::Null)).map(|_| ()).map_err(|m| Error::field("reading", m))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::str::FromStr;

    fn n(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }
    fn day(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }
    /// The seeded format named `name`, checked.
    fn seeded(name: &str) -> Reading {
        let pkgs = crate::packages().unwrap();
        let seed = pkgs[0].manifest.seed.iter().find(|s| s.entity == "StatementFormat").expect("formats are seeded");
        let rec = seed.records.iter().find(|r| r["format_name"] == json!(name)).unwrap_or_else(|| panic!("no seed {name}"));
        reading(rec["kind"].as_str().unwrap(), &rec["reading"]).unwrap_or_else(|e| panic!("{name}: {e}"))
    }
    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }
    fn only_lines(r: &Read) -> Vec<Line> {
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        r.statements.iter().flat_map(|(_, l)| l.clone()).collect()
    }

    #[test]
    fn a_markup_statement_reads_booked_entries() {
        let camt = seeded("ISO 20022 statement (camt.053)");
        let text = fixture("camt053.xml");
        assert!(camt.fits(&text));
        assert!(!seeded("ISO 20022 account report (camt.052)").fits(&text) && !seeded("SWIFT MT940").fits(&text) && !seeded("OFX and QFX").fits(&text));
        let read = camt.read(&text).unwrap();
        assert_eq!(read.statements.len(), 1);
        assert_eq!(read.statements[0].0.as_deref(), Some("GB82WEST12345698765432"));
        assert_eq!(read.skipped, 1, "the pending entry is left out");
        let lines = only_lines(&read);
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert_eq!(
            lines[0],
            Line {
                date: day("2026-03-05"),
                deposit: n("1500.00"),
                withdrawal: n("0"),
                description: "Acme Corp Invoice INV-1001 & delivery".into(),
                reference: "INV-1001".into(),
                bank_reference: "BANK-0001".into()
            }
        );
        // A date with a time, NOTPROVIDED read as nothing, the entry's own text.
        assert_eq!((lines[1].date, lines[1].withdrawal, lines[1].description.as_str()), (day("2026-03-06"), n("25.00"), "Account fee March"));
        assert_eq!((lines[1].reference.as_str(), lines[1].bank_reference.as_str()), ("", "BANK-0002"));
        // A batch: one line, its parts' texts joined, the first payee and reference.
        assert_eq!(
            (lines[2].withdrawal, lines[2].description.as_str(), lines[2].reference.as_str()),
            (n("800.00"), "Landlord Ltd Rent March Electricity", "R-3")
        );
    }

    #[test]
    fn unclosed_leaf_tags_are_closed() {
        let ofx = seeded("OFX and QFX");
        let (sgml, xml) = (fixture("ofx1.ofx"), fixture("ofx2.xml"));
        assert!(ofx.fits(&sgml) && ofx.fits(&xml));
        let a = ofx.read(&sgml).unwrap();
        let b = ofx.read(&xml).unwrap();
        assert_eq!(only_lines(&a), only_lines(&b));
        assert_eq!(a.statements[0].0.as_deref(), Some("98765432"));
        let lines = only_lines(&a);
        assert_eq!(lines.len(), 3);
        assert_eq!((lines[0].date, lines[0].deposit, lines[0].description.as_str()), (day("2026-03-05"), n("1500.00"), "Acme Corp Invoice INV-1001 AT&T line"));
        assert_eq!((lines[1].withdrawal, lines[1].reference.as_str(), lines[1].bank_reference.as_str()), (n("200.00"), "1043", "2026030602"));
        assert_eq!(close_leaves("junk <A><B>x<C>y & z</A>"), "<A><B>x</B><C>y &amp; z</C></A>");
    }

    #[test]
    fn a_tagged_statement_reads_lines_and_details() {
        let mt = seeded("SWIFT MT940");
        let text = fixture("mt940.sta");
        assert!(mt.fits(&text));
        let read = mt.read(&text).unwrap();
        assert!(read.errors.is_empty(), "{:?}", read.errors);
        let accounts: Vec<Option<&str>> = read.statements.iter().map(|(a, _)| a.as_deref()).collect();
        assert_eq!(accounts, [Some("GB82WEST12345698765432"), Some("37040044/0532013000")]);
        let first = &read.statements[0].1;
        assert_eq!(first.len(), 3);
        assert_eq!(
            first[0],
            Line {
                date: day("2026-03-05"),
                deposit: n("1500.00"),
                withdrawal: n("0"),
                description: "166 CREDIT TRANSFER Payment from Acme Corp Invoice INV-1001 and delivery".into(),
                reference: "INV-1001".into(),
                bank_reference: "BANK-0001".into()
            },
            "field 86 continues on the next line; its subfield marks become spaces"
        );
        assert_eq!((first[1].withdrawal, first[1].reference.as_str(), first[1].description.as_str()), (n("25.00"), "", "Account fee March"));
        assert_eq!(first[2].deposit, n("100.00"), "a reversed debit is money in");
        let second = &read.statements[1].1;
        assert_eq!((second.len(), second[0].date, second[0].deposit, second[0].bank_reference.as_str()), (1, day("2026-03-09"), n("10.00"), ""));
        // A line the pattern does not describe is named by its line in the file.
        let bad = mt.read(":20:X\n:25:1\n:61:yesterday\n").unwrap();
        assert_eq!(bad.errors, ["line 3: not as the format describes"]);
    }

    #[test]
    fn a_reading_with_an_unknown_key_or_a_bad_pattern_is_refused() {
        let csv = json!({"columns": {"date": "Date", "amount": "Amount"}});
        assert!(reading("Delimited", &csv).is_ok());
        assert!(reading("Delimited", &json!(csv.to_string())).is_ok(), "as text too");
        assert!(reading("Delimited", &json!({"columns": {"date": "Date", "amount": "Amount"}, "colour": "red"})).unwrap_err().contains("colour"));
        assert!(reading("Delimited", &json!({"columns": {"date": "Date", "total": "Amount"}})).unwrap_err().contains("total"));
        assert!(reading("Delimited", &json!({"columns": {"date": "Date"}})).unwrap_err().contains("amount"));
        assert!(
            reading("Markup", &json!({"entry": "Ntry", "date": ["Dt"], "amount": ["Amt"], "loose": true, "delimiter": ";"})).unwrap_err().contains("delimiter")
        );
        let tagged = |p: &str| json!({"line": {"tag": "61", "pattern": p}});
        assert!(reading("Tagged", &tagged("^(?P<date>\\d{6})(?P<sign>[CD])(?P<amount>[0-9,]+)")).is_ok());
        assert!(reading("Tagged", &tagged("^(?P<date>\\d{6}")).unwrap_err().contains("does not compile"));
        assert!(reading("Tagged", &tagged("^(?P<date>\\d{6})(?P<amount>.*)")).unwrap_err().contains("'sign'"));
        assert!(reading("Tagged", &tagged(&format!("(?P<date>a)(?P<sign>b)(?P<amount>c){}", "x".repeat(500)))).unwrap_err().contains("longer than 500"));
        assert!(
            reading("Tagged", &tagged("(?P<date>\\w{1000}){1000}(?P<sign>a)(?P<amount>b)")).unwrap_err().contains("does not compile"),
            "too big once compiled"
        );
        assert!(reading("Fixed", &json!({})).unwrap_err().contains("Fixed"));
    }

    #[test]
    fn markup_that_nests_too_deep_or_declares_entities_is_refused() {
        let r = reading("Markup", &json!({"entry": "E", "date": ["D"], "amount": ["A"]})).unwrap();
        let deep = format!("{}{}", "<x>".repeat(70), "</x>".repeat(70));
        assert_eq!(r.read(&deep).unwrap().errors, ["the file nests elements deeper than 64 levels"]);
        let declared = "<!DOCTYPE s [<!ENTITY boom \"lots\"> <!ENTITY ext SYSTEM \"file:///etc/passwd\">]>\
                        <s><E><D>2026-03-01</D><A>5</A><T>a&boom;b&ext;c&amp;&#100;</T></E></s>";
        let r = reading("Markup", &json!({"entry": "E", "date": ["D"], "amount": ["A"], "description": ["T"]})).unwrap();
        let read = r.read(declared).unwrap();
        assert_eq!(only_lines(&read)[0].description, "abc&d", "a declared entity reads as nothing");
        assert!(r.read("<s><E><D>2026-03-01</E></s>").unwrap().errors[0].contains("not well formed"));
        assert!(r.read("<s><E>").unwrap().errors[0].contains("not well formed"));
        assert!(r.read("").unwrap().statements.is_empty());
        let many = format!("<s>{}</s>", "<E><D>2026-03-01</D><A>1</A></E>".repeat(MAX_LINES + 1));
        assert!(r.read(&many).unwrap_err().contains("more than 10000"));
        // Every kept value is cut to its field.
        let long = format!("<s><E><D>2026-03-01</D><A>1</A><T>{}</T></E></s>", "y".repeat(5000));
        assert_eq!(only_lines(&r.read(&long).unwrap())[0].description.chars().count(), MAX_DESCRIPTION);
    }

    #[test]
    fn the_same_bank_reference_is_one_line() {
        let l = |desc: &str, bank_ref: &str| Line::new(day("2026-03-05"), n("10"), desc, None, Some(bank_ref.into()));
        let bank = Uuid::nil();
        let ids = identities(bank, &[l("Coffee", "R1"), l("Coffee, again", "R1"), l("Coffee", ""), l("Coffee", "")]);
        assert_eq!(ids[0].0, ids[1].0, "the bank's reference is the line, whatever its text");
        assert_eq!((ids[0].1, ids[1].1), (false, true), "the second is a duplicate");
        assert_ne!(ids[2].0, ids[3].0, "two equal lines without a reference both import");
        assert!(!ids[2].1 && !ids[3].1);
        assert_eq!(ids[0].0, rules::reference_hash(bank, " r1 "));
    }

    #[test]
    fn the_seeded_formats_are_valid() {
        let pkgs = crate::packages().unwrap();
        let seed = pkgs[0].manifest.seed.iter().find(|s| s.entity == "StatementFormat").expect("formats are seeded");
        assert_eq!(seed.records.len(), 4);
        for rec in &seed.records {
            reading(rec["kind"].as_str().unwrap(), &rec["reading"]).unwrap_or_else(|e| panic!("{}: {e}", rec["format_name"]));
        }
    }
}
