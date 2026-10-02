use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;

use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;
use serde_json::json;

const TOOL_NAME: &str = "local_code_search";
const MODEL_OUTPUT_LIMIT_BYTES: usize = 100_000;
const PROCESS_OUTPUT_LIMIT_BYTES: usize = 512_000;
const DEFAULT_SEARCH_RESULTS: u32 = 50;
const MAX_SEARCH_RESULTS: u32 = 100;
const MAX_SEARCH_CONTEXT: u32 = 10;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(45);

#[derive(Debug, Deserialize)]
struct LocalCodeSearchArgs {
    mode: String,
    #[serde(default)]
    query: String,
    #[serde(default)]
    profile: Option<String>,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    exact: bool,
    #[serde(default)]
    max: Option<u32>,
    #[serde(default)]
    context: Option<u32>,
}

pub struct LocalCodeSearchHandler;

impl ToolExecutor<ToolInvocation> for LocalCodeSearchHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        create_local_code_search_tool()
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(self.handle_call(invocation))
    }
}

impl CoreToolRuntime for LocalCodeSearchHandler {}

impl LocalCodeSearchHandler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let ToolInvocation { turn, payload, .. } = invocation;
        let ToolPayload::Function { arguments } = payload else {
            return Err(FunctionCallError::RespondToModel(format!(
                "{TOOL_NAME} handler received unsupported payload"
            )));
        };
        let args: LocalCodeSearchArgs = parse_arguments(&arguments)?;
        let argv = arc_idx_args(&args).map_err(FunctionCallError::RespondToModel)?;
        let configured = turn.config.local_search.command.as_deref();
        let exe = arc_idx_executable(configured);
        let cwd = turn.config.cwd.clone();

        let output = run_arc_idx(&exe, &argv, cwd.as_path())
            .await
            .map_err(FunctionCallError::RespondToModel)?;
        let success = output.status.success();
        Ok(boxed_tool_output(FunctionToolOutput::from_text(
            format_arc_idx_output(&output),
            Some(success),
        )))
    }
}

