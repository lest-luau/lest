//! lest: test, lest your code break.
//!
//! Exit codes: 0 = all tests passed, 1 = test failures, 2 = tool error.
//! Never conflated.

mod backend;
mod changed;
mod config;
mod coverage;
mod discover;
mod embed;
mod error;
mod init;
mod report;
mod resolve;
mod self_cmd;
mod snapshot;
mod watch;

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::report::{
    paint, render_warning, sentence, CoverageData, Event, Json, Junit, Pretty, SnapshotSummary,
    Totals, BOLD, BOLD_RED, PROTOCOL_VERSION,
};
// Re-exported at the crate root because watch mode addresses the diagnostic
// voice as `crate::render_diagnostic` / `crate::Severity`; the definitions
// live in `report::diagnostic`, next to the palette they use.
pub use crate::report::{render_diagnostic, Severity};
use clap::builder::styling::{Style, Styles};
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};

use backend::native::CoverageMap;
use backend::SuitePlan;
use config::{BackendKind, Config, Suite};
use error::ToolError;
use snapshot::SnapshotStore;

#[derive(Debug, Parser)]
// `bin_name` is pinned rather than taken from argv[0] so Windows doesn't print
// `Usage: lest.exe` — the docs, the config, and every other line say `lest`.
#[command(
    name = "lest",
    bin_name = "lest",
    version,
    about = "Test, lest your code break."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Bare `lest` behaves as `lest run`.
    #[command(flatten)]
    run_args: RunArgs,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run test suites (the default command).
    Run(RunArgs),
    /// Create lest.toml by answering a few questions.
    Init(InitArgs),
    /// Manage the lest installation (PATH; updates later).
    #[command(name = "self")]
    SelfCmd(SelfArgs),
}

#[derive(Debug, Args)]
struct SelfArgs {
    #[command(subcommand)]
    action: SelfAction,
}

#[derive(Debug, Subcommand)]
enum SelfAction {
    /// Copy lest into ~/.lest/bin and add it to your PATH.
    Install,
    /// Remove lest from your PATH and delete ~/.lest/bin.
    Uninstall,
}

#[derive(Debug, Args, Clone)]
pub struct RunArgs {
    /// Suites to run by name; all default suites when omitted. Also the only
    /// way to run suites configured with `default = false` outside CI.
    #[arg(value_name = "SUITE")]
    suites: Vec<String>,

    /// Force every selected suite onto one backend for this run.
    #[arg(long, value_name = "BACKEND")]
    backend: Option<BackendKind>,

    /// Only run tests whose full name contains this text (plain substring).
    #[arg(short = 't', long = "filter", value_name = "TEXT")]
    filter: Option<String>,

    /// Output format.
    #[arg(long, value_enum, default_value_t = ReporterKind::Pretty)]
    reporter: ReporterKind,

    /// Re-run affected suites when files they depend on change.
    #[arg(long)]
    watch: bool,

    /// Path to the config file (default: ./lest.toml when present).
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Disable colored output.
    #[arg(long)]
    no_color: bool,

    /// Overwrite snapshots that differ instead of failing on the difference.
    #[arg(short = 'u', long)]
    update: bool,

    /// Measure line coverage (native suites only) and print a coverage report.
    #[arg(long)]
    coverage: bool,

    /// Coverage output format.
    #[arg(long, value_enum, default_value_t = CoverageFormat::Table)]
    coverage_format: CoverageFormat,

    /// Fail (exit 1) when overall coverage is below this percentage. Implies
    /// `--coverage`.
    #[arg(long, value_name = "PCT")]
    min: Option<f64>,

    /// Run only the specs affected by files changed since this git ref.
    ///
    /// Rejected alongside `--watch`: watch mode already selects by change, from
    /// the file system rather than from git, and layering a fixed git ref on
    /// top would pin the selection to the moment the loop started. Conflicting
    /// out loud beats accepting the flag and ignoring it.
    #[arg(long, value_name = "REF", conflicts_with = "watch")]
    changed: Option<String>,
}

#[derive(Debug, Args)]
struct InitArgs {
    /// Accept every default without prompting (for scripts and CI).
    #[arg(long, short = 'y')]
    yes: bool,

    /// Disable colored output.
    #[arg(long)]
    no_color: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
#[clap(rename_all = "lowercase")]
pub enum ReporterKind {
    Pretty,
    Json,
    Junit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
#[clap(rename_all = "lowercase")]
pub enum CoverageFormat {
    Table,
    Lcov,
}

/// Reporter dispatch: concrete reporters, one switch point. All share the same
/// surface so the CLI treats them interchangeably. The output stream is boxed
/// because it is no longer always stdout — under `--coverage-format=lcov` the
/// lcov document owns stdout and the report moves to stderr.
pub enum AnyReporter {
    Pretty(Pretty<Box<dyn std::io::Write>>),
    Json(Json<Box<dyn std::io::Write>>),
    Junit(Junit<Box<dyn std::io::Write>>),
}

impl AnyReporter {
    /// Reporter on stdout, the normal home of the test report.
    pub fn new(kind: ReporterKind, color: bool) -> Self {
        Self::with_output(kind, color, Box::new(std::io::stdout()))
    }

