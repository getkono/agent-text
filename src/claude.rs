//! Claude Code's isolated, non-interactive adapter.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::request::{render_system_prompt, render_user_prompt};
use crate::{
    Agent, Error, Generation, GenerationRequest, ReasoningEffort, Result, Usage, async_trait,
};

const DEFAULT_BINARY: &str = "claude";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const DEFAULT_MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
const MAX_STDERR_BYTES: usize = 16 * 1024;

/// Runs one isolated text-generation request through the Claude Code CLI.
///
/// Authentication and provider routing remain the installed CLI's
/// responsibility. The adapter does not read credentials.
#[derive(Clone)]
pub struct ClaudeCode {
    binary: PathBuf,
    default_model: Option<String>,
    default_effort: Option<ReasoningEffort>,
    default_timeout: Duration,
    max_output_bytes: usize,
    environment: Vec<(OsString, OsString)>,
}

impl ClaudeCode {
    /// Use the `claude` executable found on `PATH`.
    pub fn new() -> Self {
        Self {
            binary: PathBuf::from(DEFAULT_BINARY),
            default_model: None,
            default_effort: None,
            default_timeout: DEFAULT_TIMEOUT,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            environment: Vec::new(),
        }
    }

    /// Use a specific Claude Code executable.
    pub fn with_binary(mut self, binary: impl Into<PathBuf>) -> Self {
        self.binary = binary.into();
        self
    }

    /// Set the model used when a request does not name one.
    pub fn with_default_model(mut self, model: impl Into<String>) -> Self {
        self.default_model = Some(model.into());
        self
    }

    /// Set the effort used when a request does not request one.
    pub fn with_default_effort(mut self, effort: ReasoningEffort) -> Self {
        self.default_effort = Some(effort);
        self
    }

    /// Set the timeout used when a request does not override it.
    pub fn with_default_timeout(mut self, timeout: Duration) -> Self {
        self.default_timeout = timeout;
        self
    }

    /// Set the maximum number of stdout bytes retained from Claude.
    pub fn with_max_output_bytes(mut self, limit: usize) -> Self {
        self.max_output_bytes = limit;
        self
    }

