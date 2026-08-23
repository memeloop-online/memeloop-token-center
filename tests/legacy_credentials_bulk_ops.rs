use std::{path::PathBuf, process::Command};

#[test]
fn legacy_credential_bulk_operator_tests_pass() {
    let repository = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let suite = repository.join("tests/ops/test-legacy-credentials-bulk.ts");
    let output = Command::new("node")
        .arg("--test")
        .arg(suite)
        .output()
        .expect("run TypeScript operator tests");
    assert!(
        output.status.success(),
        "legacy credential operator suite failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
