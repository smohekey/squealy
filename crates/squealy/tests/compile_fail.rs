#[test]
fn compile_fail_cases() {
    let tests = trybuild::TestCases::new();
    tests.compile_fail("tests/compile_fail/*.rs");

    // Feature-specific fixtures live in `feature_gated/` (excluded from the glob above). Their expected
    // stderr depends on which feature impls are in scope — rustc lists the gated `TimestampKind` /
    // `AggregateScalar` / `HasColumnType` impls in its "the following other types implement …" help
    // only when a timestamp feature is enabled. They are blessed for, and asserted only under, the
    // default feature set, so both the default and `--all-features` test runs stay green.
    #[cfg(not(any(feature = "systemtime", feature = "time", feature = "chrono")))]
    tests.compile_fail("tests/compile_fail/feature_gated/*.rs");
}
