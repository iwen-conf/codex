use std::collections::BTreeMap;
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::OnceLock;
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

pub(crate) const LOCAL_CODE_SEARCH_TOOL_NAME: &str = "local_code_search";
const PROTOCOL_NAME: &str = "ai-code-index";
const PROTOCOL_VERSION: u32 = 1;
const MODEL_OUTPUT_LIMIT_BYTES: usize = 100_000;
const PROCESS_OUTPUT_LIMIT_BYTES: usize = 512_000;
const DEFAULT_SEARCH_RESULTS: u32 = 50;
const MAX_SEARCH_RESULTS: u32 = 100;
const MAX_SEARCH_CONTEXT: u32 = 10;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(45);

static PROBE_CACHE: OnceLock<Mutex<HashMap<PathBuf, bool>>> = OnceLock::new();

#[derive(Debug, Deserialize)]
struct LocalCodeSearchArgs {
    mode: String,
    #[serde(default)]
    query: String,
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

#[derive(Debug, Deserialize)]
struct ProtocolCapabilities {
    name: String,
    protocol_version: u32,
    capabilities: ProtocolCapabilityFlags,
    formats: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ProtocolCapabilityFlags {
    search: bool,
    symbol: bool,
    files: bool,
    ast: bool,
    stats: bool,
}

#[derive(Clone, Debug)]
pub struct LocalCodeSearchHandler {
    executable: PathBuf,
}

impl LocalCodeSearchHandler {
    pub(crate) fn new(executable: PathBuf) -> Self {
        Self { executable }
    }

    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let ToolInvocation { turn, payload, .. } = invocation;
        let ToolPayload::Function { arguments } = payload else {
            return Err(FunctionCallError::RespondToModel(format!(
                "{LOCAL_CODE_SEARCH_TOOL_NAME} handler received unsupported payload"
            )));
        };
        let args: LocalCodeSearchArgs = parse_arguments(&arguments)?;
        let argv = ai_code_index_args(&args).map_err(FunctionCallError::RespondToModel)?;
        let cwd = turn.config.cwd.clone();

        let output = run_ai_code_index(&self.executable, &argv, cwd.as_path())
            .await
            .map_err(FunctionCallError::RespondToModel)?;
        let success = output.status.success();
        Ok(boxed_tool_output(FunctionToolOutput::from_text(
            format_ai_code_index_output(&output),
            Some(success),
        )))
    }
}

impl ToolExecutor<ToolInvocation> for LocalCodeSearchHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(LOCAL_CODE_SEARCH_TOOL_NAME)
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
                "When true, symbol mode requires an exact symbol name.".to_string(),
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
        name: LOCAL_CODE_SEARCH_TOOL_NAME.to_string(),
        description:
            "Local repository discovery backed by ai-code-index protocol v1. Finds files, symbols, content, AST patterns, and index inventory. Modes: search, symbol, files, ast, stats. Output is a JSON envelope whose data field contains the ai-code-index JSON response."
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

fn ai_code_index_args(args: &LocalCodeSearchArgs) -> Result<Vec<String>, String> {
    let mode = args.mode.trim();
    match mode {
        "search" => {
            if args.query.trim().is_empty() {
                return Err("search mode requires query".to_string());
            }
            let mut argv = vec![
                "search".to_string(),
                "--format".to_string(),
                "json".to_string(),
                "--max".to_string(),
                args.max
                    .unwrap_or(DEFAULT_SEARCH_RESULTS)
                    .clamp(1, MAX_SEARCH_RESULTS)
                    .to_string(),
            ];
            push_num(
                &mut argv,
                "--context",
                args.context.map(|value| value.min(MAX_SEARCH_CONTEXT)),
            );
            argv.push(args.query.clone());
            Ok(argv)
        }
        "symbol" => {
            if args.query.trim().is_empty() {
                return Err("symbol mode requires query".to_string());
            }
            let mut argv = vec![
                "symbol".to_string(),
                "--format".to_string(),
                "json".to_string(),
            ];
            push_opt(&mut argv, "--kind", args.kind.as_deref());
            if args.exact {
                argv.push("--exact".to_string());
            }
            argv.push(args.query.clone());
            Ok(argv)
        }
        "files" => {
            let mut argv = vec![
                "files".to_string(),
                "--format".to_string(),
                "json".to_string(),
            ];
            if !args.query.trim().is_empty() {
                argv.push(args.query.clone());
            }
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
                "--format".to_string(),
                "json".to_string(),
                "--lang".to_string(),
                language.to_string(),
                args.query.clone(),
            ])
        }
        "stats" => Ok(vec![
            "stats".to_string(),
            "--format".to_string(),
            "json".to_string(),
        ]),
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

struct LocalSearchOutput {
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

fn safe_environment() -> Vec<(OsString, OsString)> {
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
        "XDG_RUNTIME_DIR",
        "SYSTEMROOT",
        "WINDIR",
        "COMSPEC",
        "PATHEXT",
    ];