    /// Reporter on an explicit stream, for when stdout belongs to something
    /// else (the lcov document).
    pub fn with_output(kind: ReporterKind, color: bool, out: Box<dyn std::io::Write>) -> Self {
        match kind {
            ReporterKind::Pretty => AnyReporter::Pretty(Pretty::new(out, color)),
            ReporterKind::Json => AnyReporter::Json(Json::new(out)),
            ReporterKind::Junit => AnyReporter::Junit(Junit::new(out)),
        }
    }

    pub fn begin_suite(&mut self, name: &str, backend: &str) {
        match self {
            AnyReporter::Pretty(r) => r.begin_suite(name, backend),
            AnyReporter::Json(r) => r.begin_suite(name, backend),
            AnyReporter::Junit(r) => r.begin_suite(name, backend),
        }
    }

    pub fn note(&mut self, message: &str) {
        match self {
            AnyReporter::Pretty(r) => r.note(message),
            AnyReporter::Json(r) => r.note(message),
            AnyReporter::Junit(r) => r.note(message),
        }
    }

    /// Snapshots are compared CLI-side, so a mismatch never arrives as a
    /// protocol event — but it still fails the run. Routing it through `note`
    /// dropped it entirely for JUnit, which published `failures="0"` alongside
    /// exit 1; each reporter now renders it in its own idiom, and JUnit gets a
    /// real `<testcase>` a CI annotation can point at.
    pub fn snapshot_failure(&mut self, spec: &str, key: &str, detail: &str) {
        match self {
            AnyReporter::Pretty(r) => r.snapshot_failure(spec, key, detail),
            AnyReporter::Json(r) => r.snapshot_failure(spec, key, detail),
            AnyReporter::Junit(r) => r.snapshot_failure(spec, key, detail),
        }
    }

    /// A backend died mid-suite. A dead backend emits nothing, so the event
    /// stream cannot say so — yet the suite is anything but passed, and the
    /// summary above the fatal error used to claim it was. Each reporter
    /// renders the verdict in its own idiom; the CLI still prints the
    /// detailed diagnostic after the summary.
    pub fn suite_error(&mut self, message: &str) {
        match self {
            AnyReporter::Pretty(r) => r.suite_error(message),
            AnyReporter::Json(r) => r.suite_error(message),
            AnyReporter::Junit(r) => r.suite_error(message),
        }
    }

    /// Marks the suite that just ran failed because `count` of its snapshots
    /// mismatched. Pretty prints a pointer line and fixes its `Test Suites:`
    /// count; Json consumers already see the corrected synthesized `run_end`
    /// plus a structured `snapshot_failure` line per key; JUnit synthesizes a
    /// real `<testcase>` per mismatch, so anything here would double-report.
    pub fn suite_snapshot_failures(&mut self, count: usize) {
        match self {
            AnyReporter::Pretty(r) => r.suite_snapshot_failures(count),
            AnyReporter::Json(_) | AnyReporter::Junit(_) => {}
        }
    }

    pub fn on_event(&mut self, event: &Event) {
        match self {
            AnyReporter::Pretty(r) => r.on_event(event),
            AnyReporter::Json(r) => r.on_event(event),
            AnyReporter::Junit(r) => r.on_event(event),
        }
    }

