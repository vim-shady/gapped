//! CLI progress reporting.
//!
//! A single [`Reporter`] is created once per invocation in `main`, passed down
//! to each command, and used to spawn [`ProgressBar`]s for individual phases.
//! When stderr is not a TTY (pipes, CI, redirection), the underlying
//! `MultiProgress` draws to a hidden target — every `spinner`/`counter` call
//! silently becomes a no-op.

use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use std::borrow::Cow;
use std::io::IsTerminal;
use std::sync::LazyLock;
use std::time::Duration;

pub struct Reporter {
    mp: MultiProgress,
}

impl Reporter {
    /// Draws to stderr when it's a TTY; hidden otherwise.
    pub fn stderr() -> Self {
        let target = if std::io::stderr().is_terminal() {
            ProgressDrawTarget::stderr()
        } else {
            ProgressDrawTarget::hidden()
        };
        Self {
            mp: MultiProgress::with_draw_target(target),
        }
    }

    /// Explicitly hidden. Used by tests.
    #[cfg(test)]
    pub fn hidden() -> Self {
        Self {
            mp: MultiProgress::with_draw_target(ProgressDrawTarget::hidden()),
        }
    }

    /// Underlying `MultiProgress`, for wiring into a logger bridge.
    pub fn multi(&self) -> &MultiProgress {
        &self.mp
    }

    /// Spinner for a phase with no pre-known total. Callers may `inc(1)` as
    /// they observe new items; the bar ticks automatically in the background.
    pub fn spinner(&self, msg: impl Into<Cow<'static, str>>) -> ProgressBar {
        let pb = self.mp.add(ProgressBar::new_spinner());
        pb.set_style(SPINNER_STYLE.clone());
        pb.set_message(msg);
        pb.enable_steady_tick(Duration::from_millis(80));
        pb
    }

    /// Counter bar with a known total and a per-second rate + remaining time estimate.
    pub fn counter(&self, msg: impl Into<Cow<'static, str>>, total: u64) -> ProgressBar {
        let pb = self.mp.add(ProgressBar::new(total));
        pb.set_style(COUNTER_STYLE.clone());
        pb.set_message(msg);
        pb
    }
}

impl Default for Reporter {
    fn default() -> Self {
        Self::stderr()
    }
}

static SPINNER_STYLE: LazyLock<ProgressStyle> = LazyLock::new(|| {
    ProgressStyle::with_template("  {spinner:.cyan} {msg:<22} [{elapsed_precise}] {pos:>7}")
        .expect("valid template")
});

static COUNTER_STYLE: LazyLock<ProgressStyle> = LazyLock::new(|| {
    ProgressStyle::with_template(
        "  {spinner:.cyan} {msg:<22} [{elapsed_precise}] [{bar:30.cyan/blue}] {pos}/{len} ({per_sec}, eta {eta})",
    )
    .expect("valid template")
    .progress_chars("##-")
});
