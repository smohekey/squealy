//! Compile-fail coverage for the `#[derive(Table)]` macro's error reporting.
//!
//! Each case under `tests/compile_fail/` triggers one of the macro's
//! `compile_error!` paths; the committed `.stderr` files pin the exact message
//! so a regression in the diagnostics is caught.

#[test]
fn compile_fail_cases() {
    let tests = trybuild::TestCases::new();
    tests.compile_fail("tests/compile_fail/*.rs");
}

#[test]
fn compile_pass_cases() {
    let tests = trybuild::TestCases::new();
    tests.pass("tests/pass/*.rs");
}