    pub fn finish(
        &mut self,
        totals: &Totals,
        snapshots: &SnapshotSummary,
        elapsed: std::time::Duration,
    ) {
        match self {
            AnyReporter::Pretty(r) => r.finish(totals, snapshots, elapsed),
            // Json and Junit have no snapshot line; the summary is unaffected.
            AnyReporter::Json(r) => r.finish(totals, elapsed),
            AnyReporter::Junit(r) => r.finish(totals, elapsed),
        }
    }
}

/// Clap's help in the lest palette: `Usage:`/`Commands:`/`Options:` bold and
/// otherwise uncolored, exactly like the `Tests:` and `Coverage:` labels the
/// reporters print; dim metavars so flags read as the foreground. Color is left
/// to mean pass/fail, as it does everywhere else in the CLI.
fn help_styles() -> Styles {
    let label = Style::new().bold();
    Styles::plain()
        .header(label)
        .usage(label)
        .literal(Style::new())
        .placeholder(Style::new().dimmed())
}

/// Rewrites clap's rendered parse error into lest's voice: `Error:` bold red
/// and `Usage:` bold like every other label, sentences capitalized with no
/// trailing period, and clap's `tip:` prefix dropped — the indented line under
/// the error already reads as advice.
///
/// This works on the *rendered* text rather than the [`clap::Error`] because
/// clap builds the message internally; only the sentence-shaped lines are
/// touched, so structural lines (possible values, blanks) pass through as-is.
/// The single-line case matches [`render_diagnostic`] byte for byte.
fn restyle_parse_error(rendered: &str, color: bool) -> String {
    let mut result = String::new();
    for line in rendered.lines() {
        let indent = &line[..line.len() - line.trim_start().len()];
        let trimmed = line.trim_start();
        // `.expect` rather than `let _ =`: writing to a String is infallible, and
        // the crate says so in one voice (see build.rs) instead of discarding a
        // Result in one place and asserting it in another.
        if let Some(rest) = trimmed.strip_prefix("error: ") {
            writeln!(
                result,
                "{indent}{} {}",
                paint(color, BOLD_RED, "Error:"),
                sentence(rest)
            )
            .expect("writing to a String cannot fail");
        } else if let Some(rest) = trimmed.strip_prefix("tip: ") {
            writeln!(result, "{indent}{}", sentence(rest))
                .expect("writing to a String cannot fail");
        } else if let Some(rest) = trimmed.strip_prefix("Usage:") {
            writeln!(result, "{indent}{}{rest}", paint(color, BOLD, "Usage:"))
                .expect("writing to a String cannot fail");
        } else {
            writeln!(result, "{line}").expect("writing to a String cannot fail");
        }
    }
    result
}

fn main() {
    // Help and usage errors are rendered by clap *during* parsing, so `--no-color`
    // has to be read off argv here — by the time it lands in `RunArgs` the text
    // is already printed. The other two off-switches match `execute_run`.
    // `take_while` stops at the `--` escape so an argument *value* spelled
    // `--no-color` after it cannot switch styling off.
    let no_color = std::env::args()
        .take_while(|a| a != "--")
        .any(|a| a == "--no-color")
        || std::env::var_os("NO_COLOR").is_some();
    let color = !no_color && std::io::stdout().is_terminal();
    let command = Cli::command().styles(if color {
        help_styles()
    } else {
        Styles::plain()
    });
    let cli = match command
        .try_get_matches()
        .and_then(|m| Cli::from_arg_matches(&m))
    {
        Ok(cli) => cli,
        Err(err) => {
            // `--help` and `--version` arrive here too, but they are output, not
            // errors: they go to stdout already styled, and exit 0.
            if !err.use_stderr() {
                err.exit();
            }
            let color = !no_color && std::io::stderr().is_terminal();
            eprint!("{}", restyle_parse_error(&err.render().to_string(), color));
            // A malformed command line is a tool error, never a test failure.
            std::process::exit(2);
        }
    };
    let result = match cli.command {
        Some(Command::Init(args)) => init::run(args.yes, args.no_color).map(|()| 0),
        Some(Command::SelfCmd(args)) => match args.action {
            SelfAction::Install => self_cmd::install().map(|()| 0),
            SelfAction::Uninstall => self_cmd::uninstall().map(|()| 0),
        },
        Some(Command::Run(args)) => execute_run(args),
        None => execute_run(cli.run_args),
    };
    let code = match result {
        Ok(code) => code,
        Err(err) => {
            let color = !no_color && std::io::stderr().is_terminal();
            eprint!(
                "{}",
                render_diagnostic(Severity::Error, &err.to_string(), color)
            );
            2
        }
    };
    std::process::exit(code);
}

/// Parameters for a single `run_suites` invocation, bundled so the function
/// stays under the argument-count clippy lint and callers read clearly.
pub struct RunParams<'a> {
    /// Restrict execution to this subset of spec files (watch / `--changed`).
    /// Paths are canonical cache-key paths, matched via `cache_key_path`.
    pub only_specs: Option<&'a HashSet<PathBuf>>,
    pub name_filter: Option<&'a str>,
    pub reporter: ReporterKind,
    pub color: bool,
    /// Overwrite differing snapshots rather than failing (`-u`).
    pub update: bool,
    /// Instrument native suites for line coverage.
    pub coverage: bool,
}

/// Hooks that extend a run beyond [`RunParams`]. They live in their own struct
/// with a `Default` because `watch.rs` builds `RunParams` with an exhaustive
/// struct literal — a new field there would break it, whereas everything here
/// stays additive: `RunExtras::default()` reproduces the historical behavior
/// exactly.
#[derive(Default)]
pub struct RunExtras<'a> {
    /// Spec files already discovered by the caller, keyed by suite name.
    /// `None` means [`run_suites_with`] walks the tree itself (the one-shot
    /// default). Watch mode discovers every suite's specs while selecting
    /// affected tests, so it can hand that result straight in and skip the
    /// second walk per save. When `Some`, the map is authoritative: a suite
    /// absent from it simply has no specs this pass.
    pub pre_discovered: Option<&'a HashMap<String, Vec<PathBuf>>>,
    /// Send the human report to stderr, leaving stdout to a machine document.
    /// Set when `--coverage-format=lcov` is in effect: the lcov text owns
    /// stdout so `lest --coverage --coverage-format=lcov > lcov.info` captures
    /// records and nothing else.
    pub report_to_stderr: bool,
}

/// Builds the reporter for a run: stdout normally, stderr when stdout belongs
/// to the lcov document.
fn make_reporter(kind: ReporterKind, color: bool, to_stderr: bool) -> AnyReporter {
    if to_stderr {
        AnyReporter::with_output(kind, color, Box::new(std::io::stderr()))
    } else {
        AnyReporter::new(kind, color)
    }
}

