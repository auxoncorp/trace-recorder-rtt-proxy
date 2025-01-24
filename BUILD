load("@rules_rust//rust:defs.bzl", "rust_binary")
load("@rtt-proxy-crates//:defs.bzl", "all_crate_deps")

rust_binary(
    name = "trc-rtt-proxy",
    srcs = glob([
        "src/*.rs",
    ]),
    deps = all_crate_deps(normal = True) + [
        "//rtt-proxy:rtt-proxy",
    ],
    visibility = ["//visibility:public"],
)

rust_binary(
    name = "trc-rtt-proxy-client",
    srcs = glob([
        "examples/*.rs",
    ]),
    deps = all_crate_deps(normal = True) + [
        "//rtt-proxy:rtt-proxy",
        # must include dev-dependencies explicitly for examples
        "@rtt-proxy-crates//:humantime",
        "@rtt-proxy-crates//:url",
        "@rtt-proxy-crates//:clap-num",
        "@rtt-proxy-crates//:goblin",
    ],
    visibility = ["//visibility:public"],
)