fn create_local_code_search_tool() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "mode".to_string(),
            JsonSchema::string_enum(
                vec![
                    json!("search"),
                    json!("symbol"),
                    json!("files"),
                    json!("ast"),
                    json!("stats"),
                ],
                Some(
                    "search (content), symbol, files, ast (AST patterns), or stats. Use search, symbol, files, or ast for discovery and stats for index inventory."
                        .to_string(),
                ),
            ),
        ),
        (
            "query".to_string(),
            JsonSchema::string(Some(
                "Search query, symbol name, file substring, or ast-grep pattern.".to_string(),
            )),
        ),
        (
            "profile".to_string(),
            JsonSchema::string(Some(
                "Optional arc-idx profile from the repo .ai-code-index/config.json.".to_string(),
            )),
        ),
        (
            "language".to_string(),
            JsonSchema::string(Some(
                "Language for ast mode (passed as --lang). Required for ast.".to_string(),
            )),
        ),
        (
            "kind".to_string(),
            JsonSchema::string(Some(
                "Optional ctags kind for symbol mode (func, struct, method, ...).".to_string(),
            )),
        ),
        (
            "exact".to_string(),
            JsonSchema::boolean(Some(
                "When true, symbol mode passes --exact.".to_string(),
            )),
        ),
        (
            "max".to_string(),
            JsonSchema::integer(Some(
                "Optional search --max result cap.".to_string(),
            )),
        ),
        (
            "context".to_string(),
            JsonSchema::integer(Some(
                "Optional search --context lines.".to_string(),
            )),
        ),
    ]);

    ToolSpec::Function(ResponsesApiTool {
        name: TOOL_NAME.to_string(),
        description:
            "Primary local code discovery via arc-idx (arc is also supported on macOS). Finds files, \
symbols, content, AST patterns, and index inventory. Modes: search, symbol, files, ast, stats. \
When this tool is available, prefer it over shell rg, grep, find, fd, git grep, recursive ls, \
.ai-code-index/*.sh, or ad-hoc python/node tree walks for retrieval. Output is a JSON envelope."
                .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["mode".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

fn arc_idx_args(args: &LocalCodeSearchArgs) -> Result<Vec<String>, String> {
    let mode = args.mode.trim();
    match mode {
        "search" => {
            if args.query.trim().is_empty() {
                return Err("search mode requires query".to_string());
            }
            let mut argv = vec![
                "search".to_string(),
                args.query.clone(),
                "--format".to_string(),
                "json".to_string(),
            ];
            push_opt(&mut argv, "--profile", args.profile.as_deref());
            push_num(
                &mut argv,
                "--context",
                args.context.map(|value| value.min(MAX_SEARCH_CONTEXT)),
            );
            push_num(
                &mut argv,
                "--max",
                Some(
                    args.max
                        .unwrap_or(DEFAULT_SEARCH_RESULTS)
                        .clamp(1, MAX_SEARCH_RESULTS),
                ),
            );
            Ok(argv)
        }
        "symbol" => {
            if args.query.trim().is_empty() {
                return Err("symbol mode requires query".to_string());
            }
            let mut argv = vec![
                "symbol".to_string(),
                args.query.clone(),
                "--format".to_string(),
                "json".to_string(),
            ];
            push_opt(&mut argv, "--kind", args.kind.as_deref());
            if args.exact {
                argv.push("--exact".to_string());
            }
            Ok(argv)
        }
        "files" => {
            let mut argv = vec!["files".to_string()];
            if !args.query.trim().is_empty() {
                argv.push(args.query.clone());
            }
            push_opt(&mut argv, "--profile", args.profile.as_deref());
            argv.push("--format".to_string());
            argv.push("json".to_string());
            Ok(argv)
        }
        "ast" => {
            let language = args
                .language
                .as_deref()
                .map(str::trim)
                .filter(|language| !language.is_empty())
                .ok_or_else(|| "ast mode requires language".to_string())?;
            if args.query.trim().is_empty() {
                return Err("ast mode requires query".to_string());
            }
            Ok(vec![
                "ast".to_string(),
                "--lang".to_string(),
                language.to_string(),
                args.query.clone(),
                "--format".to_string(),
                "json".to_string(),
            ])
        }
        "stats" => {
            let mut argv = vec![
                "stats".to_string(),
                "--format".to_string(),
                "json".to_string(),
            ];
            push_opt(&mut argv, "--profile", args.profile.as_deref());
            Ok(argv)
        }
        other => Err(format!(
            "unknown local_code_search mode `{other}`; expected search, symbol, files, ast, or stats"
        )),
    }
}

fn push_opt(argv: &mut Vec<String>, flag: &str, value: Option<&str>) {
    if let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) {
        argv.push(flag.to_string());
        argv.push(value.to_string());
    }
}

fn push_num(argv: &mut Vec<String>, flag: &str, value: Option<u32>) {
    if let Some(value) = value {
        argv.push(flag.to_string());
        argv.push(value.to_string());
    }
}

struct BoundedCapture {
    text: String,
    truncated: bool,
}

struct ArcIdxOutput {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
    stdout_truncated: bool,
    stderr_truncated: bool,
}

async fn read_bounded<R>(mut reader: R, limit: usize) -> std::io::Result<BoundedCapture>
where
    R: AsyncRead + Unpin,
{
    let mut stored = Vec::with_capacity(limit.min(8192));
    let mut buffer = [0_u8; 8192];
    let mut truncated = false;

    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        if stored.len() < limit {
            let take = (limit - stored.len()).min(read);
            stored.extend_from_slice(&buffer[..take]);
            if take < read {
                truncated = true;
            }
        } else {
            truncated = true;
        }
    }

    Ok(BoundedCapture {
        text: String::from_utf8_lossy(&stored).into_owned(),
        truncated,
    })
}

fn configure_arc_idx_env(command: &mut tokio::process::Command) {
    command.env_clear();

    const SAFE_ENV_KEYS: &[&str] = &[
        "PATH",
        "HOME",
        "USERPROFILE",
        "TMPDIR",
        "TEMP",
        "TMP",
        "LANG",
        "TERM",
        "XDG_CONFIG_HOME",
        "XDG_CACHE_HOME",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
        "SYSTEMROOT",
        "WINDIR",
        "COMSPEC",
        "PATHEXT",
    ];

    for key in SAFE_ENV_KEYS {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }

    for (key, value) in std::env::vars_os() {
        let key_text = key.to_string_lossy();
        if key_text.starts_with("ARC_IDX_") || key_text.starts_with("LC_") {
            command.env(&key, value);
        }
    }
}

async fn run_arc_idx(exe: &Path, argv: &[String], cwd: &Path) -> Result<ArcIdxOutput, String> {
    let mut command = tokio::process::Command::new(exe);
    configure_arc_idx_env(&mut command);
    command
        .args(argv)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    let mut child = command.spawn().map_err(|err| {
        format!(
            "local_code_search could not start `{}`: {err}. Install arc-idx (or arc on macOS), set ARC_IDX_BIN, or configure [local_search].command.",
            exe.display()
        )
    })?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "local_code_search could not capture stdout".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "local_code_search could not capture stderr".to_string())?;

    let wait = async {
        tokio::try_join!(
            child.wait(),
            read_bounded(stdout, PROCESS_OUTPUT_LIMIT_BYTES),
            read_bounded(stderr, PROCESS_OUTPUT_LIMIT_BYTES),
        )
    };

    let (status, stdout, stderr) = match tokio::time::timeout(COMMAND_TIMEOUT, wait).await {
        Ok(Ok(result)) => result,
        Ok(Err(err)) => {
            return Err(format!(
                "local_code_search failed while running `{}`: {err}",
                exe.display()
            ));
        }
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(format!(
                "local_code_search timed out after {}s running `{}`",
                COMMAND_TIMEOUT.as_secs(),
                exe.display()
            ));
        }
    };

    Ok(ArcIdxOutput {
        status,
        stdout: stdout.text,
        stderr: stderr.text,
        stdout_truncated: stdout.truncated,
        stderr_truncated: stderr.truncated,
    })
}

