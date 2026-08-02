//! `minimap sql "..."` — ask the build database something.
//!
//! This exists so that inspecting a build is not a reason to write another
//! binary. It replaces three example programs whose whole job was to open the
//! database the way [`Config::connect`] opens it, run one query and print it;
//! they kept drifting from the real connection settings, which is exactly the
//! bug that makes a probe lie to you.
//!
//! Every value is cast to VARCHAR in SQL rather than matched on in Rust, so
//! DuckDB does its own formatting for every type it has — including the ones
//! this pipeline invented no opinion about, like GEOMETRY.

use crate::config::Config;
use crate::progress;

type Error = Box<dyn std::error::Error>;

/// Rows printed before giving up on the terminal's scrollback.
const LIMIT: usize = 200;

pub fn run(cfg: &Config, query: &str) -> Result<(), Error> {
    let query = query.trim().trim_end_matches(';').trim();
    if query.is_empty() {
        return Err("nothing to run -- try: make sql Q=\"SELECT * FROM features LIMIT 5\"".into());
    }

    // A statement that returns nothing cannot be wrapped in a SELECT, and a
    // read-only connection cannot run one. The first word decides both.
    let word = query
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let reads = matches!(
        word.as_str(),
        "select" | "with" | "from" | "describe" | "summarize" | "show" | "pragma" | "table" | "explain"
    );

    let con = cfg.connect(!reads)?;
    if !reads {
        let t0 = std::time::Instant::now();
        con.execute_batch(query)?;
        println!("ok  ({})", progress::secs(t0.elapsed()));
        return Ok(());
    }

    // `COLUMNS(*)` applies the cast to every column at once and keeps the
    // names, so this works for any shape of result without naming anything.
    let t0 = std::time::Instant::now();
    let mut stmt = con.prepare(&format!("SELECT CAST(COLUMNS(*) AS VARCHAR) FROM ({query})"))?;
    let mut rows = stmt.query([])?;

    let mut table: Vec<Vec<String>> = Vec::new();
    let mut truncated = 0usize;
    let mut width: Vec<usize> = Vec::new();
    while let Some(row) = rows.next()? {
        if table.len() >= LIMIT {
            truncated += 1;
            continue;
        }
        let mut out = Vec::new();
        let mut i = 0;
        while let Ok(v) = row.get::<_, Option<String>>(i) {
            let v = v.unwrap_or_else(|| "NULL".into());
            if width.len() <= i {
                width.push(0);
            }
            width[i] = width[i].max(v.chars().count());
            out.push(v);
            i += 1;
        }
        table.push(out);
    }

    // Column names come off the statement, so they survive expressions that
    // were never given an alias.
    let names: Vec<String> = stmt
        .column_names()
        .into_iter()
        .map(|n| n.to_string())
        .collect();
    for (i, n) in names.iter().enumerate() {
        if width.len() <= i {
            width.push(0);
        }
        width[i] = width[i].max(n.chars().count());
    }
    // A single runaway column should not push every other one off the screen.
    for w in width.iter_mut() {
        *w = (*w).min(48);
    }

    let rule: Vec<String> = width.iter().map(|w| "-".repeat(*w)).collect();
    println!("{}", pad(&names, &width));
    println!("{}", rule.join("  "));
    for row in &table {
        println!("{}", pad(row, &width));
    }
    println!(
        "\n{} row{}{}  ({})",
        progress::commas(table.len() as u64),
        if table.len() == 1 { "" } else { "s" },
        if truncated > 0 {
            format!(", {} more not shown", progress::commas(truncated as u64))
        } else {
            String::new()
        },
        progress::secs(t0.elapsed())
    );
    Ok(())
}

fn pad(cells: &[String], width: &[usize]) -> String {
    cells
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let w = width.get(i).copied().unwrap_or(0);
            let mut c = c.replace('\n', " ");
            if c.chars().count() > w {
                c = c.chars().take(w.saturating_sub(1)).collect::<String>() + "…";
            }
            format!("{c:<w$}")
        })
        .collect::<Vec<_>>()
        .join("  ")
        .trim_end()
        .to_string()
}
