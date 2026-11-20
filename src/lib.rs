//! astscan: run many ast-grep rules over a source tree in one pass.

pub mod cache;
pub mod config;
pub mod eval;
pub mod fastparse;
pub mod lang;
pub mod output;
pub mod record;
pub mod rule;
pub mod scan;
pub mod simd;
pub mod tree;
