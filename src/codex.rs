//! Codex CLI's isolated, non-interactive adapter.

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use semver::Version;
use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::request::{render_system_prompt, render_user_prompt};
use crate::{
    Agent, Error, Generation, GenerationRequest, ReasoningEffort, Result, Usage, async_trait,
};

const DEFAULT_BINARY: &str = "codex";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const DEFAULT_MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
const MAX_STDERR_BYTES: usize = 16 * 1024;
const MAX_VERSION_BYTES: usize = 16 * 1024;
const MINIMUM_VERSION: &str = "0.146.0";

/// A semantic version reported by the Codex CLI.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CodexVersion(Version);

impl CodexVersion {
    /// The semantic-version major component.
    pub fn major(&self) -> u64 {
        self.0.major
    }

    /// The semantic-version minor component.
    pub fn minor(&self) -> u64 {
        self.0.minor
    }

    /// The semantic-version patch component.
    pub fn patch(&self) -> u64 {
        self.0.patch
    }

    /// The prerelease component, when present.
    pub fn prerelease(&self) -> Option<&str> {
        (!self.0.pre.is_empty()).then(|| self.0.pre.as_str())
    }

    /// The build metadata component, when present.
    pub fn build(&self) -> Option<&str> {
        (!self.0.build.is_empty()).then(|| self.0.build.as_str())
    }

    fn parse_cli_output(output: &str) -> Result<Self> {
        let mut words = output.split_whitespace();
        let prefix = words.next();
        let version = words.next();
        if !matches!(prefix, Some("codex-cli" | "codex-cli-exec"))
            || version.is_none()
            || words.next().is_some()
        {
            return Err(Error::InvalidResponse {
                message: format!("unexpected Codex version output `{}`", output.trim()),
            });
        }
        Version::parse(version.expect("checked above"))
            .map(Self)
            .map_err(|source| Error::InvalidResponse {
                message: format!("invalid Codex semantic version: {source}"),
            })
    }

    fn is_compatible(&self) -> bool {
        self.0 >= Version::new(0, 146, 0)
    }
}

impl fmt::Display for CodexVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Runs one isolated text-generation request through the Codex CLI.
///
/// Authentication and provider routing remain the installed CLI's
/// responsibility. The adapter does not read credentials.
#[derive(Clone)]
pub struct Codex {
    binary: PathBuf,
    default_model: Option<String>,
    default_effort: Option<ReasoningEffort>,
    default_timeout: Duration,
    max_output_bytes: usize,
    environment: Vec<(OsString, OsString)>,
}

impl Codex {
    /// Use the `codex` executable found on `PATH`.
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

    /// Use a specific Codex executable.
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

    /// Set the maximum number of stdout bytes retained from Codex.
    pub fn with_max_output_bytes(mut self, limit: usize) -> Self {
        self.max_output_bytes = limit;
        self
    }