    /// Add or replace one environment variable in the Claude process.
    ///
    /// Debug output includes variable names but never values.
    pub fn with_env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        let key = key.into();
        self.environment.retain(|(existing, _)| existing != &key);
        self.environment.push((key, value.into()));
        self
    }

    fn resolved_model<'a>(&'a self, request: &'a GenerationRequest) -> Option<&'a str> {
        request
            .options
            .model
            .as_deref()
            .or(self.default_model.as_deref())
    }

    fn resolved_effort(&self, request: &GenerationRequest) -> Option<ReasoningEffort> {
        request.options.reasoning_effort.or(self.default_effort)
    }

    fn resolved_timeout(&self, request: &GenerationRequest) -> Duration {
        request.options.timeout.unwrap_or(self.default_timeout)
    }

    fn validate(&self, request: &GenerationRequest) -> Result<()> {
        request.validate()?;
        if self.binary.as_os_str().is_empty() {
            return Err(Error::InvalidRequest {
                field: "claude.binary",
                message: "must not be empty".to_string(),
            });
        }
        if self
            .resolved_model(request)
            .is_some_and(|model| model.trim().is_empty())
        {
            return Err(Error::InvalidRequest {
                field: "model",
                message: "must not be blank".to_string(),
            });
        }
        if self.resolved_timeout(request).is_zero() {
            return Err(Error::InvalidRequest {
                field: "timeout",
                message: "must be greater than zero".to_string(),
            });
        }
        if self.max_output_bytes == 0 {
            return Err(Error::InvalidRequest {
                field: "claude.max_output_bytes",
                message: "must be greater than zero".to_string(),
            });
        }
        if self.resolved_effort(request) == Some(ReasoningEffort::Minimal) {
            return Err(Error::UnsupportedOption {
                adapter: "Claude Code",
                option: "reasoning_effort",
                value: ReasoningEffort::Minimal.to_string(),
            });
        }
        Ok(())
    }

    fn args(&self, request: &GenerationRequest) -> Vec<OsString> {
        let mut args = [
            "-p",
            "--output-format",
            "json",
            "--tools",
            "",
            "--no-session-persistence",
            "--disable-slash-commands",
            "--safe-mode",
            "--system-prompt",
        ]
        .into_iter()
        .map(OsString::from)
        .collect::<Vec<_>>();
        args.push(render_system_prompt(request).into());

        if let Some(model) = self.resolved_model(request) {
            args.push("--model".into());
            args.push(model.into());
        }
        if let Some(effort) = self.resolved_effort(request) {
            args.push("--effort".into());
            args.push(effort.as_str().into());
        }
        args
    }

    async fn invoke(
        &self,
        request: &GenerationRequest,
        working_directory: &Path,
    ) -> Result<String> {
        let mut command = Command::new(&self.binary);
        command
            .args(self.args(request))
            .current_dir(working_directory)
            .envs(self.environment.iter().cloned())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = command.spawn().map_err(|source| Error::Spawn {
            binary: self.binary.clone(),
            source,
        })?;
        let mut stdin = child.stdin.take().ok_or_else(|| Error::InvalidResponse {
            message: "Claude stdin was unavailable".to_string(),
        })?;
        let stdout = child.stdout.take().ok_or_else(|| Error::InvalidResponse {
            message: "Claude stdout was unavailable".to_string(),
        })?;
        let stderr = child.stderr.take().ok_or_else(|| Error::InvalidResponse {
            message: "Claude stderr was unavailable".to_string(),
        })?;

        let payload = render_user_prompt(request);
        let timeout = self.resolved_timeout(request);
        let write_stdin = async move {
            stdin.write_all(payload.as_bytes()).await?;
            stdin.shutdown().await?;
            drop(stdin);
            Ok::<(), std::io::Error>(())
        };
        let operation = async {
            tokio::join!(
                write_stdin,
                read_bounded(stdout, self.max_output_bytes),
                read_bounded(stderr, MAX_STDERR_BYTES),
                child.wait(),
            )
        };

        let (stdin_result, stdout_result, stderr_result, status_result) =
            match tokio::time::timeout(timeout, operation).await {
                Ok(outcome) => outcome,
                Err(_) => {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    return Err(Error::Timeout { duration: timeout });
                }
            };

        let status = status_result.map_err(|source| Error::Io {
            operation: "wait for Claude",
            source,
        })?;
        let stdout = stdout_result.map_err(|source| Error::Io {
            operation: "read Claude stdout",
            source,
        })?;
        let stderr = stderr_result.map_err(|source| Error::Io {
            operation: "read Claude stderr",
            source,
        })?;

        if !status.success() {
            let diagnostic = if stderr.bytes.iter().any(|byte| !byte.is_ascii_whitespace()) {
                display_capture(stderr)
            } else {
                display_capture(stdout)
            };
            return Err(Error::Exit {
                status: status.code(),
                stderr: diagnostic,
            });
        }

        stdin_result.map_err(|source| Error::Io {
            operation: "write Claude stdin",
            source,
        })?;
        if stdout.truncated {
            return Err(Error::OutputTooLarge {
                stream: "stdout",
                limit: self.max_output_bytes,
            });
        }

        Ok(String::from_utf8_lossy(&stdout.bytes).into_owned())
    }
}

impl Default for ClaudeCode {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for ClaudeCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let environment_keys = self
            .environment
            .iter()
            .map(|(key, _)| key)
            .collect::<Vec<_>>();
        f.debug_struct("ClaudeCode")
            .field("binary", &self.binary)
            .field("default_model", &self.default_model)
            .field("default_effort", &self.default_effort)
            .field("default_timeout", &self.default_timeout)
            .field("max_output_bytes", &self.max_output_bytes)
            .field("environment_keys", &environment_keys)
            .finish()
    }
}

#[async_trait]
impl Agent for ClaudeCode {
    async fn generate(&self, request: &GenerationRequest) -> Result<Generation> {
        self.validate(request)?;
        let working_directory = tempfile::Builder::new()
            .prefix("agent-text-")
            .tempdir()
            .map_err(|source| Error::Io {
                operation: "create an isolated working directory",
                source,
            })?;
        let started = Instant::now();
        let stdout = self.invoke(request, working_directory.path()).await?;
        let mut generation = parse_response(&stdout)?;
        generation.elapsed = started.elapsed();
        Ok(generation)
    }
}

#[derive(Debug)]
struct Capture {
    bytes: Vec<u8>,
    truncated: bool,
}

async fn read_bounded(
    mut reader: impl AsyncRead + Unpin,
    limit: usize,
) -> std::io::Result<Capture> {
    let mut bytes = Vec::with_capacity(limit.min(8 * 1024));
    let mut buffer = [0_u8; 8 * 1024];
    let mut truncated = false;
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(bytes.len());
        let retained = read.min(remaining);
        bytes.extend_from_slice(&buffer[..retained]);
        truncated |= retained < read;
    }
    Ok(Capture { bytes, truncated })
}

fn display_capture(capture: Capture) -> String {
    let mut message = String::from_utf8_lossy(&capture.bytes).trim().to_string();
    if capture.truncated {
        message.push_str("\n[diagnostic truncated]");
    }
    if message.is_empty() {
        "no diagnostic output".to_string()
    } else {
        message
    }
}