fn execute_run(args: RunArgs) -> Result<i32, ToolError> {
    let cwd = std::env::current_dir()
        .map_err(|e| ToolError(format!("cannot determine working directory: {e}")))?;
    // Resolved against the *working directory* once, here. Everything
    // downstream is handed the project root instead, and `config::load` joins a
    // relative path onto whatever it is given — so passing the original
    // `--config` on to watch mode would resolve it a second time against the
    // config's own directory and double the path.
    let config_path = args.config.as_ref().map(|path| {
        if path.is_absolute() {
            path.clone()
        } else {
            cwd.join(path)
        }
    });
    let (config, root) = config::load(config_path.as_deref(), &cwd)?;
    // Dev convenience: load a project-root `.env` so cloud credentials
    // (`ROBLOX_API_KEY` etc.) are available without exporting them by hand.
    // Real environment variables already set are never overridden. A missing
    // file is the normal case, but a malformed one is a mistake worth naming —
    // silently continuing would look like the key simply was not set.
    let env_file = root.join(".env");
    match dotenvy::from_path(&env_file) {
        Ok(()) => {}
        Err(err) if err.not_found() => {}
        Err(err) => {
            return Err(ToolError(format!(
                "cannot load {}: {err}",
                env_file.display()
            )))
        }
    }
    let selected = select_suites(&config, &args.suites, args.backend)?;
    // stdout and stderr are redirected independently, so each stream gets its
    // own answer: reports go to stdout, diagnostics to stderr.
    let no_color = args.no_color || std::env::var_os("NO_COLOR").is_some();
    let color = !no_color && std::io::stdout().is_terminal();
    let err_color = !no_color && std::io::stderr().is_terminal();

    if args.watch {
        return watch::run(&args, &root, config_path.as_deref(), color, err_color).map(|()| 0);
    }

    // `--min` is meaningless without coverage, so it implies it — and so does a
    // `[coverage] min` in the config, which otherwise gated on nothing: without
    // instrumentation there is no percentage to compare and the gate returned
    // early, passing every run it was meant to fail.
    let coverage_on = args.coverage || args.min.is_some() || config.coverage.min.is_some();

    // Under `--coverage-format=lcov` stdout is a machine document: the lcov
    // text owns it and the human report moves to stderr, so redirecting stdout
    // to a file captures lcov records and nothing else. The report's color
    // then follows the stream it actually writes to.
    let lcov_to_stdout = coverage_on && args.coverage_format == CoverageFormat::Lcov;
    let report_color = if lcov_to_stdout { err_color } else { color };

    // `--changed` narrows the run to the specs affected by a git diff.
    let only_specs = match &args.changed {
        Some(git_ref) => Some(resolve_changed(&root, &selected, git_ref)?),
        None => None,
    };
    if let (Some(git_ref), Some(only)) = (&args.changed, &only_specs) {
        if only.is_empty() {
            // An empty affected set is an *answer*, not a mistake: the user
            // asked for "specs affected since <ref>" and there are none, so
            // this exits 0. It renders through the reporter so a `--reporter
            // json`/`junit` stream stays a valid document rather than gaining
            // bare prose. The coverage gate is skipped on purpose — the empty
            // run was requested via `--changed`, and gating on a run asked to
            // contain nothing would fail every no-op change.
            let mut reporter = make_reporter(args.reporter, report_color, lcov_to_stdout);
            reporter.note(&format!("no specs affected by changes since {git_ref}"));
            if args.min.or(config.coverage.min).is_some() {
                reporter.note("coverage minimum not enforced — `--changed` selected no specs");
            }
            reporter.finish(
                &Totals::default(),
                &SnapshotSummary::default(),
                std::time::Duration::ZERO,
            );
            return Ok(0);
        }
    }

    let params = RunParams {
        only_specs: only_specs.as_ref(),
        name_filter: args.filter.as_deref(),
        reporter: args.reporter,
        color: report_color,
        update: args.update,
        coverage: coverage_on,
    };
    let extras = RunExtras {
        pre_discovered: None,
        report_to_stderr: lcov_to_stdout,
    };
    let outcome = run_suites_with(&config, &root, &selected, &params, &extras)?;
    if outcome.specs == 0 {
        return Err(ToolError(no_specs_message(&config)));
    }
    // A filter that selects nothing is a mistake in the invocation, not a
    // passing run: exiting 0 here would make a typo'd `-t` in CI look exactly
    // like a green suite.
    if let Some(filter) = params.name_filter {
        if outcome.ran_nothing() {
            return Err(ToolError(no_match_message(filter)));
        }
    }

    let mut code = if outcome.totals.failed > 0 || outcome.snapshots.failed > 0 {
        1
    } else {
        0
    };

    if coverage_on {
        code = code.max(report_coverage(
            &args,
            &config,
            outcome.coverage.as_ref(),
            color,
            err_color,
        ));
    }

    Ok(code)
}

/// The message for a run that discovered no spec files. In zero-config mode
/// there is no `lest.toml` to check — pointing at one would send the reader to
/// a file that does not exist — so that case names the default glob instead.
/// Shared so the one-shot run and watch mode's startup check cannot drift.
pub fn no_specs_message(config: &Config) -> String {
    match &config.file {
        Some(path) => format!(
            "no spec files matched any selected suite — check the `include` globs in {}",
            path.display()
        ),
        None => "no spec files matched any selected suite — there is no lest.toml here, so lest \
                 looked for `**/*.spec.luau`; run `lest init` to configure suites of your own"
            .to_string(),
    }
}

/// Discovers the changed specs for `--changed`, canonicalized to match graph
/// node identity. A missing git binary or non-repo is a clean tool error.
fn resolve_changed(
    root: &Path,
    suites: &[Suite],
    git_ref: &str,
) -> Result<HashSet<PathBuf>, ToolError> {
    let changed = changed::changed_paths(root, git_ref)?;
    let mut all_specs: Vec<PathBuf> = Vec::new();
    for suite in suites {
        all_specs.extend(discover::discover(root, &suite.include)?);
    }
    Ok(changed::affected_specs(root, all_specs, &changed))
}

