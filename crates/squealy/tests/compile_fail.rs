#[test]
fn compile_fail_cases() {
    let tests = trybuild::TestCases::new();
    tests.compile_fail("tests/compile_fail/*.rs");

    // Feature-specific fixtures live in `feature_gated/` (excluded from the glob above). Their expected
    // stderr depends on which feature impls are in scope — e.g. rustc lists the `TimestampKind` impls
    // only when a timestamp feature is enabled — so each is asserted only under the feature set it was
    // blessed for, keeping both the default and `--all-features` test runs green.
    #[cfg(not(any(feature = "systemtime", feature = "time", feature = "chrono")))]
    tests.compile_fail("tests/compile_fail/feature_gated/extract_requires_timestamp_operand.rs");
}