    let mut values = Vec::new();
    for key in SAFE_ENV_KEYS {
        if let Some(value) = std::env::var_os(key) {
            values.push((OsString::from(key), value));
        }
    }
    for (key, value) in std::env::vars_os() {
        let key_text = key.to_string_lossy();
        if key_text.starts_with("AI_CODE_INDEX_") || key_text.starts_with("LC_") {
            values.push((key, value));
        }
    }
    values
}

fn configure_async_environment(command: &mut tokio::process::Command) {
    command.env_clear();
    for (key, value) in safe_environment() {
        command.env(key, value);
    }
}

fn configure_probe_environment(command: &mut std::process::Command) {
    command.env_clear();
    for (key, value) in safe_environment() {
        command.env(key, value);
    }
}

async fn run_ai_code_index(
    exe: &Path,
    argv: &[String],
    cwd: &Path,
) -> Result<LocalSearchOutput, String> {
    let mut command = tokio::process::Command::new(exe);
    configure_async_environment(&mut command);
    command
        .args(argv)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    let mut child = command.spawn().map_err(|err| {
        format!(
            "local_code_search could not start `{}`: {err}. Install ai-code-index, set AI_CODE_INDEX_BIN, or configure [local_search].command.",
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

    Ok(LocalSearchOutput {
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

fn format_ai_code_index_output(output: &LocalSearchOutput) -> String {
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

pub(crate) fn local_code_search_backend(configured: Option<&str>) -> Option<PathBuf> {
    let executable = resolve_candidate(configured)?;
    protocol_compatible(&executable).then_some(executable)
}

fn resolve_candidate(configured: Option<&str>) -> Option<PathBuf> {
    if let Some(command) = configured
        .map(str::trim)
        .filter(|command| !command.is_empty())
    {
        return existing_command(command);
    }

    if let Ok(env_value) = std::env::var("AI_CODE_INDEX_BIN") {
        let env_value = env_value.trim();
        if !env_value.is_empty()
            && let Some(found) = existing_command(env_value)
        {
            return Some(found);
        }
    }

    if let Ok(found) = which::which("ai-code-index") {
        return Some(found);
    }

    local_bin_candidate("ai-code-index")
}

fn protocol_compatible(executable: &Path) -> bool {
    let cache = PROBE_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(cache) = cache.lock()
        && let Some(compatible) = cache.get(executable)
    {
        return *compatible;
    }

    let compatible = probe_protocol(executable);
    if let Ok(mut cache) = cache.lock() {
        cache.insert(executable.to_path_buf(), compatible);
    }
    compatible
}

fn probe_protocol(executable: &Path) -> bool {
    let mut command = std::process::Command::new(executable);
    configure_probe_environment(&mut command);
    let output = match command
        .args(["capabilities", "--json"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
    {
        Ok(output) if output.status.success() => output,
        Ok(_) | Err(_) => return false,
    };

    let capabilities: ProtocolCapabilities = match serde_json::from_slice(&output.stdout) {
        Ok(capabilities) => capabilities,
        Err(_) => return false,
    };
    capabilities.name == PROTOCOL_NAME
        && capabilities.protocol_version == PROTOCOL_VERSION
        && capabilities.formats.iter().any(|format| format == "json")
        && capabilities.capabilities.search
        && capabilities.capabilities.symbol
        && capabilities.capabilities.files
        && capabilities.capabilities.ast
        && capabilities.capabilities.stats
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
    use super::ai_code_index_args;

    fn args(mode: &str, query: &str) -> LocalCodeSearchArgs {
        LocalCodeSearchArgs {
            mode: mode.to_string(),
            query: query.to_string(),
            language: None,
            kind: None,
            exact: false,
            max: None,
            context: None,
        }
    }

    #[test]
    fn search_defaults_and_caps_limits() {
        let defaults = ai_code_index_args(&args("search", "UserService")).expect("search args");
        assert_eq!(
            defaults,
            [
                "search".to_string(),
                "--format".to_string(),
                "json".to_string(),
                "--max".to_string(),
                DEFAULT_SEARCH_RESULTS.to_string(),
                "UserService".to_string(),
            ]
        );

        let mut capped_args = args("search", "UserService");
        capped_args.max = Some(u32::MAX);
        capped_args.context = Some(u32::MAX);
        let capped = ai_code_index_args(&capped_args).expect("capped search args");
        assert_eq!(
            capped,
            [
                "search".to_string(),
                "--format".to_string(),
                "json".to_string(),
                "--max".to_string(),
                MAX_SEARCH_RESULTS.to_string(),
                "--context".to_string(),
                MAX_SEARCH_CONTEXT.to_string(),
                "UserService".to_string(),
            ]
        );
    }

    #[test]
    fn maps_discovery_modes_to_protocol_v1() {
        let mut symbol_args = args("symbol", "UserService");
        symbol_args.kind = Some("struct".to_string());
        symbol_args.exact = true;
        assert_eq!(
            ai_code_index_args(&symbol_args).expect("symbol args"),
            [
                "symbol",
                "--format",
                "json",
                "--kind",
                "struct",
                "--exact",
                "UserService",
            ]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>()
        );

        let files = ai_code_index_args(&args("files", "auth")).expect("files args");
        assert_eq!(
            files,
            ["files", "--format", "json", "auth"]
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>()
        );

        let mut ast_args = args("ast", "fn $NAME()");
        ast_args.language = Some("rust".to_string());
        assert_eq!(
            ai_code_index_args(&ast_args).expect("ast args"),
            ["ast", "--format", "json", "--lang", "rust", "fn $NAME()"]
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>()
        );

        assert_eq!(
            ai_code_index_args(&args("stats", "")).expect("stats args"),
            ["stats", "--format", "json"]
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>()
        );
    }
}