/// Prints the coverage report and returns an exit code contribution: `1` when
/// coverage falls below the configured minimum, `2` when a minimum was set but
/// nothing was instrumented, else `0`. `color` styles the table on stdout,
/// `err_color` the gate diagnostics on stderr.
pub fn report_coverage(
    args: &RunArgs,
    config: &Config,
    coverage: Option<&CoverageData>,
    color: bool,
    err_color: bool,
) -> i32 {
    let Some(coverage) = coverage else {
        return 0;
    };
    match args.coverage_format {
        // lcov is emitted whenever explicitly requested, regardless of the
        // reporter — it owns stdout (the report went to stderr).
        CoverageFormat::Lcov => print!("{}", coverage.to_lcov()),
        CoverageFormat::Table if args.reporter == ReporterKind::Pretty => {
            println!();
            print!("{}", coverage.table(color));
        }
        // The table belongs to the pretty report; splicing it into a json or
        // junit stream would corrupt the document those reporters promise.
        CoverageFormat::Table => {}
    }

    let Some(min) = args.min.or(config.coverage.min) else {
        return 0;
    };
    match coverage.overall_percent() {
        None => {
            // A gate over nothing is a misconfiguration, not a pass: every
            // selected suite ran on a non-native backend, so there is no
            // percentage to compare and exiting 0 would green-light CI while
            // measuring nothing. (The `--changed`-matched-nothing path never
            // reaches here — an empty run was requested, and skips the gate
            // with a note instead.)
            eprint!(
                "{}",
                render_diagnostic(
                    Severity::Error,
                    "cannot enforce the coverage minimum — no native suite was instrumented, so \
                     there is no coverage to compare; select a native suite or drop the minimum",
                    err_color,
                )
            );
            2
        }
        Some(pct) if pct + 1e-9 < min => {
            // A missed gate is the project's shortfall, not lest malfunctioning:
            // `Failure:` and exit 1, matching how a failing test is reported.
            eprint!(
                "{}",
                render_diagnostic(
                    Severity::Failure,
                    &format!("coverage {pct:.1}% is below the required minimum {min:.1}%"),
                    err_color,
                )
            );
            1
        }
        Some(_) => 0,
    }
}

pub struct RunOutcome {
    pub totals: Totals,
    pub specs: usize,
    pub snapshots: SnapshotSummary,
    pub coverage: Option<CoverageData>,
}

impl RunOutcome {
    /// True when spec files ran but not one of them produced a test outcome.
    /// Filtering happens Luau-side inside core, so the CLI cannot know a
    /// filter's match count up front — it can only observe that nothing came
    /// back. Callers pair this with a name filter being set to distinguish
    /// "your `-t` matched nothing" from the backend load failures the
    /// per-backend guards catch.
    pub fn ran_nothing(&self) -> bool {
        self.specs > 0
            && self.totals.passed == 0
            && self.totals.failed == 0
            && self.totals.skipped == 0
    }
}

/// A test's full name — the describe path joined with spaces, then the test
/// name: the same identity core uses for `-t` filtering and snapshot keys.
/// Used to match host-side snapshot verdicts back to streamed test outcomes.
fn full_test_name(path: &[String], name: &str) -> String {
    if path.is_empty() {
        name.to_string()
    } else {
        format!("{} {}", path.join(" "), name)
    }
}

/// The message shown when `-t` selects no tests. Shared so the hard failure in
/// a one-shot run and the note in watch mode never drift apart.
pub fn no_match_message(filter: &str) -> String {
    format!(
        "no tests matched -t \"{filter}\" — the filter is a plain substring of the full name \
         (describe path joined with spaces, then the test name)"
    )
}

/// Discovers and runs the given suites, streaming events into a reporter.
/// `params.only_specs` restricts execution to a subset of spec files (watch /
/// `--changed`); a suite whose discovery comes up empty is skipped with a note
/// only on unrestricted runs, where it usually means a misconfigured glob.
///
/// Run framing is normalized here: each backend's own `run_start`/`run_end`
/// events are dropped and the CLI synthesizes exactly one pair per suite, so
/// JSON consumers see one structure regardless of backend. Snapshots are
/// compared CLI-side, and the reporter's summary always runs — even when a
/// backend errors mid-run — so partial output is never left unterminated.
pub fn run_suites(
    config: &Config,
    root: &Path,
    suites: &[Suite],
    params: &RunParams,
) -> Result<RunOutcome, ToolError> {
    run_suites_with(config, root, suites, params, &RunExtras::default())
}