#[derive(Deserialize)]
struct ClaudeResponse {
    #[serde(default)]
    is_error: bool,
    subtype: Option<String>,
    result: Option<String>,
    total_cost_usd: Option<f64>,
    usage: Option<ClaudeUsage>,
    model: Option<String>,
    #[serde(rename = "modelUsage", default)]
    model_usage: BTreeMap<String, ClaudeModelUsage>,
}

#[derive(Deserialize, Default)]
struct ClaudeUsage {
    input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct ClaudeModelUsage {
    #[serde(rename = "inputTokens")]
    input_tokens: Option<u64>,
    #[serde(rename = "outputTokens")]
    output_tokens: Option<u64>,
    #[serde(rename = "cacheReadInputTokens")]
    cache_read_input_tokens: Option<u64>,
    #[serde(rename = "cacheCreationInputTokens")]
    cache_creation_input_tokens: Option<u64>,
    #[serde(rename = "costUSD")]
    cost_usd: Option<f64>,
}

fn parse_response(stdout: &str) -> Result<Generation> {
    let response: ClaudeResponse =
        serde_json::from_str(stdout).map_err(|source| Error::InvalidResponse {
            message: source.to_string(),
        })?;
    let reported_error = response.is_error
        || response
            .subtype
            .as_deref()
            .is_some_and(|subtype| subtype.starts_with("error"));
    if reported_error {
        return Err(Error::AgentReported {
            message: response
                .result
                .or(response.subtype)
                .unwrap_or_else(|| "Claude reported an unspecified failure".to_string()),
        });
    }

    let text = response
        .result
        .ok_or_else(|| Error::InvalidResponse {
            message: "missing `result` field".to_string(),
        })?
        .trim()
        .to_string();
    if text.is_empty() {
        return Err(Error::EmptyOutput);
    }

    let usage = normalize_usage(response.usage, response.total_cost_usd);
    let model = response.model.or_else(|| {
        (response.model_usage.len() == 1)
            .then(|| response.model_usage.into_keys().next())
            .flatten()
    });

    Ok(Generation {
        text,
        usage,
        model,
        elapsed: Duration::ZERO,
    })
}

fn normalize_usage(usage: Option<ClaudeUsage>, cost_usd: Option<f64>) -> Option<Usage> {
    let usage = usage.unwrap_or_default();
    let total_input_tokens = [
        usage.input_tokens,
        usage.cache_creation_input_tokens,
        usage.cache_read_input_tokens,
    ]
    .into_iter()
    .flatten()
    .reduce(u64::saturating_add);

    let normalized = Usage {
        total_input_tokens,
        cached_input_tokens: usage.cache_read_input_tokens,
        cache_write_input_tokens: usage.cache_creation_input_tokens,
        output_tokens: usage.output_tokens,
        cost_usd,
    };
    (normalized != Usage::default()).then_some(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn args_are_isolated_and_request_options_win() {
        let mut request = GenerationRequest::new("task");
        request.options.model = Some("sonnet".to_string());
        request.options.reasoning_effort = Some(ReasoningEffort::High);
        let adapter = ClaudeCode::new()
            .with_default_model("haiku")
            .with_default_effort(ReasoningEffort::Low);

        let args = adapter.args(&request);
        assert_eq!(
            &args[..9],
            [
                "-p",
                "--output-format",
                "json",
                "--tools",
                "",
                "--no-session-persistence",
                "--disable-slash-commands",
                "--safe-mode",
                "--system-prompt",
            ]
            .map(OsString::from)
        );
        assert!(args.contains(&OsString::from("--model")));
        assert!(args.contains(&OsString::from("sonnet")));
        assert!(!args.contains(&OsString::from("haiku")));
        assert_eq!(
            &args[args.len() - 2..],
            [OsString::from("--effort"), OsString::from("high")]
        );
    }

    #[tokio::test]
    async fn rejects_minimal_effort_without_spawning() {
        let mut request = GenerationRequest::new("task");
        request.options.reasoning_effort = Some(ReasoningEffort::Minimal);
        let error = ClaudeCode::new().generate(&request).await.unwrap_err();
        assert!(matches!(error, Error::UnsupportedOption { .. }));
    }

    #[test]
    fn parses_and_normalizes_a_success() {
        let generation = parse_response(
            r#"{
                "is_error": false,
                "result": "  generated text\n",
                "total_cost_usd": 0.125,
                "usage": {
                    "input_tokens": 2,
                    "cache_creation_input_tokens": 3,
                    "cache_read_input_tokens": 5,
                    "output_tokens": 7
                },
                "modelUsage": {
                    "claude-sonnet-test": {
                        "inputTokens": 2,
                        "outputTokens": 7,
                        "costUSD": 0.125
                    }
                }
            }"#,
        )
        .unwrap();

        assert_eq!(generation.text, "generated text");
        assert_eq!(generation.model.as_deref(), Some("claude-sonnet-test"));
        assert_eq!(
            generation.usage,
            Some(Usage {
                total_input_tokens: Some(10),
                cached_input_tokens: Some(5),
                cache_write_input_tokens: Some(3),
                output_tokens: Some(7),
                cost_usd: Some(0.125),
            })
        );
    }

    #[test]
    fn rejects_reported_malformed_and_empty_results() {
        assert!(matches!(
            parse_response(r#"{"is_error":true,"result":"budget exhausted"}"#),
            Err(Error::AgentReported { .. })
        ));
        assert!(matches!(
            parse_response("not json"),
            Err(Error::InvalidResponse { .. })
        ));
        assert!(matches!(
            parse_response(r#"{"is_error":false,"result":"  "}"#),
            Err(Error::EmptyOutput)
        ));
    }

    #[test]
    fn debug_redacts_environment_values() {
        let debug = format!(
            "{:?}",
            ClaudeCode::new().with_env("SECRET_NAME", "do-not-display")
        );
        assert!(debug.contains("SECRET_NAME"));
        assert!(!debug.contains("do-not-display"));
    }

    #[cfg(unix)]
    mod unix {
        use super::*;
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        fn script(body: &str) -> (tempfile::TempDir, PathBuf) {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("fake-claude");
            fs::write(&path, format!("#!/bin/sh\nset -eu\n{body}\n")).unwrap();
            let mut permissions = fs::metadata(&path).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&path, permissions).unwrap();
            (directory, path)
        }

        #[tokio::test]
        async fn invokes_fake_claude_in_an_isolated_directory() {
            let (_directory, binary) = script(
                r#"
printf '%s\n' "$@" > "$FAKE_ARGS"
pwd > "$FAKE_CWD"
cat > "$FAKE_STDIN"
printf '%s' '{"is_error":false,"result":"  hello  ","usage":{"input_tokens":2,"output_tokens":1}}'
"#,
            );
            let output = tempfile::tempdir().unwrap();
            let args_path = output.path().join("args");
            let cwd_path = output.path().join("cwd");
            let stdin_path = output.path().join("stdin");
            let adapter = ClaudeCode::new()
                .with_binary(binary)
                .with_env("FAKE_ARGS", args_path.as_os_str())
                .with_env("FAKE_CWD", cwd_path.as_os_str())
                .with_env("FAKE_STDIN", stdin_path.as_os_str());
            let request = GenerationRequest::new("say hello")
                .with_context(crate::ContextItem::text("facts", "hello"));

            let generation = adapter.generate(&request).await.unwrap();

            assert_eq!(generation.text, "hello");
            assert_ne!(
                fs::read_to_string(cwd_path).unwrap().trim(),
                std::env::current_dir().unwrap().to_string_lossy()
            );
            assert!(
                fs::read_to_string(args_path)
                    .unwrap()
                    .contains("--safe-mode")
            );
            let stdin = fs::read_to_string(stdin_path).unwrap();
            assert!(stdin.contains("say hello"));
            assert!(stdin.contains("label=\"facts\""));
        }

        #[tokio::test]
        async fn enforces_timeout_and_output_limit() {
            let (_directory, slow_binary) = script(
                r#"
cat >/dev/null
sleep 1
"#,
            );
            let mut request = GenerationRequest::new("task");
            request.options.timeout = Some(Duration::from_millis(20));
            let timeout = ClaudeCode::new()
                .with_binary(slow_binary)
                .generate(&request)
                .await
                .unwrap_err();
            assert!(matches!(timeout, Error::Timeout { .. }));

            let (_directory, noisy_binary) = script(
                r#"
cat >/dev/null
printf '%0200d' 0
"#,
            );
            let too_large = ClaudeCode::new()
                .with_binary(noisy_binary)
                .with_max_output_bytes(32)
                .generate(&GenerationRequest::new("task"))
                .await
                .unwrap_err();
            assert!(matches!(too_large, Error::OutputTooLarge { .. }));
        }

        #[tokio::test]
        async fn reports_nonzero_exit_without_leaking_unbounded_stderr() {
            let (_directory, binary) = script(
                r#"
cat >/dev/null
printf 'denied' >&2
exit 7
"#,
            );
            let error = ClaudeCode::new()
                .with_binary(binary)
                .generate(&GenerationRequest::new("task"))
                .await
                .unwrap_err();
            assert!(matches!(
                error,
                Error::Exit {
                    status: Some(7),
                    ..
                }
            ));
            assert!(error.to_string().contains("denied"));
        }
    }
}
