//! The command line, as a type.
//!
//! # Why this is not hand-rolled
//!
//! It was, and the cost showed up as bugs rather than as ugliness. Modes were
//! matched on `args[0]` and every parameter read by position through
//! `args.get(n)`, so a flag was indistinguishable from a positional: `qa rename
//! --toon` took `--toon` as the group name and reported "no check matching
//! Some(\"--toon\")" until the flag was filtered back out by hand at each site.
//! A mistyped flag was silently ignored rather than rejected, because nothing
//! ever looked at the arguments it did not expect.
//!
//! The help text had the same problem from the other side: a 73 line string
//! kept in step with the dispatch by hand, describing defaults that lived as
//! literals hundreds of lines away. It drifted, and there was no way to notice.
//!
//! Deriving both from one definition means the parse and the help cannot
//! disagree, and a default is written once where the reader can see it.
//!
//! # Diagnostics are flags, not environment variables
//!
//! `QA_TRACE=1` and `SWEEP_TRACE=1` used to gate tracing. An environment
//! variable is invisible in the command a person pastes into a bug report, does
//! not appear in `--help`, and cannot be validated. They are `--trace` now.
//!
//! # One output format
//!
//! TOON, always, rather than a column layout for a person and a machine format
//! behind a flag. Two formats mean two code paths through every reporting
//! function, gated on a boolean threaded down from the argument parser, and the
//! one nobody runs is the one that rots. TOON is readable enough to keep as the
//! only answer: a uniform array declares its fields once and spends a line per
//! row, which is the shape a column layout was approximating anyway, without
//! losing any field that happens to contain a space.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

fn parse_timeout_scale(value: &str) -> Result<f64, String> {
    let scale = value
        .parse::<f64>()
        .map_err(|_| "timeout scale must be a number".to_owned())?;
    if scale.is_finite() && (1.0..=10.0).contains(&scale) {
        Ok(scale)
    } else {
        Err("timeout scale must be between 1 and 10".to_owned())
    }
}

/// Whether the checks of one component share a page.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum CheckMode {
    /// A fresh host per check. Nothing a check does can reach its neighbour.
    Isolated,
    /// One host for the group, state carried between checks.
    Sweep,
}

#[derive(Parser)]
#[command(
    name = "ps-qa",
    about = "Drive a Blitz application through its control socket and judge what the renderer did.",
    long_about = None,
    version,
)]
pub struct Cli {
    /// The inspector descriptor to attach to. Defaults to
    /// `target/blitz-control.json`, then the newest one the running application
    /// advertises in the temporary directory.
    #[arg(long, global = true)]
    pub descriptor: Option<PathBuf>,

    /// The application profile: which surfaces exist, what they are called, and
    /// which controls must not be pressed. Defaults to `ps-qa.ron` in the
    /// working directory. Application-driving sweeps fail clearly when neither
    /// path exists; the harness does not guess a built-in application.
    #[arg(long, global = true)]
    pub app: Option<PathBuf>,

    /// Inter-event delay in seconds. Pass 0 to saturate the event queue, which
    /// measures a renderer's coalescing rather than its steady state: at the
    /// default the harness sets the cadence, so the reported frame interval
    /// describes the harness rather than the application.
    #[arg(long, global = true, default_value_t = 1.0 / 60.0)]
    pub pace: f64,

    /// Multiply interaction and rendered-outcome deadlines on an overloaded
    /// runner. The default remains the strict local latency contract; CI must
    /// opt in explicitly rather than silently weakening every check.
    #[arg(
        long,
        global = true,
        default_value_t = 1.0,
        value_parser = parse_timeout_scale
    )]
    pub timeout_scale: f64,

    /// Report the node each step addressed, and why a step chose it.
    #[arg(long, global = true)]
    pub trace: bool,

    #[command(subcommand)]
    pub command: Command,
}

/// Every mode, with its own parameters.
///
/// The doc comment on each variant is what `--help` prints, so the description
/// and the behaviour are the same edit.
#[derive(Subcommand)]
pub enum Command {
    /// Tree size and a role histogram.
    Nodes,

    /// Node count per retained pane, and what retention costs against the whole
    /// tree.
    Panes,

    /// One metrics read, as a frame-window summary.
    Idle,