fn truncate_utf8(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut preview = text[..end].to_string();
    preview.push_str("\n...[truncated]");
    preview
}

fn format_arc_idx_output(output: &ArcIdxOutput) -> String {
    let stdout = output.stdout.trim();
    let parsed = if stdout.is_empty() {
        serde_json::Value::Null
    } else if output.stdout_truncated {
        serde_json::Value::String(output.stdout.clone())
    } else {
        serde_json::from_str::<serde_json::Value>(stdout)
            .unwrap_or_else(|_| serde_json::Value::String(output.stdout.clone()))
    };

    let response = json!({
        "ok": output.status.success(),
        "exit_code": output.status.code(),
        "data": parsed,
        "stderr": if output.stderr.trim().is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::Value::String(output.stderr.clone())
        },
        "output_truncated": output.stdout_truncated || output.stderr_truncated,
    });

    let serialized = serde_json::to_string(&response)
        .unwrap_or_else(|_| "{\"ok\":false,\"error\":\"failed_to_serialize_tool_output\"}".to_string());
    if serialized.len() <= MODEL_OUTPUT_LIMIT_BYTES {
        return serialized;
    }

    serde_json::to_string(&json!({
        "ok": output.status.success(),
        "exit_code": output.status.code(),
        "error": "tool_output_too_large",
        "stdout_preview": truncate_utf8(&output.stdout, 8192),
        "stderr_preview": truncate_utf8(&output.stderr, 4096),
        "output_truncated": true,
    }))
    .unwrap_or_else(|_| "{\"ok\":false,\"error\":\"failed_to_serialize_tool_output\"}".to_string())
}

pub(crate) fn local_code_search_available(configured: Option<&str>) -> bool {
    resolve_existing_arc_idx(configured).is_some()
}

fn arc_idx_executable(configured: Option<&str>) -> PathBuf {
    resolve_existing_arc_idx(configured).unwrap_or_else(|| {
        configured
            .map(str::trim)
            .filter(|command| !command.is_empty())
            .map_or_else(|| PathBuf::from("arc-idx"), PathBuf::from)
    })
}

