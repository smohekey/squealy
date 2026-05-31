#[test]
fn compile_fail_cases() {
    let tests = trybuild::TestCases::new();
    tests.compile_fail("tests/compile_fail/*.rs");
}
