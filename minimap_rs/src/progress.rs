//! One output format for the whole pipeline, and one answer to "how long?".
//!
//! These steps run for minutes to hours, and the two audiences for their output
//! want opposite things: a terminal wants a line that overwrites itself, a log
//! file wants lines that accumulate. Both come through here, and [`interactive`]
//! decides which — so no caller has to care, and `make` can tee every stage to
//! `$(LOG)` without producing a megabyte of carriage returns.
//!
//! ```text
//! ==> bake  1,034,854 features -> MVT tiles, z6..14
//!     [########------------]  41%  z13 buildings band 9/16   16.2s elapsed, ~23s left
//! ```
//!
//! **Why the task is a global.** There is exactly one stdout and exactly one
//! line being overwritten on it, so the thing tracking that line is genuinely
//! process-wide state and pretending otherwise only means threading a `&mut`
//! through every function that might want to say something. It also has to be
//! reachable from inside rayon — the extractor's passes run on all cores and
//! are the part with the least to show for the longest — and a shared counter
//! is the only shape that works there. Workers `try_lock`, so a busy terminal
//! never stalls a decode.

use std::io::{IsTerminal, Write};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The log file, when one was asked for.
///
/// This exists because `make ... | tee log` cannot work: the pipe makes stdout
/// a non-terminal, so the live bar switches itself off for the one audience it
/// was written for. Writing the log here instead keeps stdout a terminal and
/// lets the two audiences get genuinely different output -- a bar that
/// overwrites itself on screen, and timestamped lines that accumulate on disk
/// -- rather than the same bytes twice.
static LOG: Mutex<Option<std::fs::File>> = Mutex::new(None);

/// Send durable lines to `path` as well as to stdout.
pub fn log_to(path: &std::path::Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::File::create(path)?;
    if let Ok(mut slot) = LOG.lock() {
        *slot = Some(file);
    }
    Ok(())
}

/// Write one line to the log file, if there is one.
fn to_log(msg: &str) {
    if let Ok(mut slot) = LOG.try_lock() {
        if let Some(file) = slot.as_mut() {
            let _ = writeln!(file, "{msg}");
        }
    }
}

/// A durable line: to the terminal, and to the log.
fn emit(msg: &str) {
    println!("{msg}");
    to_log(msg);
}

/// Whether stdout is a terminal, which is the only thing that makes `\r`
/// meaningful. Checked once: it cannot change, and every redraw asks.
pub fn interactive() -> bool {
    use std::sync::OnceLock;
    static TTY: OnceLock<bool> = OnceLock::new();
    *TTY.get_or_init(|| std::io::stdout().is_terminal())
}

// --- the live task ---------------------------------------------------------

struct Task {
    /// Total weight of the work, in whatever unit the caller chose. Only the
    /// ratio matters, so callers pick whatever they can actually predict —
    /// bytes for a download, `4^zoom` for a bake.
    total: f64,
    done: f64,
    /// What is happening right now, for the "what is going on" half.
    what: String,
    t0: Instant,
    /// Rate-limits redraws on a terminal, and heartbeats in a log.
    last: Instant,
    /// Last percentage written to a log, so a redirected run does not repeat
    /// itself while nothing is changing, and when it last said anything at all.
    shown: i64,
    beat: Instant,
    /// Whether a live line is currently on screen and needs erasing before
    /// anything else prints.
    visible: bool,
}

static TASK: Mutex<Option<Task>> = Mutex::new(None);

/// Redraw no more often than this. Fast enough to look live, slow enough that
/// a bake emitting thousands of updates does not spend its time in `write`.
const TICK: Duration = Duration::from_millis(100);

/// In a log there is no overwriting, so progress is a heartbeat instead. Long
/// enough that an eight-hour bake costs a few hundred lines.
const HEARTBEAT: Duration = Duration::from_secs(30);

/// Begin tracking a unit of work of size `total`.
pub fn begin(total: f64) {
    if let Ok(mut task) = TASK.lock() {
        *task = Some(Task {
            total,
            done: 0.0,
            what: String::new(),
            t0: Instant::now(),
            last: Instant::now() - TICK,
            shown: -1,
            beat: Instant::now(),
            visible: false,
        });
    }
}

