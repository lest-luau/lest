//! Result protocol schema and reporters for lest.
//!
//! The protocol is the contract between the framework (`lest/core`, Luau) and
//! the CLI: the framework reports facts as events, the host decides what they
//! mean. Everything in [`Event`] is JSON-safe by rule because out-of-process
//! backends ship it as JSON lines.

mod coverage;
mod diagnostic;
mod event;
mod json;
mod junit;
mod pretty;
mod snapshot;

pub use coverage::{CoverageData, FileCoverage};
// The diagnostic voice — the `Error:`/`Failure:`/`Warning:` labels — lives
// beside the palette so the two-voice rule (see `pretty.rs`) is enforced in
// one module. `render_warning` is the crate-wide way to emit a warning:
// callers `eprint!` the result (warnings always belong on stderr).
pub use diagnostic::{render_diagnostic, render_warning, sentence, Severity};
pub use event::{check_protocol_version, Event, Failure, Totals, PROTOCOL_VERSION};
pub use json::Json;
pub use junit::Junit;
// `paint`/`BOLD`/`BOLD_RED` escape the reporters so the CLI's own output —
// clap's help and parse errors — can wear the same palette.
pub use pretty::{paint, Pretty, BOLD, BOLD_RED};
pub use snapshot::{diff, parse_snapshots, serialize_snapshots, SnapshotFile, SnapshotSummary};

// Error types named in the signatures above. Callers only ever `{e}` them, so
// nothing imports them today — re-exported anyway so a caller that wants to
// match on one does not have to reach past this module's surface.
#[allow(unused_imports)]
pub use event::ProtocolVersionMismatch;
#[allow(unused_imports)]
pub use snapshot::SnapshotParseError;
