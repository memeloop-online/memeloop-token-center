use std::process::Command;

const DATABASE_CANARY: &str = "MTC_CANARY_DATABASE_PASSWORD_7f6ddbbc";

#[test]
fn server_migration_stderr_never_contains_database_configuration() {
    let output = Command::new(env!("CARGO_BIN_EXE_memeloop-token-center"))
        .arg("migrate")
        .env("MTC_KEY_PEPPER", "k".repeat(32))
        .env("MTC_SERVICE_TOKEN", "s".repeat(32))
        .env(
            "MTC_DATABASE_URL",
            format!("postgres://user:{DATABASE_CANARY}@[invalid/database"),
        )
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stderr.contains(DATABASE_CANARY), "{stderr}");
    assert!(!stdout.contains(DATABASE_CANARY), "{stdout}");
    assert!(
        stderr.contains("database_connect_failed") || stdout.contains("database_connect_failed"),
        "stdout={stdout}; stderr={stderr}"
    );
}

#[test]
fn session_importer_stderr_never_contains_database_configuration() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("empty.jsonl");
    std::fs::write(&input, b"").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_import-cpa-session-archive"))
        .arg("--input")
        .arg(&input)
        .arg("--plan-directory")
        .arg(directory.path())
        .env(
            "MTC_DATABASE_URL",
            format!("postgres://user:{DATABASE_CANARY}@[invalid/database"),
        )
        .env("MTC_ARCHIVE_BACKEND", "filesystem")
        .env("MTC_ARCHIVE_PATH", directory.path())
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains(DATABASE_CANARY), "{stderr}");
    assert!(stderr.contains("database_connect_failed"), "{stderr}");
}
