use std::{path::PathBuf, process::Command};

#[test]
fn legacy_credential_bulk_operator_tests_pass() {
    let repository = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let suite = repository.join("tests/ops/test_legacy_credentials_bulk.py");
    let output = Command::new("python3")
        .arg(suite)
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .output()
        .expect("run Python standard-library operator tests");
    assert!(
        output.status.success(),
        "legacy credential operator suite failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