    /// Assert the blinking-rectangle repro: no missed refreshes, and no frame
    /// interval past two refresh periods. Exits 1 when the blink is present.
    Blink {
        /// Missed refreshes to tolerate before failing.
        #[arg(default_value_t = 0)]
        allowed_missed: u64,
    },

    /// Hidden nodes that still own a painted box, worst first. Retention keeps
    /// some on purpose, so this exits 1 only past a budget.
    Ghost {
        /// Ignore boxes smaller than this, in square pixels.
        #[arg(default_value_t = 64.0)]
        min_area: f64,
        /// How many hidden boxes are acceptable before this fails.
        #[arg(default_value_t = 400)]
        max: usize,
    },

    /// What the application does while nothing happens.
    Drift {
        /// How long to watch, in seconds.
        #[arg(default_value_t = 20.0)]
        seconds: f64,
    },

    /// One metrics read, laid out for reading.
    Frames,

    /// The raw metrics response.
    Metrics,

    /// The semantic tree.
    Tree,

    /// Query the live tree: every control matching a role, a name pattern and a
    /// state, as TOON.
    ///
    /// This is the one to reach for. `layout` answers "where is the node called
    /// exactly this", which is why every real question ended up piped through
    /// awk: "which buttons are off screen", "is anything painting at 0x0",
    /// "what is on this surface at all". Those are filters, and they belong
    /// here rather than in whatever the caller can assemble from a column
    /// dump.
    ///
    /// Patterns are glob-style: `chat*` matches every name starting with
    /// "chat", `*settings*` anywhere, and a bare word is a substring, which is
    /// what a name is usually recalled as.
    Find {
        /// Name pattern. `chat*`, `*close*`, or a bare substring. Omit to match
        /// every node.
        #[arg(default_value = "*")]
        pattern: String,
        /// Only this role: button, textbox, menuitem, checkbox, heading, and so
        /// on. Repeat to accept several.
        #[arg(long)]
        role: Vec<String>,
        /// Only nodes the tree calls visible.
        #[arg(long)]
        visible: bool,
        /// Only nodes the tree calls hidden. The pair that found the retained
        /// panes.
        #[arg(long)]
        hidden: bool,
        /// Only nodes with a non-zero box. A control at 0x0 is in the tree and
        /// on nobody's screen, which is the distinction that cost the most time
        /// to keep re-deriving.
        #[arg(long)]
        painted: bool,
        /// Only nodes whose box lies outside the window, on either axis.
        #[arg(long)]
        offscreen: bool,
        /// Only nodes that are disabled.
        #[arg(long)]
        disabled: bool,
        /// Report how many matched, and nothing else.
        #[arg(long)]
        count: bool,
        /// Stop after this many rows.
        #[arg(long)]
        limit: Option<usize>,
    },

    /// Live boxes: x, y, w, h per named node.
    Layout {
        /// Match nodes whose accessible name contains this.
        #[arg(default_value = "")]
        name: String,
    },

    /// Matching nodes with their attributes, plus the ancestor chain, so a
    /// spill can be read against the container that was meant to clip it.
    Dom {
        /// Match nodes whose accessible name contains this.
        name: String,
        /// How many ancestors to walk up.
        #[arg(default_value_t = 6)]
        depth: usize,
    },

    /// Scroll state and lowest descendants of the main scrolling region.
    Transcript,

    /// The colours the renderer resolved per node, biggest box first, so a
    /// full-window wash names the element that asked for it.
    Paint {
        /// Match nodes whose accessible name contains this.
        #[arg(default_value = "")]
        name: String,
        /// Ignore boxes smaller than this, in square pixels.
        #[arg(default_value_t = 10000.0)]
        min_area: f64,
    },

    /// Named, painted text whose resolved foreground is too close to the
    /// background actually stacked beneath it.
    Contrast {
        /// Match nodes whose accessible name contains this.
        #[arg(default_value = "")]
        name: String,
        /// Minimum WCAG ratio for prose and labels.
        #[arg(long, default_value_t = 4.5)]
        text_ratio: f64,
        /// Minimum WCAG ratio for interactive control chrome.
        #[arg(long, default_value_t = 3.0)]
        control_ratio: f64,
    },

    /// Boxes that stick out of their container, worst first.
    Spill {
        /// Which axis to measure.
        #[arg(default_value = "h")]
        axis: String,
        /// Overhang to tolerate, in pixels.
        #[arg(default_value_t = 1.0)]
        tolerance: f64,
    },

