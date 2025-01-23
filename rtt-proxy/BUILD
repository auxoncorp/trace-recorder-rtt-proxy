load("@rules_rust//rust:defs.bzl", "rust_library")
load("@crates//:defs.bzl", "all_crate_deps", "aliases")

package(default_visibility = ["//visibility:public"])

rust_library(
  name = "rtt-proxy",
  aliases = aliases(),
  deps = all_crate_deps(normal = True),
  proc_macro_deps = all_crate_deps(proc_macro = True),
  srcs = ["src/lib.rs"],
)