/// [`run_suites`] with the additive [`RunExtras`] hooks; see that struct for
/// why they are not `RunParams` fields.
pub fn run_suites_with(
    config: &Config,
    root: &Path,
    suites: &[Suite],
    params: &RunParams,
    extras: &RunExtras,
) -> Result<RunOutcome, ToolError> {
    let mut reporter = make_reporter(params.reporter, params.color, extras.report_to_stderr);
    let mut totals = Totals::default();
    // A run is "filtered" whenever it did not execute every test in every
    // discovered spec, which is what makes obsolete-key detection unsound —
    // see `SnapshotStore::new`.
    let filtered = params.name_filter.is_some() || params.only_specs.is_some();
    // Snapshot diffs are colored for the pretty terminal report only: json and
    // junit are machine documents, and an ANSI escape baked into a diff would
    // corrupt the JSON detail field and turn into U+FFFD in the XML.
    let diff_color = params.color && params.reporter == ReporterKind::Pretty;
    let mut store = SnapshotStore::new(params.update, diff_color, filtered);
    let started = Instant::now();

    let mut planned: Vec<(&Suite, Vec<PathBuf>)> = Vec::new();
    for suite in suites {
        let mut specs = match extras.pre_discovered {
            // The caller's discovery is authoritative — a suite absent from
            // the map has no specs this pass.
            Some(map) => map.get(&suite.name).cloned().unwrap_or_default(),
            None => discover::discover(root, &suite.include)?,
        };
        if let Some(only) = params.only_specs {
            specs.retain(|spec| only.contains(&crate::resolve::cache_key_path(spec)));
        }
        if specs.is_empty() {
            // An empty suite usually means a misconfigured glob — but only on
            // an unrestricted run; under a spec restriction or pre-discovered
            // hand-off it just means "nothing affected here".
            if params.only_specs.is_none() && extras.pre_discovered.is_none() {
                reporter.note(&format!(
                    "suite \"{}\": no files matched its include globs",
                    suite.name
                ));
            }
            continue;
        }
        planned.push((suite, specs));
    }

    if planned.is_empty() {
        return Ok(RunOutcome {
            totals,
            specs: 0,
            snapshots: SnapshotSummary::default(),
            coverage: None,
        });
    }

    let core_entry = find_core_entry(root, config)?;
    let mut specs_seen = 0usize;
    let mut cov_acc: Option<CoverageMap> = params.coverage.then(CoverageMap::new);
    let mut non_native_specs: Vec<PathBuf> = Vec::new();
    let mut fatal: Option<ToolError> = None;
    // Warnings always go to stderr. `params.color` describes the report
    // stream, so here it only proves color was not switched off; stderr must
    // additionally be a terminal of its own. (A piped report with a TTY stderr
    // thus prints plain warnings — conservative, never wrong.)
    let warn_color = params.color && std::io::stderr().is_terminal();

    for (suite, specs) in planned {
        specs_seen += specs.len();
        if params.coverage && suite.backend != BackendKind::Native {
            non_native_specs.extend(specs.iter().cloned());
        }
        reporter.begin_suite(&suite.name, &suite.backend.to_string());
        // Synthesized run_start for this suite (replaces each backend's own).
        // `spec_count` here is deliberately the number of spec *files*, not
        // tests, even though the protocol doc describes the framework's own
        // field as a test count: events stream as they happen, so the CLI
        // cannot know how many tests a file holds (or a filter keeps) before
        // running it — only core can, and its framing was just dropped.
        // Nothing consumes the field today; it is kept because it is a wire
        // field.
        reporter.on_event(&Event::RunStart {
            spec_count: specs.len() as u32,
            protocol_version: PROTOCOL_VERSION,
        });

        let plan = SuitePlan {
            name: suite.name.clone(),
            specs,
            root: root.to_path_buf(),
            core_entry: core_entry.clone(),
            timeout: config.timeout,
            workers: config.workers,
            name_filter: params.name_filter.map(str::to_string),
            coverage: params.coverage && suite.backend == BackendKind::Native,
            rojo_project: config.rojo.as_ref().map(|project| root.join(project)),
        };

        let mut suite_totals = Totals::default();
        let mut snapshot_err: Option<ToolError> = None;
        // Test identities, for reconciling host-side snapshot verdicts with
        // the streamed events below: a test whose snapshot mismatched has
        // already streamed (and been counted) as a pass, unless it also
        // failed on its own.
        let mut snapshot_failed_tests: HashSet<String> = HashSet::new();
        let mut failed_tests: HashSet<String> = HashSet::new();
        let failures_before_suite = store.failures.len();
        // Scoped so the sink's mutable borrows of `reporter`/`totals`/`store`
        // are released before the synthesized run_end below.
        let result = {
            let mut sink = |spec: Option<&Path>, event: &Event| {
                match event {
                    // Backend framing is dropped; the CLI supplies its own.
                    Event::RunStart { .. } | Event::RunEnd { .. } => return,
                    Event::Snapshot {
                        path,
                        name,
                        key,
                        received,
                    } => {
                        if let Some(spec) = spec {
                            let display = backend::display_rel(spec, root);
                            let failures_before = store.failures.len();
                            if let Err(err) = store.record(spec, &display, key, received) {
                                snapshot_err.get_or_insert(err);
                            }
                            if store.failures.len() > failures_before {
                                snapshot_failed_tests.insert(full_test_name(path, name));
                            }
                        } else {
                            // A snapshot with no spec attribution cannot be
                            // compared or stored; dropping it silently would
                            // leave the author believing it was checked.
                            eprint!(
                                "{}",
                                render_warning(
                                    &format!(
                                        "snapshot \"{key}\" from test \"{name}\" arrived without \
                                         a spec file attribution — it was not compared or stored"
                                    ),
                                    warn_color,
                                )
                            );
                        }
                    }
                    Event::TestFail { path, name, .. } => {
                        failed_tests.insert(full_test_name(path, name));
                    }
                    _ => {}
                }
                // Failures are polished for human eyes before any consumer
                // sees them: project paths become root-relative, lest's own
                // frames drop out of traces (core takes the traceback, so
                // `protectedCall`/`run` appeared in every single failure),
                // and mlua's error framing is normalized. Here rather than in
                // a backend so every backend and every reporter agree.
                let event: std::borrow::Cow<Event> = match event {
                    Event::TestFail { .. } => {
                        let mut polished = event.clone();
                        if let Event::TestFail { failure, .. } = &mut polished {
                            backend::polish_failure(failure, root, &core_entry);
                        }
                        std::borrow::Cow::Owned(polished)
                    }
                    _ => std::borrow::Cow::Borrowed(event),
                };
                let event = event.as_ref();
                suite_totals.record(event);
                totals.record(event);
                reporter.on_event(event);
            };

            match suite.backend {
                BackendKind::Native => backend::native::run(&plan, &mut sink, cov_acc.as_mut()),
                BackendKind::Lune => {
                    backend::runtime::run(crate::resolve::Runtime::Lune, &plan, &mut sink)
                }
                BackendKind::Lute => {
                    backend::runtime::run(crate::resolve::Runtime::Lute, &plan, &mut sink)
                }
                BackendKind::Cloud => backend::cloud::run(&plan, &suite.cloud, &mut sink),
            }
        };

        // Reconcile the verdicts the event stream could not carry, *before*
        // the synthesized run_end so every consumer sees the same counts.
        // Snapshot comparison is host-side: a test whose snapshot mismatched
        // streamed as a pass, and without this the summary said `passed`
        // beside exit 1. Tests that also failed on their own are already
        // counted and must not be double-flipped.
        let suite_snapshot_failures = store.failures.len() - failures_before_suite;
        if suite_snapshot_failures > 0 {
            let flipped = snapshot_failed_tests.difference(&failed_tests).count() as u32;
            suite_totals.passed = suite_totals.passed.saturating_sub(flipped);
            suite_totals.failed += flipped;
            totals.passed = totals.passed.saturating_sub(flipped);
            totals.failed += flipped;
            reporter.suite_snapshot_failures(suite_snapshot_failures);
        }
        // A dead backend emitted no test_fail, so without this the suite
        // counted as passed in the very summary printed above the fatal
        // error it caused.
        if let Err(err) = &result {
            reporter.suite_error(&format!("the suite did not finish: {err}"));
        }

        // Synthesized run_end for this suite.
        reporter.on_event(&Event::RunEnd {
            passed: suite_totals.passed,
            failed: suite_totals.failed,
            skipped: suite_totals.skipped,
        });

        if let Some(err) = snapshot_err {
            fatal = Some(err);
            break;
        }
        if let Err(err) = result {
            fatal = Some(err);
            break;
        }
    }

    // Surface snapshot mismatch diffs before the summary.
    for failure in &store.failures {
        reporter.snapshot_failure(&failure.spec, &failure.key, &failure.detail);
    }
    let snapshots = match store.finish() {
        Ok(summary) => summary,
        Err(err) => {
            fatal.get_or_insert(err);
            store.summary
        }
    };

    // The summary always runs, even on a mid-run tool error, so partial output
    // is never left without its footer.
    reporter.finish(&totals, &snapshots, started.elapsed());
    // The summary line counts obsolete snapshots; this says what to do about
    // them. Not under `-u` — that run just pruned them.
    if snapshots.obsolete > 0 && !params.update {
        reporter.note(&format!(
            "run `lest -u` to prune {} obsolete snapshot{}",
            snapshots.obsolete,
            if snapshots.obsolete == 1 { "" } else { "s" }
        ));
    }

    if let Some(err) = fatal {
        return Err(err);
    }

    let coverage = cov_acc.map(|acc| {
        coverage::build(
            root,
            &core_entry,
            &config.coverage.exclude,
            &acc,
            &non_native_specs,
        )
    });
    let coverage = match coverage {
        Some(Ok(data)) => Some(data),
        Some(Err(err)) => return Err(err),
        None => None,
    };

    Ok(RunOutcome {
        totals,
        specs: specs_seen,
        snapshots,
        coverage,
    })
}