    /// Stream metrics, console and runtime errors.
    Watch {
        /// How long to listen, in seconds.
        #[arg(default_value_t = 20.0)]
        seconds: f64,
    },

    /// Wheel events over a named node.
    Scroll {
        /// How many wheel ticks to send.
        #[arg(default_value_t = 120)]
        ticks: u32,
        /// Pixels per tick. Negative scrolls down.
        #[arg(default_value_t = -80.0)]
        delta: f64,
        /// Match the scroller whose accessible name contains this.
        #[arg(default_value = "")]
        over: String,
    },

    /// Scroll a named node's container directly, rather than by wheel events.
    Drag {
        /// Match the node whose accessible name contains this.
        #[arg(default_value = "")]
        name: String,
        /// Pixels to move per step.
        #[arg(default_value_t = -400.0)]
        dy: f64,
        /// How many steps.
        #[arg(default_value_t = 10)]
        steps: u32,
    },

    /// Drive real keystrokes into a text field.
    Type {
        /// How many characters to send.
        #[arg(default_value_t = 20)]
        count: u32,
        /// Match the field whose accessible name contains this.
        #[arg(default_value = "")]
        name: String,
    },

    /// Send a named key into a scroller, or into a bare node id.
    Key {
        /// pageup, pagedown, home, end, up, down, left, right or tab.
        name: String,
        /// How many times to send it.
        #[arg(default_value_t = 1)]
        count: u32,
        /// Match the scroller whose accessible name contains this.
        #[arg(default_value = "")]
        over: String,
    },

    /// Scroll a named node into view, reporting its y before and after.
    Reveal {
        /// Match the node whose accessible name contains this.
        name: String,
    },

    /// Render what the application actually drew and report the visible ink in
    /// it, for the whole window or one named node.
    ///
    /// This is the only mode that can tell a drawn control from a blank box:
    /// every other reading here comes from the tree, where the two are
    /// identical.
    Capture {
        /// Match the node by accessible name, role:name, #id or @data-slot.
        /// Empty captures the window.
        #[arg(default_value = "")]
        name: String,
        /// Render scale.
        #[arg(default_value_t = 1.0)]
        scale: f64,
        /// Save the rendered pixels as a binary PPM image for visual diagnosis.
        #[arg(long)]
        output: Option<std::path::PathBuf>,
    },

    /// Move, press and release a real pointer over the first match, which is
    /// the path a person's mouse takes.
    ///
    /// `click` synthesises an event at a node id instead, so when a control is
    /// reported working that a sweep calls dead, this is what tells the two
    /// apart.
    Press {
        /// Match the control whose accessible name contains this.
        name: String,
    },

    /// Click the first matching visible, enabled node.
    Click {
        /// Match the control whose accessible name contains this. Omit when
        /// `--id` comes from `find`.
        #[arg(required_unless_present = "id")]
        name: Option<String>,
        /// Activate this exact semantic node id.
        #[arg(long, conflicts_with = "name")]
        id: Option<u64>,
    },

    /// Every button in the running application, measured against what the
    /// renderer drew for it. Reports the ones that cannot be seen.
    ///
    /// Exits 1 on any fault. Does not click anything.
    Audit {
        /// Restrict to one family of controls.
        family: Option<String>,
    },

    /// Click every button and check it did what its name says.
    ///
    /// This presses destructive controls on purpose, so point the application
    /// at a throwaway profile first. Exits 1 on any button that did not act.
    Sweep {
        /// Restrict to one family of controls.
        family: Option<String>,
    },

    /// Sweep every surface, not just the one the application opened on.
    ///
    /// Navigates each surface, expands what is collapsed and hovers every row
    /// first, then clicks what that reveals. Reports what it could not reach
    /// instead of skipping it, so coverage is a number rather than silence.
    Cover {
        /// Restrict to one surface.
        surface: Option<String>,
        /// Activate only controls that have no named outcome check.
        ///
        /// Inventory still materializes and accounts for every concrete
        /// control. Controls already driven by the ordered outcome suite are
        /// not clicked a second time, which avoids turning a coverage audit
        /// into a destructive replay of every repeated row action.
        #[arg(long)]
        unmapped_only: bool,
        /// Where the named outcome checks live. Defaults to `tests/ps-qa`.
        #[arg(long)]
        checks: Option<PathBuf>,
        /// Hard wall-clock budget for the complete sweep.
        #[arg(long, default_value_t = 180)]
        max_seconds: u64,
    },

