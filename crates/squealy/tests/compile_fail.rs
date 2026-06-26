#[test]
fn compile_fail_cases() {
    let tests = trybuild::TestCases::new();
    tests.compile_fail("tests/compile_fail/*.rs");

    // Feature-specific fixtures live in `feature_gated/` (excluded from the glob above). Their expected
    // stderr depends on which feature impls are in scope — rustc lists the gated `TimestampKind` /
    // `AggregateScalar` / `HasColumnType` impls in its "the following other types implement …" help
    // (and its trailing "and N others" count) only when the corresponding feature is enabled. Any
    // feature that adds such an impl — the timestamp bridges, `uuid`, and `bytes` — perturbs that help,
    // so the fixtures are blessed for, and asserted only under, the default feature set. This keeps the
    // default and `--all-features` runs green, as well as any single-feature run.
    #[cfg(not(any(
        feature = "systemtime",
        feature = "time",
        feature = "chrono",
        feature = "uuid",
        feature = "bytes"
    )))]
    tests.compile_fail("tests/compile_fail/feature_gated/*.rs");
}
