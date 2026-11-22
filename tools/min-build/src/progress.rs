//! Pacman-style progress bars (with `ILoveCandy`) for setting up the tools
//! ([`Progress`], [`ComponentBar`]) and for the build ([`BuildBar`]).
//!
//! Each missing component gets one line. That line goes through two stages:
//! 1. downloading, 2. unpacking/copying. Both stages count bytes: while
//! downloading, the bytes received; while unpacking, the bytes read from the
//! archive (so the archive's size is the stage's length).
//!
//! The bar between the brackets stretches so that the line fills the console
//! window, and follows when the window is resized:
//!
//! ```text
//!  Rust       (1/2) Herunterladen   41.20 MiB   5.31 MiB/s 00:07 [-----------co  o  o  o  o ]  45%
//!  LLVM MinGW (2/2) Entpacken       12.00 MiB  48.02 MiB/s 00:02 [------------------C  o  o ]  71%
//! ```

use std::{
    fmt, io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use console::{Term, measure_text_width, style};
use indicatif::{MultiProgress, ProgressBar, ProgressBarIter, ProgressState, ProgressStyle};

use crate::error::InstallError;

/// Narrowest name column; it grows to fit the longest name added.
const MIN_NAME_COLS: usize = 8;

// Widths of the other columns around the bar. A value wider than its column
// pushes the rest of the line to the right, so these must fit the longest content.
const STAGE_COLS: usize = 19; // "(1/2) Herunterladen", "Fehler: Dateisystem"
const BYTES_COLS: usize = 11; // "1023.99 MiB"
const SPEED_COLS: usize = 13; // "1023.99 MiB/s"
const TIME_COLS: usize = 5; // "mm:ss"
const PERCENT_COLS: usize = 3; // "100"

/// Characters of the line outside the brackets, apart from the name column:
/// the columns above plus the spaces, brackets and `%` that [`template`] puts
/// between them.
const FIXED_COLS: usize = STAGE_COLS + BYTES_COLS + SPEED_COLS + TIME_COLS + PERCENT_COLS + 10;

/// The bar never gets narrower than this, even if the line then gets too long
/// for the window.
const MIN_BAR_COLS: usize = 10;

/// Template for a line; `{pacman}` and `{mmss}` are filled in by our own functions below.
/// Keep the literal characters in sync with [`FIXED_COLS`].
fn template(name_cols: usize) -> String {
    format!(
        " {{prefix:<{name_cols}}} {{msg:<{STAGE_COLS}}} {{bytes:>{BYTES_COLS}}} \
         {{binary_bytes_per_sec:>{SPEED_COLS}}} {{mmss}} [{{pacman}}] {{percent:>{PERCENT_COLS}}}%"
    )
}

/// Cells between the brackets so that the line fills the console window,
/// given the width of everything else on the line (`other_cols`).
///
/// Asked anew on every redraw, so the bars follow when the window is resized.
/// One column stays free: a line that reaches the last column can make the
/// Windows console wrap to the next line, which breaks the redrawing.
fn bar_cols(other_cols: usize) -> usize {
    let window_cols = Term::stderr().size().1 as usize;
    window_cols.saturating_sub(other_cols + 1).max(MIN_BAR_COLS)
}

/// All progress lines that are drawn together.
pub struct Progress {
    multi: MultiProgress,
    lines: Mutex<Lines>,
}

/// The bars added so far and the name column width they share.
struct Lines {
    bars: Vec<ComponentBar>,
    name_cols: usize,
}

impl Progress {
    pub fn new() -> Self {
        Self {
            multi: MultiProgress::new(),
            lines: Mutex::new(Lines {
                bars: Vec::new(),
                name_cols: MIN_NAME_COLS,
            }),
        }
    }

    /// Adds a line for one component, below the lines added before it.
    ///
    /// If `name` is longer than the names before it, the name column of every
    /// line widens (and their bars shrink), so the columns stay aligned.
    pub fn add(&self, name: &str) -> ComponentBar {
        let mut lines = self
            .lines
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let bar = ComponentBar {
            bar: self.multi.add(ProgressBar::no_length()),
            failed: Arc::new(AtomicBool::new(false)),
        };
        bar.bar.set_prefix(name.to_owned());

        let name_cols = lines.name_cols.max(measure_text_width(name));
        if name_cols != lines.name_cols {
            lines.name_cols = name_cols;
            for other in &lines.bars {
                other.apply_style(name_cols);
            }
        }
        bar.apply_style(name_cols);
        lines.bars.push(bar.clone());

        bar
    }
}

/// The progress line of one component (Rust or LLVM-MinGW).
///
/// Cheap to clone and usable from any thread.
#[derive(Clone)]
pub struct ComponentBar {
    bar: ProgressBar,
    /// Set by [`Self::fail`]; tells the drawing code to show the crash.
    failed: Arc<AtomicBool>,
}

impl ComponentBar {
    /// Runs the work for this component and ends the line accordingly:
    /// "fertig" on success, the failure on error. The error is passed on, so
    /// the caller can print the full message once the bars are done.
    pub fn run<T>(
        &self,
        work: impl FnOnce(&ComponentBar) -> Result<T, InstallError>,
    ) -> Result<T, InstallError> {
        let result = work(self);
        match &result {
            Ok(_) => self.finish(),
            Err(error) => self.fail(error),
        }
        result
    }

    /// Starts stage 1. Pass the download's size if known (e.g. from `Content-Length`);
    /// it can also be set later with [`Self::set_length`].
    pub fn start_download(&self, total_bytes: Option<u64>) {
        self.start_stage("(1/2) Herunterladen", total_bytes);
    }

    /// Starts stage 2. `total_bytes` is what the stage will count: the
    /// unpacked size for a zip, or the archive's size when the archive file
    /// itself is read through [`Self::wrap_read`].
    pub fn start_unpack(&self, total_bytes: u64) {
        self.start_stage("(2/2) Entpacken", Some(total_bytes));
    }

    pub fn inc(&self, bytes: u64) {
        self.bar.inc(bytes);
    }

    /// Wraps a reader so that every byte read moves the bar forward: the HTTP
    /// response body while downloading, the archive file while unpacking.
    pub fn wrap_read<R: io::Read>(&self, reader: R) -> ProgressBarIter<R> {
        self.bar.wrap_read(reader)
    }

    /// Leaves the line as a full bar with "fertig".
    pub fn finish(&self) {
        self.bar
            .finish_with_message(style("fertig").green().for_stderr().to_string());
    }

    /// Leaves the line where it stopped: a red `X` instead of the pacman and
    /// the short reason in place of the stage.
    pub fn fail(&self, error: &InstallError) {
        self.failed.store(true, Ordering::Relaxed);
        self.bar.abandon_with_message(failure_label(error));
    }

    fn apply_style(&self, name_cols: usize) {
        self.bar
            .set_style(bar_style(name_cols, self.failed.clone()));
    }

    fn start_stage(&self, label: &'static str, total_bytes: Option<u64>) {
        // `reset` also restarts the speed and time measurement for the new stage.
        self.bar.reset();
        match total_bytes {
            Some(total) => self.bar.set_length(total),
            None => self.bar.unset_length(),
        }
        self.bar.set_message(label);
    }
}

fn failure_label(error: &InstallError) -> String {
    style(format!("Fehler: {}", error.short()))
        .red()
        .bold()
        .for_stderr()
        .to_string()
}

fn bar_style(name_cols: usize, failed: Arc<AtomicBool>) -> ProgressStyle {
    ProgressStyle::with_template(&template(name_cols))
        .expect("progress template is valid")
        .with_key(
            "pacman",
            move |state: &ProgressState, w: &mut dyn fmt::Write| {
                let failed = failed.load(Ordering::Relaxed);
                draw_pacman(state, w, bar_cols(name_cols + FIXED_COLS), failed, false)
            },
        )
        .with_key("mmss", draw_time)
}

/// Draws the inside of the bar the way pacman does with `ILoveCandy`:
/// `-` for the eaten part, the pacman (mouth alternating between `C` and `c`
/// as he moves), and a pellet `o` on every third cell ahead of him.
/// After a failure he is a red `X` where he stopped, with the pellets dimmed.
///
/// With `chomp_by_time` the mouth opens and closes over time instead of with
/// each cell, so he keeps chomping while the bar stands still.
fn draw_pacman(
    state: &ProgressState,
    w: &mut dyn fmt::Write,
    width: usize,
    failed: bool,
    chomp_by_time: bool,
) {
    let eaten = if failed {
        // Keep the `X` visible even when the failure came at 100 %,
        // e.g. a checksum mismatch after a complete download.
        ((state.fraction() * width as f32) as usize).min(width.saturating_sub(1))
    } else if state.is_finished() {
        width
    } else {
        ((state.fraction() * width as f32) as usize).min(width)
    };

    for cell in 0..width {
        let _ = if cell < eaten {
            w.write_char('-')
        } else if cell == eaten && failed {
            write!(w, "{}", style('X').red().bold().for_stderr())
        } else if cell == eaten {
            let open = if chomp_by_time {
                (state.elapsed().as_millis() / 300) % 2 == 0
            } else {
                cell % 2 == 0
            };
            let mouth = if open { 'C' } else { 'c' };
            write!(w, "{}", style(mouth).yellow().bold().for_stderr())
        } else if (width - cell) % 3 == 0 {
            // Pellets are counted from the right end, so they stay put while
            // the pacman approaches and gets them one after another.
            let pellet = style('o').for_stderr();
            let pellet = if failed { pellet.dim() } else { pellet.white() };
            write!(w, "{pellet}")
        } else {
            w.write_char(' ')
        };
    }
}

/// Time column like pacman's: remaining time while running, total time once
/// finished, `--:--` while the size is still unknown.
fn draw_time(state: &ProgressState, w: &mut dyn fmt::Write) {
    let _ = if state.is_finished() {
        write_mmss(w, state.elapsed())
    } else if state.len().is_none() || state.pos() == 0 {
        w.write_str("--:--")
    } else {
        write_mmss(w, state.eta())
    };
}

fn write_mmss(w: &mut dyn fmt::Write, time: Duration) -> fmt::Result {
    let secs = time.as_secs().min(99 * 60 + 59);
    write!(w, "{:02}:{:02}", secs / 60, secs % 60)
}

// --- The build ---------------------------------------------------------------

/// Crate names longer than this are shortened with `...`.
const BUILD_NAME_COLS: usize = 24;
const BUILD_COUNT_COLS: usize = 11; // "(9999/9999)"

/// Characters of a build line outside the brackets: the columns plus the
/// spaces, brackets and `%` that [`build_template`] puts between them.
const BUILD_FIXED_COLS: usize = BUILD_NAME_COLS + BUILD_COUNT_COLS + TIME_COLS + PERCENT_COLS + 8;

/// Keep the literal characters in sync with [`BUILD_FIXED_COLS`].
fn build_template() -> String {
    format!(
        " {{prefix:<{BUILD_NAME_COLS}}} {{count}} {{elapsed_mmss}} [{{pacman}}] \
         {{percent:>{PERCENT_COLS}}}%"
    )
}

/// The progress line of the build:
///
/// ```text
///  gpui_ce                  (512/680) 03:12 [-------------------------c o  o  o  o ]  75%
/// ```
///
/// The name is the crate Cargo started last, the count the finished compile
/// steps out of all. The time is the time taken so far: a remaining time
/// cannot be estimated sensibly, as crates take anything from a blink to
/// minutes, the last one (with LTO) the longest.
#[derive(Clone)]
pub struct BuildBar {
    bar: ProgressBar,
    failed: Arc<AtomicBool>,
}

impl BuildBar {
    /// Starts the line, still without a total: "Vorbereitung" until the
    /// first crate name comes in.
    pub fn new() -> Self {
        let failed = Arc::new(AtomicBool::new(false));
        let pacman_failed = failed.clone();
        let style = ProgressStyle::with_template(&build_template())
            .expect("progress template is valid")
            .with_key(
                "pacman",
                move |state: &ProgressState, w: &mut dyn fmt::Write| {
                    let failed = pacman_failed.load(Ordering::Relaxed);
                    draw_pacman(state, w, bar_cols(BUILD_FIXED_COLS), failed, true)
                },
            )
            .with_key("count", draw_count)
            .with_key(
                "elapsed_mmss",
                |state: &ProgressState, w: &mut dyn fmt::Write| {
                    let _ = write_mmss(w, state.elapsed());
                },
            );

        let bar = ProgressBar::no_length().with_style(style);
        bar.set_prefix("Vorbereitung");
        // Redraws even when nothing happens, so the time keeps running and the
        // pacman keeps chomping through long steps (the last crate, with LTO).
        bar.enable_steady_tick(Duration::from_millis(150));
        Self { bar, failed }
    }

    /// Shows `name` in the name column (shortened if too long).
    pub fn set_crate(&self, name: &str) {
        self.bar.set_prefix(fit(name, BUILD_NAME_COLS));
    }

    /// Sets the number of compile steps the build will report.
    pub fn set_total(&self, total: u64) {
        self.bar.set_length(total);
    }

    /// One compile step is finished.
    pub fn step(&self) {
        self.bar.inc(1);
    }

    /// Leaves the line as a full bar with "fertig".
    pub fn finish(&self) {
        self.bar
            .set_prefix(style("fertig").green().for_stderr().to_string());
        self.bar.finish();
    }

    /// Leaves the line where it stopped, with a red `X` and "Fehler".
    pub fn fail(&self) {
        self.failed.store(true, Ordering::Relaxed);
        let label = style("Fehler").red().bold().for_stderr().to_string();
        self.bar.set_prefix(label);
        self.bar.abandon();
    }
}

/// `(done/total)`; just `(done)` while the total is unknown, nothing before
/// the first step.
fn draw_count(state: &ProgressState, w: &mut dyn fmt::Write) {
    let count = match (state.pos(), state.len()) {
        (_, Some(total)) => format!("({}/{total})", state.pos()),
        (0, None) => String::new(),
        (done, None) => format!("({done})"),
    };
    let _ = write!(w, "{count:<BUILD_COUNT_COLS$}");
}

/// Shortens `text` to `cols` characters, ending in `...` if it was longer.
fn fit(text: &str, cols: usize) -> String {
    if text.chars().count() <= cols {
        text.to_owned()
    } else {
        let kept: String = text.chars().take(cols.saturating_sub(3)).collect();
        format!("{kept}...")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_labels_fit_the_stage_column() {
        let errors = [
            InstallError::download("u", ureq::Error::StatusCode(404)),
            InstallError::Checksum {
                url: "u".into(),
                expected: [0; 32],
                actual: [1; 32],
            },
            InstallError::fs("schreiben", "p", io::Error::other("x")),
            InstallError::unpack("a", "x"),
        ];
        for error in &errors {
            let width = measure_text_width(&failure_label(error));
            assert!(
                width <= STAGE_COLS,
                "{:?} is {width} columns wide",
                error.short()
            );
        }
    }
}
