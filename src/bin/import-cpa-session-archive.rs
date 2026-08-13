use std::path::PathBuf;

use clap::Parser;
use memeloop_token_center::{
    archive::ArchiveStore,
    config::Config,
    db::Database,
    session_archive_import::{SessionArchiveImportOptions, import_session_archive},
};

#[derive(Debug, Parser)]
#[command(about = "Import a lossless cpa-session-archive JSONL export")]
struct Args {
    #[arg(long, env = "CPA_SESSION_ARCHIVE_INPUT")]
    input: PathBuf,
    #[arg(
        long,
        env = "IMPORT_TENANT_EXTERNAL_ID",
        default_value = "cpa-dogfood-import"
    )]
    tenant_external_id: String,
    #[arg(
        long,
        env = "CPAMP_IMPORT_SOURCE",
        default_value = "cpamp-usage-events-v1"
    )]
    cpamp_source: String,
    #[arg(
        long,
        env = "SESSION_ARCHIVE_IMPORT_SOURCE",
        default_value = "cpa-session-archive-v2"
    )]
    archive_source: String,
    #[arg(long, env = "SESSION_ARCHIVE_OVERLAP_MS", default_value_t = 86_400_000)]
    overlap_ms: i64,
    #[arg(
        long,
        env = "SESSION_ARCHIVE_TIME_TOLERANCE_MS",
        default_value_t = 300_000
    )]
    time_tolerance_ms: i64,
    #[arg(
        long,
        env = "SESSION_ARCHIVE_MAX_LINE_BYTES",
        default_value_t = 134_217_728
    )]
    max_line_bytes: usize,
    #[arg(
        long,
        env = "SESSION_ARCHIVE_ALLOW_UNMAPPED",
        default_value_t = false,
        action = clap::ArgAction::Set
    )]
    allow_unmapped: bool,
    /// Perform writes. Without this flag the command is a fail-closed dry run.
    #[arg(long)]
    apply: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args = Args::parse();
    let config = Config::from_env()?;
    let db = Database::connect_with_max(&config.database_url, 2).await?;
    db.migrate().await?;
    let archive = ArchiveStore::from_config(&config).await?;
    let stats = import_session_archive(
        &db,
        &archive,
        &SessionArchiveImportOptions {
            input: &args.input,
            tenant_external_id: &args.tenant_external_id,
            cpamp_source: &args.cpamp_source,
            archive_source: &args.archive_source,
            overlap_ms: args.overlap_ms,
            time_tolerance_ms: args.time_tolerance_ms,
            max_line_bytes: args.max_line_bytes,
            allow_unmapped: args.allow_unmapped,
            apply: args.apply,
        },
    )
    .await?;
    println!("{}", serde_json::to_string(&stats)?);
    Ok(())
}