    /// Add or replace one environment variable in the Codex process.
    ///
    /// Debug output includes variable names but never values.
    pub fn with_env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        let key = key.into();
        self.environment.retain(|(existing, _)| existing != &key);
        self.environment.push((key, value.into()));
        self
    }

    /// Detect the configured Codex executable's semantic version.
    pub async fn detect_version(&self) -> Result<CodexVersion> {
        self.validate_adapter()?;
        self.detect_version_with_timeout(self.default_timeout).await
    }

    /// Detect the configured Codex executable and enforce the supported version floor.
    pub async fn verify_compatibility(&self) -> Result<CodexVersion> {
        let version = self.detect_version().await?;
        Self::require_compatible(version)
    }

    fn require_compatible(version: CodexVersion) -> Result<CodexVersion> {
        if version.is_compatible() {
            Ok(version)
        } else {
            Err(Error::IncompatibleVersion {
                adapter: "Codex",
                detected: version.to_string(),
                minimum: MINIMUM_VERSION,
            })
        }
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

    fn validate_adapter(&self) -> Result<()> {
        if self.binary.as_os_str().is_empty() {
            return Err(Error::InvalidRequest {
                field: "codex.binary",
                message: "must not be empty".to_string(),
            });
        }
        if self.default_timeout.is_zero() {
            return Err(Error::InvalidRequest {
                field: "timeout",
                message: "must be greater than zero".to_string(),
            });
        }
        Ok(())
    }

    fn validate(&self, request: &GenerationRequest) -> Result<()> {
        request.validate()?;
        self.validate_adapter()?;
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
                field: "codex.max_output_bytes",
                message: "must be greater than zero".to_string(),
            });
        }
        if self.resolved_effort(request) == Some(ReasoningEffort::Max) {
            return Err(Error::UnsupportedOption {
                adapter: "Codex",
                option: "reasoning_effort",
                value: ReasoningEffort::Max.to_string(),
            });
        }
        Ok(())
    }

    fn args(&self, request: &GenerationRequest) -> Vec<OsString> {
        let mut args = [
            "exec",
            "--json",
            "--ephemeral",
            "--ignore-user-config",
            "--ignore-rules",
            "--skip-git-repo-check",
            "--sandbox",
            "read-only",
            "--color",
            "never",
            "--disable",
            "multi_agent",
            "--config",
            r#"web_search="disabled""#,
            "--config",
            r#"approval_policy="never""#,
            "--config",
            r#"shell_environment_policy.inherit="none""#,
            "--config",
        ]
        .into_iter()
        .map(OsString::from)
        .collect::<Vec<_>>();
        args.push(
            format!(
                "developer_instructions={}",
                toml_string(&render_system_prompt(request))
            )
            .into(),
        );

        if let Some(model) = self.resolved_model(request) {
            args.push("--model".into());
            args.push(model.into());
        }
        if let Some(effort) = self.resolved_effort(request) {
            args.push("--config".into());
            args.push(format!("model_reasoning_effort={}", toml_string(effort.as_str())).into());
        }
        args.push("-".into());
        args
    }

    async fn detect_version_with_timeout(&self, timeout: Duration) -> Result<CodexVersion> {
        let capture = self
            .invoke(["--version"], "", timeout, MAX_VERSION_BYTES)
            .await?;
        if capture.stdout.truncated {
            return Err(Error::OutputTooLarge {
                stream: "version stdout",
                limit: MAX_VERSION_BYTES,
            });
        }
        CodexVersion::parse_cli_output(&String::from_utf8_lossy(&capture.stdout.bytes))
    }

    async fn invoke_generation(
        &self,
        request: &GenerationRequest,
        working_directory: &Path,
        timeout: Duration,
    ) -> Result<String> {
        let args = self.args(request);
        let payload = render_user_prompt(request);
        let capture = self
            .invoke_in(
                args,
                &payload,
                timeout,
                self.max_output_bytes,
                working_directory,
            )
            .await?;
        if capture.stdout.truncated {
            return Err(Error::OutputTooLarge {
                stream: "stdout",
                limit: self.max_output_bytes,
            });
        }
        Ok(String::from_utf8_lossy(&capture.stdout.bytes).into_owned())
    }

    async fn invoke(
        &self,
        args: impl IntoIterator<Item = impl Into<OsString>>,
        payload: &str,
        timeout: Duration,
        stdout_limit: usize,
    ) -> Result<ProcessCapture> {
        let current_directory = std::env::current_dir().map_err(|source| Error::Io {
            operation: "resolve the current directory",
            source,
        })?;
        self.invoke_in(args, payload, timeout, stdout_limit, &current_directory)
            .await
    }

    async fn invoke_in(
        &self,
        args: impl IntoIterator<Item = impl Into<OsString>>,
        payload: &str,
        timeout: Duration,
        stdout_limit: usize,
        working_directory: &Path,
    ) -> Result<ProcessCapture> {
        let mut command = Command::new(&self.binary);
        command
            .args(args.into_iter().map(Into::into))
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
            message: "Codex stdin was unavailable".to_string(),
        })?;
        let stdout = child.stdout.take().ok_or_else(|| Error::InvalidResponse {
            message: "Codex stdout was unavailable".to_string(),
        })?;
        let stderr = child.stderr.take().ok_or_else(|| Error::InvalidResponse {
            message: "Codex stderr was unavailable".to_string(),
        })?;
        let payload = payload.as_bytes().to_vec();
        let write_stdin = async move {
            stdin.write_all(&payload).await?;
            stdin.shutdown().await?;
            drop(stdin);
            Ok::<(), std::io::Error>(())
        };
        let operation = async {
            tokio::join!(
                write_stdin,
                read_bounded(stdout, stdout_limit),
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
            operation: "wait for Codex",
            source,
        })?;
        let stdout = stdout_result.map_err(|source| Error::Io {
            operation: "read Codex stdout",
            source,
        })?;
        let stderr = stderr_result.map_err(|source| Error::Io {
            operation: "read Codex stderr",
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
            operation: "write Codex stdin",
            source,
        })?;
        Ok(ProcessCapture { stdout })
    }
}

