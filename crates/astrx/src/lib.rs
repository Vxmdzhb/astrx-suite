//! `astrx`: one binary for the whole suite.
//!
//! The suite is five engines (`gitweb`, `onioncrawler`, `websearch`,
//! `torrentds`, `suitedash`). Each still builds its own binary too, since unit
//! files, container commands and runbooks name them. This crate adds one
//! entrypoint on top:
//!
//! ```text
//! astrx gitweb --root /repos --port 8801
//! astrx websearch crawl --db /data/web.db --seeds /data/seeds.txt
//! astrx doctor --db-dir /data --repo-root /repos
//! ```
//!
//! Nothing is reimplemented. [`dispatch`] looks at exactly one token, the
//! engine name, and hands the rest of the command line to that engine's
//! `cli::run`, the same function its own binary calls. So `astrx websearch
//! crawl …` and `websearch crawl …` share one parser, one help text and one
//! set of exit codes.
//!
//! [`doctor`] is the new part: cross-engine diagnostics in one command with
//! one exit code.
//!
//! [`supervise`] (`astrx run`) starts several engine roles as children of
//! this binary and keeps them running, so one container can hold every role
//! that shares its network.
//!
//! No third-party dependencies of its own: it depends on the five engines and
//! `crawlcore`, all first-party.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

// Both modules are behind `net`, because that is the tier in which the engines
// have a `cli` module to dispatch to at all, and because `doctor` opens sockets
// to probe ports and the Tor proxy. Without `net` this crate is deliberately
// empty rather than half-working: an `astrx` that could dispatch to some engines
// but not others would be worse than none, since an operator cannot tell from
// the binary's name which build they are holding.
#[cfg(feature = "net")]
pub mod dispatch;
#[cfg(feature = "net")]
pub mod doctor;
#[cfg(feature = "net")]
pub mod supervise;

#[cfg(feature = "net")]
pub use dispatch::{run, ENGINES};
