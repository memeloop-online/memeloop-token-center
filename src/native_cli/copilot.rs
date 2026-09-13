//! Native Copilot CLI stdio protocol, pinned to official SDK v1.0.8 / protocol 3.
//!
//! No legacy runtime, URL, token exchange, shell, or user-selected RPC method is
//! involved. The process supervisor owns the official binary and sealed account
//! home. This module deliberately does not turn missing provider usage into zero.

use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use uuid::Uuid;

const MAX_HEADER: usize = 1024;
const MAX_FRAME: usize = 4 * 1024 * 1024;
const MAX_PROMPT: usize = 1024 * 1024;
const MAX_EVENTS: usize = 16_384;

pub const RUNTIME_ARGUMENTS: [&str; 3] = ["--headless", "--no-auto-update", "--stdio"];
pub const RUNTIME_VERSION: &str = "1.0.73";
pub const RUNTIME_ARCHIVE_SHA512: &str = "937dd722bebf3d5a7e2bee45ff3bf7368e0f3da3489af1f3ef799c6c8c3ade8c61e6289a7178e0af45aa6c1212e6cfeb9213968cd3c891ba1ffd34cf67b9908c";
const RUNTIME_BINARY: &str = "/opt/copilot/copilot";

/// A dedicated sandbox worker constructs this only after authenticating and
/// provisioning the tenant-bound state carrier. Never inside the gateway pod.
pub struct CopilotRuntime {
    home: std::path::PathBuf,
    workspace: std::path::PathBuf,
}

impl CopilotRuntime {
    pub fn new(
        home: std::path::PathBuf,
        workspace: std::path::PathBuf,
    ) -> Result<Self, super::process::RuntimeError> {
        if !home.is_absolute() || !workspace.is_absolute() || home == workspace {
            return Err(super::process::RuntimeError::Configuration);
        }
        Ok(Self { home, workspace })
    }

    fn command(&self) -> super::process::RuntimeCommand {
        super::process::RuntimeCommand {
            executable: RUNTIME_BINARY.into(),
            arguments: RUNTIME_ARGUMENTS
                .iter()
                .map(std::ffi::OsString::from)
                .collect(),
            home: self.home.clone(),
            workspace: self.workspace.clone(),
            input: vec![],
            timeout: std::time::Duration::from_secs(170),
        }
    }

    pub async fn generate<F, Fut>(
        &self,
        model: &str,
        prompt: &str,
        emit: F,
    ) -> Result<Usage, super::process::RuntimeError>
    where
        F: FnMut(Output) -> Fut,
        Fut: std::future::Future<Output = Result<(), ProtocolError>>,
    {
        // Reject unsupported/oversized input before any supplier process starts.
        create_session(Uuid::nil(), model).map_err(|_| super::process::RuntimeError::Protocol)?;
        send_prompt(Uuid::nil(), prompt).map_err(|_| super::process::RuntimeError::Protocol)?;
        super::process::run_interactive(self.command(), |mut stdin, mut stdout| async move {
            execute(&mut stdout, &mut stdin, model, prompt, emit)
                .await
                .map_err(|error| match error {
                    ProtocolError::MissingUsage => super::process::RuntimeError::UsageUnavailable,
                    ProtocolError::Limit => super::process::RuntimeError::OutputLimit,
                    _ => super::process::RuntimeError::Protocol,
                })
        })
        .await
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolError {
    Io,
    InvalidFrame,
    Limit,
    Version,
    UnexpectedResponse,
    ProviderFailure,
    MissingUsage,
}

/// Errors are stable categories only: never include a frame, provider message,
/// prompt, stdout, credentials, or stderr in logs/API responses.
impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "native Copilot protocol: {self:?}")
    }
}

impl std::error::Error for ProtocolError {}

pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Value, ProtocolError> {
    let mut header = Vec::with_capacity(64);
    while !header.ends_with(b"\r\n\r\n") {
        if header.len() == MAX_HEADER {
            return Err(ProtocolError::Limit);
        }
        header.push(reader.read_u8().await.map_err(|_| ProtocolError::Io)?);
    }
    let header = std::str::from_utf8(&header).map_err(|_| ProtocolError::InvalidFrame)?;
    let mut length = None;
    for line in header[..header.len() - 4].split("\r\n") {
        let (name, value) = line.split_once(':').ok_or(ProtocolError::InvalidFrame)?;
        if name.eq_ignore_ascii_case("Content-Length") {
            if length.is_some() {
                return Err(ProtocolError::InvalidFrame);
            }
            let value = value.trim();
            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(ProtocolError::InvalidFrame);
            }
            length = Some(value.parse::<usize>().map_err(|_| ProtocolError::Limit)?);
        } else if !name.eq_ignore_ascii_case("Content-Type") {
            return Err(ProtocolError::InvalidFrame);
        }
    }
    let length = length.ok_or(ProtocolError::InvalidFrame)?;
    if length == 0 || length > MAX_FRAME {
        return Err(ProtocolError::Limit);
    }
    let mut body = vec![0; length];
    reader
        .read_exact(&mut body)
        .await
        .map_err(|_| ProtocolError::Io)?;
    let value: Value = serde_json::from_slice(&body).map_err(|_| ProtocolError::InvalidFrame)?;
    if value.get("jsonrpc").and_then(Value::as_str) != Some("2.0") || !value.is_object() {
        return Err(ProtocolError::InvalidFrame);
    }
    Ok(value)
}

pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    message: &Value,
) -> Result<(), ProtocolError> {
    let body = serde_json::to_vec(message).map_err(|_| ProtocolError::InvalidFrame)?;
    if body.len() > MAX_FRAME {
        return Err(ProtocolError::Limit);
    }
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    writer
        .write_all(header.as_bytes())
        .await
        .map_err(|_| ProtocolError::Io)?;
    writer
        .write_all(&body)
        .await
        .map_err(|_| ProtocolError::Io)?;
    writer.flush().await.map_err(|_| ProtocolError::Io)
}

fn rpc(id: u64, method: &'static str, params: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
}

/// Server-owned operation set. A client cannot request arbitrary SDK methods,
/// tools, MCP servers, plugins, filesystem callbacks, or login commands.
pub fn connect() -> Value {
    rpc(1, "connect", json!({}))
}

pub fn validate_connect(response: &Value) -> Result<(), ProtocolError> {
    let result = response_result(response, 1)?;
    if result.get("protocolVersion").and_then(Value::as_u64) != Some(3) {
        return Err(ProtocolError::Version);
    }
    Ok(())
}

pub fn list_models() -> Value {
    rpc(2, "models.list", json!({}))
}

pub fn quota_snapshot() -> Value {
    // Official SDK RPC.Account.GetQuota is read-only; never prepare/reset/consume.
    rpc(3, "account.getQuota", json!({}))
}

pub fn create_session(session: Uuid, model: &str) -> Result<Value, ProtocolError> {
    if model.is_empty()
        || model.len() > 256
        || !model
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._:/-".contains(&byte))
    {
        return Err(ProtocolError::InvalidFrame);
    }
    Ok(rpc(
        4,
        "session.create",
        json!({
            "sessionId":session.to_string(),
            "clientName":"memeloop-token-center",
            "model":model,
            "streaming":true,
            "tools":[],
            "availableTools":[],
            "skipCustomInstructions":true,
            "enableConfigDiscovery":false,
            "enableFileHooks":false,
            "enableHostGitOperations":false,
            "enableSkills":false,
            "enableOnDemandInstructionDiscovery":false,
            "enableSessionTelemetry":false,
            "requestPermission":true,
            "mcpServers":{},
            "skillDirectories":[],
            "pluginDirectories":[],
            "instructionDirectories":[],
            "customAgents":[]
        }),
    ))
}

pub fn send_prompt(session: Uuid, prompt: &str) -> Result<Value, ProtocolError> {
    if prompt.is_empty() || prompt.len() > MAX_PROMPT {
        return Err(ProtocolError::Limit);
    }
    Ok(rpc(
        5,
        "session.send",
        json!({"sessionId":session.to_string(),"prompt":prompt}),
    ))
}

pub fn abort(session: Uuid) -> Value {
    rpc(6, "session.abort", json!({"sessionId":session.to_string()}))
}

pub fn destroy(session: Uuid) -> Value {
    rpc(
        7,
        "session.destroy",
        json!({"sessionId":session.to_string()}),
    )
}

