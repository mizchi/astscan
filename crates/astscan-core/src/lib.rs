#![doc = include_str!("../README.md")]

mod config;
mod eval;
mod fastparse;
mod found;
mod lang;
mod rule;
mod scan;
mod simd;
mod tree;

pub use config::{RuleInfo, RuleSet, Severity, load_sgconfig};
pub use lang::{CustomDecl, Registry};
pub use scan::{Match, ParserKind, ScanError, Scanner};

/// Low-level interfaces for the companion CLI and parser experiments.
#[cfg(feature = "internals")]
#[doc(hidden)]
pub mod internals {
  pub use crate::config::plan_rules;
  pub use crate::fastparse::{Bail, Parser as FastParser};
  pub use crate::found::{Found, Span};
  pub use crate::lang::{HTML, INJECTABLE};
  pub use crate::scan::{ScanOptions, ScanStats, Target};
  pub use crate::tree::{FlatTree, first_difference};
}