fn resolve_existing_arc_idx(configured: Option<&str>) -> Option<PathBuf> {
    match configured
        .map(str::trim)
        .filter(|command| !command.is_empty())
    {
        Some(command) => existing_command(command),
        None => default_arc_idx(),
    }
}

fn existing_command(command: &str) -> Option<PathBuf> {
    if command.contains('/') || command.contains('\\') {
        let path = PathBuf::from(command);
        return path_is_executable(&path).then_some(path);
    }
    which::which(command).ok()
}

fn path_is_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }

    #[cfg(not(unix))]
    {
        true
    }
}

fn default_arc_idx() -> Option<PathBuf> {
    if let Ok(env_value) = std::env::var("ARC_IDX_BIN") {
        let env_value = env_value.trim();
        if !env_value.is_empty()
            && let Some(found) = existing_command(env_value)
        {
            return Some(found);
        }
    }

    if let Ok(found) = which::which("arc-idx") {
        return Some(found);
    }

    #[cfg(target_os = "macos")]
    if let Ok(found) = which::which("arc") {
        return Some(found);
    }

    if let Some(found) = local_bin_candidate("arc-idx") {
        return Some(found);
    }

    #[cfg(target_os = "macos")]
    if let Some(found) = local_bin_candidate("arc") {
        return Some(found);
    }

    None
}

fn local_bin_candidate(name: &str) -> Option<PathBuf> {
    let path = dirs::home_dir()?.join(".local").join("bin").join(name);
    path_is_executable(&path).then_some(path)
}

#[cfg(test)]
mod tests {
    use super::DEFAULT_SEARCH_RESULTS;
    use super::LocalCodeSearchArgs;
    use super::MAX_SEARCH_CONTEXT;
    use super::MAX_SEARCH_RESULTS;
    use super::arc_idx_args;

    fn args(mode: &str, query: &str) -> LocalCodeSearchArgs {
        LocalCodeSearchArgs {
            mode: mode.to_string(),
            query: query.to_string(),
            profile: None,
            language: None,
            kind: None,
            exact: false,
            max: None,
            context: None,
        }
    }

    #[test]
    fn search_defaults_and_caps_limits() {
        let defaults = arc_idx_args(&args("search", "UserService")).expect("search args");
        assert_eq!(
            defaults,
            [
                "search".to_string(),
                "UserService".to_string(),
                "--format".to_string(),
                "json".to_string(),
                "--max".to_string(),
                DEFAULT_SEARCH_RESULTS.to_string(),
            ]
        );

        let mut capped_args = args("search", "UserService");
        capped_args.max = Some(u32::MAX);
        capped_args.context = Some(u32::MAX);
        let capped = arc_idx_args(&capped_args).expect("capped search args");
        assert_eq!(
            capped,
            [
                "search".to_string(),
                "UserService".to_string(),
                "--format".to_string(),
                "json".to_string(),
                "--context".to_string(),
                MAX_SEARCH_CONTEXT.to_string(),
                "--max".to_string(),
                MAX_SEARCH_RESULTS.to_string(),
            ]
        );
    }

    #[test]
    fn maps_discovery_modes_to_json_output() {
        let mut symbol_args = args("symbol", "UserService");
        symbol_args.kind = Some("struct".to_string());
        symbol_args.exact = true;
        assert_eq!(
            arc_idx_args(&symbol_args).expect("symbol args"),
            [
                "symbol",
                "UserService",
                "--format",
                "json",
                "--kind",
                "struct",
                "--exact",
            ]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>()
        );

        let files = arc_idx_args(&args("files", "auth")).expect("files args");
        assert_eq!(
            files,
            ["files", "auth", "--format", "json"]
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>()
        );

        let mut ast_args = args("ast", "fn $NAME()");
        ast_args.language = Some("rust".to_string());
        assert_eq!(
            arc_idx_args(&ast_args).expect("ast args"),
            ["ast", "--lang", "rust", "fn $NAME()", "--format", "json"]
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>()
        );

        assert_eq!(
            arc_idx_args(&args("stats", "")).expect("stats args"),
            ["stats", "--format", "json"]
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>()
        );
    }
}