/// Applies suite selection rules: named suites win; otherwise every suite
/// that is default-enabled, plus `default = false` suites when CI is
/// detected. A backend override applies to every selected suite.
pub fn select_suites(
    config: &Config,
    names: &[String],
    backend: Option<BackendKind>,
) -> Result<Vec<Suite>, ToolError> {
    let mut selected: Vec<Suite> = if names.is_empty() {
        let ci = is_ci();
        config
            .suites
            .iter()
            .filter(|suite| suite.default_enabled || ci)
            .cloned()
            .collect()
    } else {
        let mut picked = Vec::new();
        // Order-preserving dedupe: `lest run unit unit` means "run unit", not
        // "run it twice" — a repeated suite would execute every spec again and
        // trip the duplicate-snapshot-key guard with a message blaming the
        // tests for a slip on the command line.
        let mut requested: HashSet<&str> = HashSet::new();
        for name in names {
            if !requested.insert(name.as_str()) {
                continue;
            }
            let suite = config
                .suites
                .iter()
                .find(|suite| &suite.name == name)
                .ok_or_else(|| {
                    let available: Vec<&str> =
                        config.suites.iter().map(|s| s.name.as_str()).collect();
                    ToolError(format!(
                        "unknown suite \"{name}\" — available suites: {}",
                        available.join(", ")
                    ))
                })?;
            picked.push(suite.clone());
        }
        picked
    };

    if let Some(backend) = backend {
        for suite in &mut selected {
            suite.backend = backend;
        }
    }
    if selected.is_empty() {
        return Err(ToolError(
            "no suites selected — every configured suite has `default = false`; name one, e.g. \
             `lest run <suite>`"
                .to_string(),
        ));
    }
    Ok(selected)
}

fn is_ci() -> bool {
    match std::env::var("CI") {
        Ok(value) => !value.is_empty() && value != "0" && value.to_lowercase() != "false",
        Err(_) => false,
    }
}