pub fn response_result(response: &Value, id: u64) -> Result<&Value, ProtocolError> {
    if response.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
        || response.get("id").and_then(Value::as_u64) != Some(id)
        || response.get("method").is_some()
    {
        return Err(ProtocolError::UnexpectedResponse);
    }
    if response.get("error").is_some() {
        return Err(ProtocolError::ProviderFailure);
    }
    response
        .get("result")
        .filter(|value| value.is_object())
        .ok_or(ProtocolError::UnexpectedResponse)
}

/// Any unsolicited runtime request is rejected, even if tools were disabled.
/// The runner must never execute filesystem, permission, input or tool callbacks.
pub fn reject_runtime_request(message: &Value) -> Result<Value, ProtocolError> {
    let id = message
        .get("id")
        .filter(|id| id.is_string() || id.is_u64())
        .ok_or(ProtocolError::InvalidFrame)?;
    if message.get("method").and_then(Value::as_str).is_none() {
        return Err(ProtocolError::InvalidFrame);
    }
    Ok(json!({
        "jsonrpc":"2.0","id":id,
        "error":{"code":-32601,"message":"runtime callbacks are disabled"}
    }))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
}

/// Delivered to the existing gateway archive/stream owner, never stdout/logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Output {
    Delta(String),
    Message(String),
    Completed(Usage),
}

async fn exchange<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    writer: &mut W,
    request: &Value,
    session: Option<Uuid>,
) -> Result<Value, ProtocolError> {
    let id = request["id"]
        .as_u64()
        .ok_or(ProtocolError::UnexpectedResponse)?;
    write_frame(writer, request).await?;
    for _ in 0..128 {
        let response = read_frame(reader).await?;
        if response.get("id").is_some() {
            return response_result(&response, id).cloned();
        }
        // Creation may deliver lifecycle metadata before its response. Only
        // known metadata for the pre-minted session is accepted here.
        if response["method"] != "session.event"
            || session.is_none_or(|id| response["params"]["sessionId"] != id.to_string())
            || !matches!(
                response["params"]["event"]["type"].as_str(),
                Some("session.start" | "session.info" | "session.model_change")
            )
        {
            return Err(ProtocolError::UnexpectedResponse);
        }
    }
    Err(ProtocolError::Limit)
}

