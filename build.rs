//! Compiles `csrc/flatten.c` against tree-sitter's private headers, which the
//! `tree-sitter` crate ships beside the public one it exports the path of.

use std::path::PathBuf;

fn main() {
  println!("cargo:rerun-if-changed=csrc/flatten.c");
  let include = PathBuf::from(std::env::var("DEP_TREE_SITTER_INCLUDE").expect("the tree-sitter crate exports its include path"));
  let src = include.parent().expect("include/ sits in the crate root").join("src");
  assert!(src.join("subtree.h").exists(), "tree-sitter's sources were expected at {}", src.display());
  cc::Build::new()
    .std("c11")
    .include(&include)
    .include(&src)
    .define("_POSIX_C_SOURCE", "200112L")
    .define("_DEFAULT_SOURCE", None)
    .define("_DARWIN_C_SOURCE", None)
    .flag_if_supported("-fvisibility=hidden")
    .warnings(false)
    .file("csrc/flatten.c")
    .compile("astscan_flatten");
}