/// Say what is being worked on now. Cheap enough to call per item.
pub fn at(what: impl Into<String>) {
    // `try_lock`, here and in `tick`: these are called from rayon workers, and
    // waiting for the terminal is never worth stalling a decode for. A dropped
    // update costs one frame of a display that refreshes ten times a second.
    if let Ok(mut guard) = TASK.try_lock() {
        if let Some(task) = guard.as_mut() {
            task.what = what.into();
            task.draw(false);
        }
    }
}

/// Record `weight` units of the total as done.
pub fn tick(weight: f64) {
    if let Ok(mut guard) = TASK.try_lock() {
        if let Some(task) = guard.as_mut() {
            task.done += weight;
            task.draw(false);
        }
    }
}

/// Both at once, for the common case of finishing a named item.
pub fn done(weight: f64, what: impl Into<String>) {
    if let Ok(mut guard) = TASK.try_lock() {
        if let Some(task) = guard.as_mut() {
            task.done += weight;
            task.what = what.into();
            task.draw(false);
        }
    }
}

/// Stop tracking, and take the live line off the screen.
pub fn end() {
    if let Ok(mut guard) = TASK.lock() {
        if let Some(task) = guard.as_mut() {
            task.erase();
        }
        *guard = None;
    }
}

impl Task {
    fn fraction(&self) -> f64 {
        if self.total > 0.0 {
            (self.done / self.total).clamp(0.0, 1.0)
        } else {
            0.0
        }
    }

    /// Time remaining, from the rate achieved so far.
    ///
    /// Below a few percent this says nothing useful — the first band of a bake
    /// is not representative of the sixty-fourth — so it declines to guess
    /// rather than print a number that will be wrong by an order of magnitude.
    fn eta(&self) -> String {
        let f = self.fraction();
        if f < 0.02 {
            return "estimating".into();
        }
        if f >= 1.0 {
            return "finishing".into();
        }
        let left = self.t0.elapsed().as_secs_f64() * (1.0 - f) / f;
        format!("~{} left", secs(Duration::from_secs_f64(left)))
    }

    fn render(&self) -> String {
        const WIDTH: usize = 20;
        let filled = ((self.fraction() * WIDTH as f64) as usize).min(WIDTH);
        format!(
            "    [{}{}] {:>3}%  {:<38} {:>9} elapsed, {}",
            "#".repeat(filled),
            "-".repeat(WIDTH - filled),
            (self.fraction() * 100.0) as i64,
            clip(&self.what, 38),
            secs(self.t0.elapsed()),
            self.eta(),
        )
    }

    /// `force` bypasses the rate limit, for the redraw after another line has
    /// been printed over the top of the live one.
    fn draw(&mut self, force: bool) {
        if !force && self.last.elapsed() < TICK {
            return;
        }
        self.last = Instant::now();
        if interactive() {
            print!("\r{}\x1b[K", self.render());
            let _ = std::io::stdout().flush();
            self.visible = true;
        }
        // A file cannot be overwritten in place, so there progress is a series
        // of lines: one per 5% of the work, plus a heartbeat so that a single
        // band that takes twenty minutes does not look like a hang. The same
        // applies to stdout when it is not a terminal.
        let pct = (self.fraction() * 100.0) as i64 / 5;
        if pct != self.shown || self.beat.elapsed() >= HEARTBEAT {
            self.shown = pct;
            self.beat = Instant::now();
            let line = self.render();
            to_log(&line);
            if !interactive() {
                println!("{line}");
            }
        }
    }

    fn erase(&mut self) {
        if self.visible {
            print!("\r\x1b[K");
            let _ = std::io::stdout().flush();
            self.visible = false;
        }
    }
}

/// Take the live line down, run `f`, then put it back.
///
/// Everything that prints a durable line goes through this, so no caller has to
/// know whether a progress bar happens to be on screen underneath it.
fn around(f: impl FnOnce()) {
    let mut guard = TASK.lock().ok();
    if let Some(Some(task)) = guard.as_deref_mut() {
        task.erase();
    }
    f();
    // Only a terminal has a line to put back; in a log the bar is just more
    // lines, and redrawing it after every durable one would double the file.
    if interactive() {
        if let Some(Some(task)) = guard.as_deref_mut() {
            task.draw(true);
        }
    }
}

// --- durable lines ---------------------------------------------------------

/// A pipeline stage, from its banner to its timing line.
///
/// Held by value so the elapsed time is the step's own, and so nesting is
/// impossible: a step is one of the four things the Makefile invokes, not any
/// unit of work that felt worth announcing.
pub struct Step {
    name: &'static str,
    t0: Instant,
}