/// Locates the lest/core the run should load. Normally that is the copy
/// embedded in this binary, written to `.lest/core` when missing or stale —
/// the framework ships with the CLI rather than being installed, so the two
/// can never be different versions. An explicit `[settings] core` opts out and
/// points at a directory on disk instead.
pub fn find_core_entry(root: &Path, config: &Config) -> Result<PathBuf, ToolError> {
    let Some(configured) = config.core.as_deref() else {
        return embed::ensure(root);
    };
    let base = root.join(configured);
    let candidates = [base.join("init.luau"), base.join("init.lua"), base.clone()];
    for candidate in candidates {
        if candidate.is_file() {
            return Ok(crate::resolve::normalize(&candidate));
        }
    }
    Err(ToolError(format!(
        "cannot find the lest/core framework at {} — `core` under [settings] in lest.toml points \
         there; remove that setting to use the framework built into this copy of lest",
        base.display()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Must match core's `fullName` exactly — snapshot verdicts are matched
    /// back to streamed test outcomes on this string.
    #[test]
    fn full_test_name_matches_core_identity() {
        assert_eq!(full_test_name(&[], "adds"), "adds");
        assert_eq!(
            full_test_name(&["math".to_string(), "add".to_string()], "adds"),
            "math add adds"
        );
    }

    fn outcome(specs: usize, totals: Totals) -> RunOutcome {
        RunOutcome {
            totals,
            specs,
            snapshots: SnapshotSummary::default(),
            coverage: None,
        }
    }

    /// The no-match predicate keys on spec files having run but no outcome
    /// arriving. A skip counts as an outcome — the filter matched, core just
    /// didn't execute the body — so it must not read as "matched nothing".
    #[test]
    fn ran_nothing_distinguishes_no_outcomes_from_no_specs() {
        assert!(outcome(3, Totals::default()).ran_nothing());
        // No specs at all is the include-glob error, reported separately.
        assert!(!outcome(0, Totals::default()).ran_nothing());

        for totals in [
            Totals {
                passed: 1,
                ..Default::default()
            },
            Totals {
                failed: 1,
                ..Default::default()
            },
            Totals {
                skipped: 1,
                ..Default::default()
            },
        ] {
            assert!(!outcome(3, totals).ran_nothing(), "{totals:?}");
        }
    }

    /// Tool errors wear the same bold red `Error:` label and sentence casing
    /// as clap's parse errors — the messages themselves stay lowercase
    /// fragments so watch mode can reuse them as notes. Bodies never end with
    /// a period.
    #[test]
    fn diagnostics_render_in_the_parse_error_voice() {
        let out = render_diagnostic(Severity::Error, &no_match_message("adds two"), true);
        assert_eq!(
            out,
            "\n\x1b[1;31mError:\x1b[0m No tests matched -t \"adds two\" — the filter is a plain \
             substring of the full name (describe path joined with spaces, then the test name)\n"
        );
        // Leading blank line separates the diagnostic from the reporter's summary.
        assert!(out.starts_with('\n'));
        // A message introducing a list keeps its colon.
        assert_eq!(
            render_diagnostic(
                Severity::Error,
                "unknown suite \"x\" — available suites:",
                false
            ),
            "\nError: Unknown suite \"x\" — available suites:\n"
        );
    }

    /// The label carries the exit class: exit 2 is lest's fault, exit 1 is the
    /// project's, and the reader can tell which from the first word.
    #[test]
    fn severity_labels_separate_the_two_exit_classes() {
        assert_eq!(
            render_diagnostic(
                Severity::Failure,
                "coverage 61.2% is below the required minimum 80.0%",
                false
            ),
            "\nFailure: Coverage 61.2% is below the required minimum 80.0%\n"
        );
        assert_eq!(Severity::Error.label(), "Error:");
        assert_eq!(Severity::Failure.label(), "Failure:");
    }

    /// Section labels are bold and uncolored, matching the reporters; color in
    /// this CLI means pass/fail, so clap's colored defaults stay off.
    #[test]
    fn help_labels_are_bold_and_uncolored() {
        let help = Cli::command()
            .styles(help_styles())
            .render_help()
            .ansi()
            .to_string();

        assert!(help.contains("\x1b[1mUsage:"));
        assert!(help.contains("\x1b[1mOptions:"));
        assert!(!help.contains("\x1b[4m")); // no underline
        assert!(!help.contains("\x1b[3")); // no foreground color on the help body
    }

    #[test]
    fn parse_errors_are_sentences_with_bold_labels() {
        let err = Cli::command()
            .styles(help_styles())
            .try_get_matches_from(["lest", "--r"])
            .unwrap_err();
        let out = restyle_parse_error(&err.render().to_string(), true);

        assert!(out.contains("\x1b[1;31mError:\x1b[0m Unexpected argument '--r' found\n"));
        assert!(out.contains("\n  A similar argument exists: '--version'\n"));
        assert!(out.contains("\x1b[1mUsage:\x1b[0m lest"));
        assert!(!out.contains("tip:"));
        // Structural lines are left alone — no stray period after a list.
        let values = restyle_parse_error("  [possible values: a, b]", false);
        assert_eq!(values, "  [possible values: a, b]\n");
    }

    /// `bin_name` is pinned, so usage reads `lest` on every platform rather than
    /// argv[0]'s `lest.exe` on Windows.
    #[test]
    fn usage_never_names_the_exe() {
        let help = Cli::command().render_help().to_string();
        assert!(help.contains("Usage: lest [OPTIONS]"));
    }
}