    /// Count reachable, unreachable, anonymous, manual and outcome-declared
    /// controls on every surface without activating ordinary controls.
    ///
    /// Navigation, section expansion and row hover use semantic node ids. This
    /// is the fast answer to "what can an agent reach?". Add
    /// `--require-outcomes` to make missing named verdicts fail CI; use `cover`
    /// when the generic effect of pressing every eligible control is required.
    Inventory {
        /// Restrict to one surface.
        surface: Option<String>,
        /// Fail when a reachable control has no named outcome check.
        #[arg(long)]
        require_outcomes: bool,
        /// Where the named outcome checks live. Defaults to `tests/ps-qa`.
        #[arg(long)]
        checks: Option<PathBuf>,
    },

    /// Reconcile a saved `inventory` report against named outcome checks.
    ///
    /// This needs no running application. It lets an agent continue filling
    /// coverage from a CI artifact instead of launching another GUI merely to
    /// ask which controls remain unverified.
    Reconcile {
        /// TOON report previously emitted by `ps-qa inventory`.
        inventory: PathBuf,
        /// Where the checks live. Defaults to `tests/ps-qa`.
        #[arg(long)]
        checks: Option<PathBuf>,
    },

    /// Drive every control named by the checks and judge what the renderer did
    /// with it. Exits 1 on any failure.
    Qa {
        /// A group, or a single check's id, so chasing one failure does not
        /// re-run its neighbours.
        selector: Option<String>,
        /// Where the checks live. Defaults to `tests/ps-qa` beneath the working
        /// directory.
        #[arg(long)]
        checks: Option<PathBuf>,
    },

    /// Every check the harness can see, without a running application.
    List {
        /// Where the checks live.
        #[arg(long)]
        checks: Option<PathBuf>,
    },

    /// Drive a component library one component at a time, each in its own
    /// process, and report a verdict per component. Exits 1 on any failure.
    ///
    /// One process per component is the point rather than an inefficiency. A
    /// shared process makes every check order-dependent, and a component that
    /// wedges the renderer takes down every component after it; a failure then
    /// describes its neighbour rather than itself. Each run here starts from a
    /// fresh page, so a verdict is about its own component and nothing else.
    ///
    /// The host is launched with the component's built page, hosts the
    /// inspection socket itself and opens no window, so a sweep of a whole
    /// library runs next to someone using their machine and on a CI box with no
    /// display server. `--host` names the binary and it is expected to print
    /// the descriptor path on stdout when it is ready to be attached to.
    SweepComponents {
        /// Component ids to run. Defaults to every directory under `--dists`.
        ids: Vec<String>,
        /// The headless host binary, which must print its descriptor path on
        /// stdout once it is serving.
        #[arg(long)]
        host: PathBuf,
        /// Where the per-component built pages live, one directory per id.
        #[arg(long)]
        dists: PathBuf,
        /// Where the checks live, one `<id>.ron` per component.
        #[arg(long)]
        checks: Option<PathBuf>,
        /// How long to wait for a component's host to announce itself.
        #[arg(long, default_value_t = 30)]
        startup_timeout: u64,

        /// How the checks of one component relate to each other.
        ///
        /// `isolated` (the default) gives every check its own host, so it runs
        /// against a page nothing has touched. On a component page there are no
        /// surfaces to inherit and a check that opens a menu simply leaves it
        /// open for its neighbour: Dropdown and Select both failed that way
        /// while passing alone, because the next check pressed the same trigger
        /// to prepare itself and closed what was already open.
        ///
        /// `sweep` runs the whole group against one host, sharing state, which
        /// is how a whole-application run behaves and the right choice for a
        /// page whose checks are deliberately a sequence. It is also faster:
        /// one host rather than one per check.
        #[arg(long, value_enum, default_value_t = CheckMode::Isolated)]
        mode: CheckMode,
    },
}

