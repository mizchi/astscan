#![doc = include_str!("../README.md")]

mod config;
mod cypher;
mod eval;
mod fastparse;
mod found;
mod lang;
mod prefilter;
mod rule;
mod scan;
mod simd;
mod tree;

pub use config::{RuleInfo, RuleSet, Severity, load_sgconfig};
pub use lang::{CustomDecl, Registry};
pub use scan::{LiteralPrefilter, Match, ParserKind, ScanError, ScanOptimizations, Scanner};

/// Low-level interfaces for the companion CLI and parser experiments.
#[cfg(feature = "internals")]
#[doc(hidden)]
pub mod internals {
  pub use crate::config::{plan_rules, plan_yaml};
  pub use crate::fastparse::{Bail, Parser as FastParser};
  pub use crate::found::{Found, Span};
  pub use crate::lang::{HTML, INJECTABLE};
  pub use crate::scan::{PrefilterStats, ScanOptions, ScanStats, Target};
  pub use crate::tree::{FlatTree, first_difference};
}