impl Step {
    pub fn start(name: &'static str, detail: impl AsRef<str>) -> Step {
        around(|| emit(&format!("\n==> {name}  {}", detail.as_ref())));
        Step {
            name,
            t0: Instant::now(),
        }
    }

    pub fn elapsed(&self) -> Duration {
        self.t0.elapsed()
    }

    pub fn done(self) {
        end();
        emit(&format!("<== {}  ok  {}", self.name, secs(self.t0.elapsed())));
        let _ = std::io::stdout().flush();
    }
}

/// A detail line under the current step.
pub fn line(msg: impl AsRef<str>) {
    around(|| emit(&format!("    {}", msg.as_ref())));
}

/// A detail line with the time the thing it describes took.
///
/// The message is padded to a fixed column so the durations line up down the
/// page, which is the only way to see at a glance which region or which zoom is
/// the one costing the money.
pub fn timed(msg: impl AsRef<str>, since: Instant) {
    around(|| emit(&format!("    {:<58} {:>9}", msg.as_ref(), secs(since.elapsed()))));
}

/// `[ 3/49] name` — the prefix for per-item lines, so a long run always says
/// where it is without anyone counting.
pub fn item(i: usize, n: usize, name: &str) -> String {
    let w = n.to_string().len();
    format!("[{:>w$}/{n}] {name:<18}", i + 1, w = w)
}

/// A warning that does not stop the run. Goes to stderr so it survives a
/// caller who is only interested in the last line of stdout.
pub fn warn(msg: impl AsRef<str>) {
    around(|| {
        eprintln!("    ! {}", msg.as_ref());
        to_log(&format!("    ! {}", msg.as_ref()));
    });
}

// --- byte-count progress ---------------------------------------------------

/// Progress over a known number of bytes: downloads, and the archive write.
///
/// A thin wrapper over the same task, because bytes are just a weight whose
/// unit happens to be printable.
pub struct Bytes {
    label: String,
    total: u64,
    done: u64,
    t0: Instant,
}

impl Bytes {
    pub fn new(label: impl Into<String>, total: u64) -> Bytes {
        begin(total as f64);
        Bytes {
            label: label.into(),
            total,
            done: 0,
            t0: Instant::now(),
        }
    }

    pub fn add(&mut self, n: u64) {
        self.done += n;
        let rate = self.done as f64 / self.t0.elapsed().as_secs_f64().max(1e-3);
        done(
            n as f64,
            format!(
                "{} {} / {} at {}/s",
                self.label,
                bytes(self.done),
                bytes(self.total),
                bytes(rate as u64)
            ),
        );
    }

    /// Replace the bar with a single settled line: what arrived, how fast.
    pub fn finish(self) {
        end();
        emit(&format!(
            "    {:<18} {:>9} in {}  ({}/s)",
            self.label,
            bytes(self.done),
            secs(self.t0.elapsed()),
            bytes((self.done as f64 / self.t0.elapsed().as_secs_f64().max(1e-3)) as u64),
        ));
        let _ = std::io::stdout().flush();
    }
}

// --- humanising ------------------------------------------------------------

/// Trim to `n` columns with an ellipsis, so one long label cannot wrap the
/// live line and leave the previous frame stranded on screen.
fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    s.chars().take(n.saturating_sub(1)).collect::<String>() + "…"
}

/// Sizes as people say them. Everything here is between a kilobyte and a
/// terabyte, and printing 153876180992 helps nobody.
pub fn bytes(n: u64) -> String {
    const UNIT: [(u64, &str); 4] = [
        (1 << 40, "TB"),
        (1 << 30, "GB"),
        (1 << 20, "MB"),
        (1 << 10, "kB"),
    ];
    for (scale, name) in UNIT {
        if n >= scale {
            return format!("{:.1} {name}", n as f64 / scale as f64);
        }
    }
    format!("{n} B")
}

/// Durations as people say them, from a tenth of a second to hours. A bake is
/// hours and a query is milliseconds, and `4523.7s` is neither readable nor
/// comparable at a glance.
pub fn secs(d: Duration) -> String {
    let s = d.as_secs_f64();
    if s < 60.0 {
        format!("{s:.1}s")
    } else if s < 3600.0 {
        format!("{}m {:02}s", s as u64 / 60, s as u64 % 60)
    } else {
        format!("{}h {:02}m", s as u64 / 3600, (s as u64 % 3600) / 60)
    }
}

/// Thousands separators, because every count here is in the millions.
pub fn commas(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}