/// Drive one native, no-tools request over a supervised official runtime.
///
/// The caller supplies *owned child stdio*, not a network stream, holds the
/// tenant/account lease, and applies the process deadline/cancellation guard.
/// The callback must durably acknowledge archive writes before returning.
/// No completion is emitted without one authoritative usage event. Multi-call
/// agent activity is rejected until an explicitly tested billing contract exists.
pub async fn execute<R, W, F, Fut>(
    reader: &mut R,
    writer: &mut W,
    model: &str,
    prompt: &str,
    mut emit: F,
) -> Result<Usage, ProtocolError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    F: FnMut(Output) -> Fut,
    Fut: std::future::Future<Output = Result<(), ProtocolError>>,
{
    let mut reader = reader.take(16 * 1024 * 1024);
    let session = Uuid::now_v7();
    let create = create_session(session, model)?;
    let send = send_prompt(session, prompt)?;
    let connected = exchange(&mut reader, writer, &connect(), None).await?;
    if connected["protocolVersion"].as_u64() != Some(3) {
        return Err(ProtocolError::Version);
    }
    let created = exchange(&mut reader, writer, &create, Some(session)).await?;
    if created["sessionId"] != session.to_string() {
        return Err(ProtocolError::UnexpectedResponse);
    }
    write_frame(writer, &send).await?;
    let mut sent = false;
    let mut usage = None;
    let mut has_message = false;
    let mut output_bytes = 0usize;
    let mut event_ids = std::collections::HashSet::new();
    for _ in 0..MAX_EVENTS {
        let message = read_frame(&mut reader).await?;
        if message.get("id").is_some() {
            if message.get("method").is_some() {
                write_frame(writer, &reject_runtime_request(&message)?).await?;
                return Err(ProtocolError::ProviderFailure);
            }
            if sent {
                return Err(ProtocolError::UnexpectedResponse);
            }
            response_result(&message, 5)?;
            sent = true;
            continue;
        }
        if message["method"] != "session.event"
            || message["params"]["sessionId"] != session.to_string()
        {
            return Err(ProtocolError::UnexpectedResponse);
        }
        let event = &message["params"]["event"];
        if event.get("agentId").is_some_and(|value| !value.is_null())
            || event["data"]
                .get("parentToolCallId")
                .is_some_and(|value| !value.is_null())
        {
            return Err(ProtocolError::ProviderFailure);
        }
        let event_id = event["id"]
            .as_str()
            .filter(|id| Uuid::parse_str(id).is_ok())
            .ok_or(ProtocolError::InvalidFrame)?;
        if !event_ids.insert(event_id.to_owned()) {
            return Err(ProtocolError::UnexpectedResponse);
        }
        let data = &event["data"];
        match event["type"].as_str() {
            Some("assistant.message_delta" | "assistant.message") => {
                if data["toolRequests"]
                    .as_array()
                    .is_some_and(|tools| !tools.is_empty())
                {
                    return Err(ProtocolError::ProviderFailure);
                }
                let final_message = event["type"] == "assistant.message";
                let field = if final_message {
                    "content"
                } else {
                    "deltaContent"
                };
                let content = data[field].as_str().ok_or(ProtocolError::InvalidFrame)?;
                output_bytes = output_bytes
                    .checked_add(content.len())
                    .ok_or(ProtocolError::Limit)?;
                if output_bytes > MAX_FRAME {
                    return Err(ProtocolError::Limit);
                }
                if final_message {
                    if has_message {
                        return Err(ProtocolError::UnexpectedResponse);
                    }
                    has_message = true;
                    emit(Output::Message(content.to_owned())).await?;
                } else {
                    emit(Output::Delta(content.to_owned())).await?;
                }
            }
            Some("assistant.usage") => {
                if usage.is_some() || data["model"] != model {
                    return Err(ProtocolError::UnexpectedResponse);
                }
                let required =
                    |field: &str| data[field].as_u64().ok_or(ProtocolError::MissingUsage);
                let optional = |field: &str| {
                    data.get(field)
                        .filter(|value| !value.is_null())
                        .map(|value| value.as_u64().ok_or(ProtocolError::MissingUsage))
                        .transpose()
                };
                usage = Some(Usage {
                    input_tokens: required("inputTokens")?,
                    output_tokens: required("outputTokens")?,
                    cache_read_tokens: optional("cacheReadTokens")?,
                    cache_write_tokens: optional("cacheWriteTokens")?,
                });
            }
            Some("session.idle") => {
                if !sent || !has_message {
                    return Err(ProtocolError::UnexpectedResponse);
                }
                let usage = usage.ok_or(ProtocolError::MissingUsage)?;
                emit(Output::Completed(usage.clone())).await?;
                return Ok(usage);
            }
            Some(
                "session.start"
                | "session.info"
                | "user.message"
                | "assistant.turn_start"
                | "assistant.turn_end"
                | "assistant.message_start"
                | "session.usage_info",
            ) => {}
            // Includes tool execution, permission, subagent, abort, provider
            // error and unknown protocol variants. Never execute callbacks.
            _ => return Err(ProtocolError::ProviderFailure),
        }
    }
    Err(ProtocolError::Limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn mock_execution(with_usage: bool) -> Result<Usage, ProtocolError> {
        let (client, server) = tokio::io::duplex(16 * 1024);
        let server_task = tokio::spawn(async move {
            let (mut reader, mut writer) = tokio::io::split(server);
            let request = read_frame(&mut reader).await.unwrap();
            assert_eq!(request["method"], "connect");
            write_frame(
                &mut writer,
                &json!({"jsonrpc":"2.0","id":1,"result":{"protocolVersion":3}}),
            )
            .await
            .unwrap();
            let request = read_frame(&mut reader).await.unwrap();
            let session = request["params"]["sessionId"].clone();
            assert_eq!(request["params"]["availableTools"], json!([]));
            write_frame(
                &mut writer,
                &json!({"jsonrpc":"2.0","id":4,"result":{"sessionId":session}}),
            )
            .await
            .unwrap();
            let request = read_frame(&mut reader).await.unwrap();
            assert_eq!(request["method"], "session.send");
            write_frame(
                &mut writer,
                &json!({"jsonrpc":"2.0","id":5,"result":{"messageId":Uuid::now_v7().to_string()}}),
            )
            .await
            .unwrap();
            let mut events = vec![
                ("assistant.message_delta", json!({"deltaContent":"hello"})),
                ("assistant.message", json!({"content":"hello"})),
            ];
            if with_usage {
                events.push((
                    "assistant.usage",
                    json!({
                        "model":"gpt-5.6","inputTokens":12,"outputTokens":2,"cacheReadTokens":0
                    }),
                ));
            }
            events.push(("session.idle", json!({})));
            for (kind, data) in events {
                write_frame(
                    &mut writer,
                    &json!({
                        "jsonrpc":"2.0","method":"session.event",
                        "params":{"sessionId":session,"event":{
                            "id":Uuid::now_v7().to_string(),"type":kind,"data":data
                        }}
                    }),
                )
                .await
                .unwrap();
            }
        });
        let (mut reader, mut writer) = tokio::io::split(client);
        let result = execute(&mut reader, &mut writer, "gpt-5.6", "hello", |_| async {
            Ok(())
        })
        .await;
        server_task.await.unwrap();
        result
    }

    #[tokio::test]
    async fn native_request_requires_authoritative_usage_before_completion() {
        assert_eq!(
            mock_execution(true).await.unwrap(),
            Usage {
                input_tokens: 12,
                output_tokens: 2,
                cache_read_tokens: Some(0),
                cache_write_tokens: None,
            }
        );
        assert_eq!(
            mock_execution(false).await,
            Err(ProtocolError::MissingUsage)
        );
    }

    #[tokio::test]
    async fn official_content_length_frames_round_trip_without_line_assumptions() {
        let message = send_prompt(Uuid::nil(), "第一行\nsecond line").unwrap();
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &message).await.unwrap();
        assert_eq!(read_frame(&mut bytes.as_slice()).await.unwrap(), message);
    }

    #[tokio::test]
    async fn rejects_duplicate_missing_and_overlarge_lengths_before_allocating() {
        for bytes in [
            &b"Content-Length: 2\r\nContent-Length: 2\r\n\r\n{}"[..],
            &b"Content-Type: application/json\r\n\r\n{}"[..],
            &b"Content-Length: 4194305\r\n\r\n"[..],
            &b"Content-Length: -1\r\n\r\n"[..],
        ] {
            assert!(read_frame(&mut &bytes[..]).await.is_err());
        }
    }

    #[tokio::test]
    async fn truncated_frame_fails_and_secret_error_body_is_not_returned() {
        assert_eq!(
            read_frame(&mut &b"Content-Length: 12\r\n\r\n{}"[..]).await,
            Err(ProtocolError::Io)
        );
        let response = json!({"jsonrpc":"2.0","id":1,"error":{"message":"secret-token"}});
        assert_eq!(
            validate_connect(&response),
            Err(ProtocolError::ProviderFailure)
        );
    }

    #[test]
    fn exact_protocol_and_response_identity_are_required() {
        assert!(
            validate_connect(&json!({"jsonrpc":"2.0","id":1,"result":{"protocolVersion":3}}))
                .is_ok()
        );
        for version in [0, 2, 4] {
            assert_eq!(
                validate_connect(
                    &json!({"jsonrpc":"2.0","id":1,"result":{"protocolVersion":version}})
                ),
                Err(ProtocolError::Version)
            );
        }
        assert!(
            validate_connect(&json!({"jsonrpc":"2.0","id":2,"result":{"protocolVersion":3}}))
                .is_err()
        );
    }

    #[test]
    fn sessions_cannot_enable_tools_plugins_or_custom_instructions() {
        let message = create_session(Uuid::nil(), "gpt-5.6").unwrap();
        let params = &message["params"];
        assert_eq!(params["availableTools"], json!([]));
        assert_eq!(params["tools"], json!([]));
        assert_eq!(params["enableFileHooks"], false);
        assert_eq!(params["skipCustomInstructions"], true);
        assert_eq!(params["mcpServers"], json!({}));
        assert!(create_session(Uuid::nil(), "--allow-all-tools").is_ok());
        // The model is a JSON field, not argv; even option-like text cannot
        // become a command. Newline/control-bearing names are rejected.
        assert!(create_session(Uuid::nil(), "model\n--allow-all-tools").is_err());
    }

    #[test]
    fn unsolicited_callbacks_fail_closed_without_echoing_parameters() {
        let rejection = reject_runtime_request(&json!({
            "jsonrpc":"2.0","id":99,"method":"tool.call","params":{"secret":"must-not-echo"}
        }))
        .unwrap();
        assert_eq!(rejection["id"], 99);
        assert!(!rejection.to_string().contains("must-not-echo"));
    }
}