/// Whether `--trace` was given.
///
/// A global rather than a parameter threaded through every driving function.
/// Tracing is read at ten call sites nested several frames deep inside the
/// sweep and the check runner, and passing a bool down to each one would put an
/// argument that means "how to talk about the work" into the signature of every
/// function that does the work. It is written once, before anything runs.
static TRACE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The inter-event delay, in seconds. Set once, from `main`, for the same
/// reason as `TRACE`.
static PACE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Multiplier for interaction and rendered-outcome deadlines. Like `PACE`, it
/// is set once before any check runs and read several frames below the CLI.
static TIMEOUT_SCALE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(1.0_f64.to_bits());

/// Record whether tracing was asked for. Called once, from `main`.
pub fn set_trace(on: bool) {
    TRACE.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// Record the inter-event delay. Called once, from `main`.
pub fn set_pace(seconds: f64) {
    PACE.store(seconds.to_bits(), std::sync::atomic::Ordering::Relaxed);
}

/// Record the validated deadline multiplier. Called once, from `main`.
pub fn set_timeout_scale(scale: f64) {
    TIMEOUT_SCALE.store(scale.to_bits(), std::sync::atomic::Ordering::Relaxed);
}

/// `--app <path>`, if one was given. Set once, from `main`, for the same reason
/// as `TRACE`: the profile is read from inside the reach and sweep code, several
/// frames below anything that has seen the command line.
static APP: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();

/// Record the profile path. Called once, from `main`.
pub fn set_app_profile(path: Option<PathBuf>) {
    let _ = APP.set(path);
}

/// The profile path named on the command line, if any.
pub fn app_profile() -> Option<PathBuf> {
    APP.get().cloned().flatten()
}

/// The inter-event delay, in seconds.
pub fn pace() -> f64 {
    f64::from_bits(PACE.load(std::sync::atomic::Ordering::Relaxed))
}

/// The interaction and rendered-outcome deadline multiplier.
pub fn timeout_scale() -> f64 {
    f64::from_bits(TIMEOUT_SCALE.load(std::sync::atomic::Ordering::Relaxed))
}

/// Whether to name the node a step addressed, and why it chose it.
pub fn trace() -> bool {
    TRACE.load(std::sync::atomic::Ordering::Relaxed)
}

impl Command {
    /// Whether this mode needs product-owned navigation and safety rules.
    ///
    /// Renderer diagnostics deliberately work against any inspectable Blitz
    /// document. Broad application driving does not: guessing which controls
    /// are manual, destructive, or surface openers makes a missing profile a
    /// safety bug rather than a useful default.
    pub fn requires_app_profile(&self) -> bool {
        matches!(
            self,
            Command::Sweep { .. }
                | Command::Cover { .. }
                | Command::Inventory { .. }
                | Command::Qa { .. }
                | Command::SweepComponents { .. }
        )
    }

    /// Whether this mode should announce the descriptor it attached to.
    ///
    /// The answer to "why is this number wrong" is usually "a different
    /// process", so the modes that report raw numbers say what they read them
    /// from.
    pub fn is_dump(&self) -> bool {
        matches!(
            self,
            Command::Metrics | Command::Watch { .. } | Command::Frames | Command::Tree
        )
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{CheckMode, Command, parse_timeout_scale};

    #[test]
    fn timeout_scale_is_explicit_and_bounded() {
        assert_eq!(parse_timeout_scale("1").unwrap(), 1.0);
        assert_eq!(parse_timeout_scale("2.5").unwrap(), 2.5);
        assert!(parse_timeout_scale("0.5").is_err());
        assert!(parse_timeout_scale("11").is_err());
        assert!(parse_timeout_scale("not-a-number").is_err());
    }

    #[test]
    fn only_application_driving_modes_require_a_profile() {
        assert!(Command::Sweep { family: None }.requires_app_profile());
        assert!(
            Command::Qa {
                selector: None,
                checks: None,
            }
            .requires_app_profile()
        );
        assert!(
            Command::SweepComponents {
                ids: Vec::new(),
                host: PathBuf::from("host"),
                dists: PathBuf::from("dist"),
                checks: None,
                startup_timeout: 30,
                mode: CheckMode::Isolated,
            }
            .requires_app_profile()
        );
        assert!(!Command::Metrics.requires_app_profile());
        assert!(
            !Command::Click {
                name: None,
                id: Some(7)
            }
            .requires_app_profile()
        );
    }
}