impl Default for Codex {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Codex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let environment_keys = self
            .environment
            .iter()
            .map(|(key, _)| key)
            .collect::<Vec<_>>();
        f.debug_struct("Codex")
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
impl Agent for Codex {
    async fn generate(&self, request: &GenerationRequest) -> Result<Generation> {
        self.validate(request)?;
        let timeout = self.resolved_timeout(request);
        let started = Instant::now();

        let version_timeout = remaining(timeout, started)?;
        let version = self
            .detect_version_with_timeout(version_timeout)
            .await
            .map_err(|error| preserve_timeout(error, timeout))?;
        Self::require_compatible(version)?;

        let working_directory = tempfile::Builder::new()
            .prefix("agent-text-")
            .tempdir()
            .map_err(|source| Error::Io {
                operation: "create an isolated working directory",
                source,
            })?;
        let generation_timeout = remaining(timeout, started)?;
        let stdout = self
            .invoke_generation(request, working_directory.path(), generation_timeout)
            .await
            .map_err(|error| preserve_timeout(error, timeout))?;
        let mut generation = parse_response(&stdout, self.resolved_model(request))?;
        generation.elapsed = started.elapsed();
        Ok(generation)
    }
}

fn remaining(timeout: Duration, started: Instant) -> Result<Duration> {
    timeout
        .checked_sub(started.elapsed())
        .filter(|remaining| !remaining.is_zero())
        .ok_or(Error::Timeout { duration: timeout })
}

fn preserve_timeout(error: Error, configured: Duration) -> Error {
    match error {
        Error::Timeout { .. } => Error::Timeout {
            duration: configured,
        },
        other => other,
    }
}

fn toml_string(value: &str) -> String {
    serde_json::to_string(value).expect("serializing a string cannot fail")
}

#[derive(Debug)]
struct Capture {
    bytes: Vec<u8>,
    truncated: bool,
}

struct ProcessCapture {
    stdout: Capture,
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
struct CodexEvent {
    #[serde(rename = "type")]
    kind: String,
    item: Option<CodexItem>,
    usage: Option<CodexUsage>,
    error: Option<CodexReportedError>,
    message: Option<String>,
}

#[derive(Deserialize)]
struct CodexItem {
    #[serde(rename = "type")]
    kind: String,
    text: Option<String>,
}

#[derive(Deserialize)]
struct CodexReportedError {
    message: String,
}

#[derive(Deserialize, Default)]
struct CodexUsage {
    input_tokens: Option<u64>,
    cached_input_tokens: Option<u64>,
    cache_write_input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}

fn parse_response(stdout: &str, model: Option<&str>) -> Result<Generation> {
    let mut text = None;
    let mut usage = None;
    let mut saw_event = false;

    for line in stdout.lines().filter(|line| !line.trim().is_empty()) {
        saw_event = true;
        let event: CodexEvent =
            serde_json::from_str(line).map_err(|source| Error::InvalidResponse {
                message: source.to_string(),
            })?;
        match event.kind.as_str() {
            "item.completed" => {
                if let Some(item) = event.item {
                    if item.kind == "agent_message" {
                        text = item.text;
                    }
                }
            }
            "turn.completed" => {
                if let Some(reported) = event.usage {
                    let normalized = Usage {
                        total_input_tokens: reported.input_tokens,
                        cached_input_tokens: reported.cached_input_tokens,
                        cache_write_input_tokens: reported.cache_write_input_tokens,
                        output_tokens: reported.output_tokens,
                        cost_usd: None,
                    };
                    usage = (normalized != Usage::default()).then_some(normalized);
                }
            }
            "turn.failed" => {
                return Err(Error::AgentReported {
                    message: event
                        .error
                        .map(|error| error.message)
                        .or(event.message)
                        .unwrap_or_else(|| {
                            "Codex reported an unspecified turn failure".to_string()
                        }),
                });
            }
            "error" => {
                return Err(Error::AgentReported {
                    message: event
                        .message
                        .or_else(|| event.error.map(|error| error.message))
                        .unwrap_or_else(|| "Codex reported an unspecified failure".to_string()),
                });
            }
            _ => {}
        }
    }

    if !saw_event {
        return Err(Error::InvalidResponse {
            message: "Codex returned no JSONL events".to_string(),
        });
    }
    let text = text
        .ok_or_else(|| Error::InvalidResponse {
            message: "missing completed Codex agent message".to_string(),
        })?
        .trim()
        .to_string();
    if text.is_empty() {
        return Err(Error::EmptyOutput);
    }
    Ok(Generation {
        text,
        usage,
        model: model.map(str::to_string),
        elapsed: Duration::ZERO,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn parses_and_displays_canonical_versions() {
        let version =
            CodexVersion::parse_cli_output("codex-cli-exec 0.146.1-alpha.2+build.7\n").unwrap();
        assert_eq!(version.major(), 0);
        assert_eq!(version.minor(), 146);
        assert_eq!(version.patch(), 1);
        assert_eq!(version.prerelease(), Some("alpha.2"));
        assert_eq!(version.build(), Some("build.7"));
        assert_eq!(version.to_string(), "0.146.1-alpha.2+build.7");

        assert!(CodexVersion::parse_cli_output("codex-cli 0.146.0").is_ok());
        assert!(CodexVersion::parse_cli_output("other 0.146.0").is_err());
        assert!(CodexVersion::parse_cli_output("codex-cli 01.146.0").is_err());
        assert!(CodexVersion::parse_cli_output("codex-cli 0.146").is_err());
    }

    #[test]
    fn compatibility_uses_semver_precedence() {
        let parse = |value| CodexVersion::parse_cli_output(value).unwrap();
        assert!(!parse("codex-cli 0.145.99").is_compatible());
        assert!(!parse("codex-cli 0.146.0-alpha.1").is_compatible());
        assert!(parse("codex-cli 0.146.0").is_compatible());
        assert!(parse("codex-cli 0.147.0-alpha.1").is_compatible());
        assert!(parse("codex-cli 1.0.0").is_compatible());
    }

    #[test]
    fn args_are_isolated_and_request_options_win() {
        let mut request = GenerationRequest::new("task");
        request.options.model = Some("gpt-request".to_string());
        request.options.reasoning_effort = Some(ReasoningEffort::High);
        let adapter = Codex::new()
            .with_default_model("gpt-default")
            .with_default_effort(ReasoningEffort::Low);
        let args = adapter.args(&request);

        assert_eq!(
            &args[..12],
            [
                "exec",
                "--json",
                "--ephemeral",
                "--ignore-user-config",
                "--ignore-rules",
                "--skip-git-repo-check",
                "--sandbox",
                "read-only",
                "--color",
                "never",
                "--disable",
                "multi_agent",
            ]
            .map(OsString::from)
        );
        assert!(args.contains(&OsString::from(r#"web_search="disabled""#)));
        assert!(args.contains(&OsString::from(r#"approval_policy="never""#)));
        assert!(args.contains(&OsString::from(
            r#"shell_environment_policy.inherit="none""#
        )));
        assert!(args.contains(&OsString::from("gpt-request")));
        assert!(!args.contains(&OsString::from("gpt-default")));
        assert!(args.contains(&OsString::from(r#"model_reasoning_effort="high""#)));
        assert_eq!(args.last(), Some(&OsString::from("-")));
    }

    #[tokio::test]
    async fn rejects_max_effort_without_spawning() {
        let mut request = GenerationRequest::new("task");
        request.options.reasoning_effort = Some(ReasoningEffort::Max);
        let error = Codex::new().generate(&request).await.unwrap_err();
        assert!(matches!(error, Error::UnsupportedOption { .. }));
    }

    #[test]
    fn parses_tolerant_jsonl_and_normalizes_usage() {
        let generation = parse_response(
            r#"{"type":"thread.started","thread_id":"id"}
{"type":"future.event","extra":true}
{"type":"item.completed","item":{"id":"one","type":"agent_message","text":"draft"}}
{"type":"item.completed","item":{"id":"two","type":"reasoning","text":"hidden"}}
{"type":"item.completed","item":{"id":"three","type":"agent_message","text":"  final text  "}}
{"type":"turn.completed","usage":{"input_tokens":10,"cached_input_tokens":5,"cache_write_input_tokens":3,"output_tokens":7,"reasoning_output_tokens":2}}"#,
            Some("gpt-test"),
        )
        .unwrap();

        assert_eq!(generation.text, "final text");
        assert_eq!(generation.model.as_deref(), Some("gpt-test"));
        assert_eq!(
            generation.usage,
            Some(Usage {
                total_input_tokens: Some(10),
                cached_input_tokens: Some(5),
                cache_write_input_tokens: Some(3),
                output_tokens: Some(7),
                cost_usd: None,
            })
        );
        assert_eq!(
            parse_response(
                r#"{"type":"item.completed","item":{"type":"agent_message","text":"answer"}}
{"type":"turn.completed","usage":{}}"#,
                None,
            )
            .unwrap()
            .usage,
            None
        );
    }

    #[test]
    fn rejects_fatal_malformed_missing_and_empty_results() {
        assert!(matches!(
            parse_response(
                r#"{"type":"turn.failed","error":{"message":"budget exhausted"}}"#,
                None
            ),
            Err(Error::AgentReported { .. })
        ));
        assert!(matches!(
            parse_response(r#"{"type":"error","message":"transport failed"}"#, None),
            Err(Error::AgentReported { .. })
        ));
        assert!(matches!(
            parse_response("not json", None),
            Err(Error::InvalidResponse { .. })
        ));
        assert!(matches!(
            parse_response(r#"{"type":"turn.completed","usage":{}}"#, None),
            Err(Error::InvalidResponse { .. })
        ));
        assert!(matches!(
            parse_response(
                r#"{"type":"item.completed","item":{"type":"agent_message","text":"  "}}"#,
                None
            ),
            Err(Error::EmptyOutput)
        ));
    }

    #[test]
    fn debug_redacts_environment_values() {
        let debug = format!(
            "{:?}",
            Codex::new().with_env("SECRET_NAME", "do-not-display")
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
            let path = directory.path().join("fake-codex");
            fs::write(&path, format!("#!/bin/sh\nset -eu\n{body}\n")).unwrap();
            let mut permissions = fs::metadata(&path).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&path, permissions).unwrap();
            (directory, path)
        }

        #[tokio::test]
        async fn detects_version_then_generates_in_an_isolated_directory() {
            let (_directory, binary) = script(
                r#"
if [ "$1" = "--version" ]; then
  printf '%s\n' 'codex-cli 0.146.0'
  exit 0
fi
printf '%s\n' "$@" > "$FAKE_ARGS"
pwd > "$FAKE_CWD"
cat > "$FAKE_STDIN"
printf '%s\n' '{"type":"item.completed","item":{"type":"agent_message","text":"  hello  "}}'
printf '%s\n' '{"type":"turn.completed","usage":{"input_tokens":2,"cached_input_tokens":0,"output_tokens":1}}'
"#,
            );
            let output = tempfile::tempdir().unwrap();
            let args_path = output.path().join("args");
            let cwd_path = output.path().join("cwd");
            let stdin_path = output.path().join("stdin");
            let adapter = Codex::new()
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
            let args = fs::read_to_string(args_path).unwrap();
            assert!(args.contains("--ephemeral"));
            assert!(args.contains("--ignore-user-config"));
            let stdin = fs::read_to_string(stdin_path).unwrap();
            assert!(stdin.contains("say hello"));
            assert!(stdin.contains("label=\"facts\""));
        }

        #[tokio::test]
        async fn public_version_methods_detect_and_verify() {
            let (_directory, binary) = script(
                r#"
printf '%s\n' 'codex-cli-exec 0.146.1'
"#,
            );
            let adapter = Codex::new().with_binary(binary);

            let detected = adapter.detect_version().await.unwrap();
            let verified = adapter.verify_compatibility().await.unwrap();

            assert_eq!(detected.to_string(), "0.146.1");
            assert_eq!(verified, detected);
        }

        #[tokio::test]
        async fn rejects_old_version_before_exec() {
            let (_directory, binary) = script(
                r#"
if [ "$1" = "--version" ]; then
  printf '%s\n' 'codex-cli 0.145.0'
  exit 0
fi
exit 99
"#,
            );
            let error = Codex::new()
                .with_binary(binary)
                .generate(&GenerationRequest::new("task"))
                .await
                .unwrap_err();
            assert!(matches!(error, Error::IncompatibleVersion { .. }));
        }

        #[tokio::test]
        async fn enforces_total_timeout_and_output_limit() {
            let (_directory, slow_binary) = script(
                r#"
if [ "$1" = "--version" ]; then
  printf '%s\n' 'codex-cli 0.146.0'
  exit 0
fi
cat >/dev/null
sleep 1
"#,
            );
            let mut request = GenerationRequest::new("task");
            request.options.timeout = Some(Duration::from_millis(20));
            let timeout = Codex::new()
                .with_binary(slow_binary)
                .generate(&request)
                .await
                .unwrap_err();
            assert!(matches!(
                timeout,
                Error::Timeout { duration } if duration == Duration::from_millis(20)
            ));

            let (_directory, noisy_binary) = script(
                r#"
if [ "$1" = "--version" ]; then
  printf '%s\n' 'codex-cli 0.146.0'
  exit 0
fi
cat >/dev/null
printf '%0200d' 0
"#,
            );
            let too_large = Codex::new()
                .with_binary(noisy_binary)
                .with_max_output_bytes(32)
                .generate(&GenerationRequest::new("task"))
                .await
                .unwrap_err();
            assert!(matches!(too_large, Error::OutputTooLarge { .. }));
        }

        #[tokio::test]
        async fn concurrent_generations_do_not_serialize_version_checks() {
            let (_directory, binary) = script(
                r#"
if [ "$1" = "--version" ]; then
  touch "$FAKE_BARRIER/version.$$"
  remaining=200
  while [ "$(find "$FAKE_BARRIER" -name 'version.*' | wc -l)" -lt 2 ]; do
    remaining=$((remaining - 1))
    [ "$remaining" -gt 0 ] || exit 91
    sleep 0.01
  done
  printf '%s\n' 'codex-cli 0.146.0'
  exit 0
fi
cat >/dev/null
printf '%s\n' '{"type":"item.completed","item":{"type":"agent_message","text":"parallel"}}'
printf '%s\n' '{"type":"turn.completed","usage":{"input_tokens":1,"cached_input_tokens":0,"output_tokens":1}}'
"#,
            );
            let barrier = tempfile::tempdir().unwrap();
            let adapter = Codex::new()
                .with_binary(binary)
                .with_default_timeout(Duration::from_secs(5))
                .with_env("FAKE_BARRIER", barrier.path().as_os_str());
            let first_request = GenerationRequest::new("first");
            let second_request = GenerationRequest::new("second");

            let (first, second) = tokio::join!(
                adapter.generate(&first_request),
                adapter.generate(&second_request)
            );
            assert_eq!(first.unwrap().text, "parallel");
            assert_eq!(second.unwrap().text, "parallel");
        }
    }
}
