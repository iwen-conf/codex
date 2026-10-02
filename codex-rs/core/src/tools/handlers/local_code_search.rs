use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

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
const OUTPUT_LIMIT_BYTES: usize = 100_000;
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
    /// Daemon verb when `mode` is `daemon`: start, status, stop, or restart.
    #[serde(default)]
    action: Option<String>,
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
            format_arc_idx_output(&exe, &argv, &output),
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
                    json!("doctor"),
                    json!("daemon"),
                ],
                Some(
                    "search (content), symbol, files, ast (AST patterns), stats, doctor, or daemon. Use search, symbol, files, or ast for discovery. Use doctor or daemon for index hygiene."
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
        (
            "action".to_string(),
            JsonSchema::string_enum(
                vec![
                    json!("start"),
                    json!("status"),
                    json!("stop"),
                    json!("restart"),
                ],
                Some(
                    "Daemon action when mode is daemon. Defaults to status.".to_string(),
                ),
            ),
        ),
    ]);

    ToolSpec::Function(ResponsesApiTool {
        name: TOOL_NAME.to_string(),
        description:
            "The only first-class local code discovery tool. Finds files, symbols, content, and AST \
patterns by invoking the arc-idx CLI (Mac: arc-idx or arc). Modes: search, symbol, files, ast; \
stats for inventory; doctor and daemon for index hygiene. Do not use shell rg, grep, find, fd, \
git grep, recursive ls, .ai-code-index/*.sh, or ad-hoc python -c / node -e tree walks for retrieval. \
Output is JSON."
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
            push_num(&mut argv, "--context", args.context);
            push_num(&mut argv, "--max", args.max);
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
        "doctor" => Ok(vec![
            "doctor".to_string(),
            "--format".to_string(),
            "json".to_string(),
        ]),
        "daemon" => {
            let action = args
                .action
                .as_deref()
                .map(str::trim)
                .filter(|action| !action.is_empty())
                .unwrap_or("status");
            if !matches!(action, "start" | "status" | "stop" | "restart") {
                return Err("daemon action must be start, status, stop, or restart".to_string());
            }
            // `daemon status` already prints JSON. Do not wrap it in a shell.
            Ok(vec!["daemon".to_string(), action.to_string()])
        }
        other => Err(format!(
            "unknown local_code_search mode `{other}`; expected search, symbol, files, ast, stats, doctor, or daemon"
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

struct ArcIdxOutput {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
}

async fn run_arc_idx(exe: &Path, argv: &[String], cwd: &Path) -> Result<ArcIdxOutput, String> {
    // Invoke the binary directly. Never route this through `sh -c`.
    let mut command = tokio::process::Command::new(exe);
    command
        .args(argv)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let child = command.spawn().map_err(|err| {
        format!(
            "local_code_search could not start `{}`: {err}. Install arc-idx or set [local_search].command \
(resolution: ARC_IDX_BIN, then `arc-idx` on PATH, then ~/.local/bin/arc-idx). \
Do not use shell rg, grep, find, fd, git grep, or recursive ls for retrieval.",
            exe.display()
        )
    })?;
    let output = tokio::time::timeout(COMMAND_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| {
            format!(
                "local_code_search timed out after {}s running `{}`",
                COMMAND_TIMEOUT.as_secs(),
                exe.display()
            )
        })?
        .map_err(|err| {
            format!(
                "local_code_search failed while running `{}`: {err}",
                exe.display()
            )
        })?;
    Ok(ArcIdxOutput {
        status: output.status,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

fn format_arc_idx_output(exe: &Path, argv: &[String], output: &ArcIdxOutput) -> String {
    let mut body = format!(
        "exit_code: {}\ncommand: {} {}\n",
        output
            .status
            .code()
            .map_or_else(|| "signal".to_string(), |code| code.to_string()),
        exe.display(),
        argv.join(" ")
    );
    if !output.stdout.trim().is_empty() {
        body.push_str(&output.stdout);
        if !body.ends_with('\n') {
            body.push('\n');
        }
    }
    if !output.stderr.trim().is_empty() {
        body.push_str("stderr:\n");
        body.push_str(&output.stderr);
    }
    truncate_output(body)
}

fn truncate_output(mut text: String) -> String {
    if text.len() <= OUTPUT_LIMIT_BYTES {
        return text;
    }
    let mut end = OUTPUT_LIMIT_BYTES;
    while !text.is_char_boundary(end) && end > 0 {
        end -= 1;
    }
    text.truncate(end);
    text.push_str("\n...[truncated]");
    text
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
        return path.is_file().then_some(path);
    }
    if let Ok(found) = which::which(command) {
        return Some(found);
    }
    if command == "arc-idx" {
        return local_bin_arc_idx();
    }
    None
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
    local_bin_arc_idx()
}

fn local_bin_arc_idx() -> Option<PathBuf> {
    let path = dirs::home_dir()?.join(".local").join("bin").join("arc-idx");
    path.is_file().then_some(path)
}

#[cfg(test)]
mod tests {
    use super::LocalCodeSearchArgs;
    use super::arc_idx_args;

    #[test]
    fn maps_modes_to_arc_idx_argv() {
        let search = arc_idx_args(&LocalCodeSearchArgs {
            mode: "search".to_string(),
            query: "sym:UserService".to_string(),
            profile: Some("backend".to_string()),
            language: None,
            kind: None,
            exact: false,
            max: Some(20),
            context: Some(3),
            action: None,
        })
        .expect("search args");
        assert_eq!(
            search,
            vec![
                "search",
                "sym:UserService",
                "--format",
                "json",
                "--profile",
                "backend",
                "--context",
                "3",
                "--max",
                "20",
            ]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>()
        );

        let ast = arc_idx_args(&LocalCodeSearchArgs {
            mode: "ast".to_string(),
            query: "fn $NAME()".to_string(),
            profile: None,
            language: Some("rust".to_string()),
            kind: None,
            exact: false,
            max: None,
            context: None,
            action: None,
        })
        .expect("ast args");
        assert_eq!(
            ast,
            ["ast", "--lang", "rust", "fn $NAME()", "--format", "json"]
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>()
        );

        let stats = arc_idx_args(&LocalCodeSearchArgs {
            mode: "stats".to_string(),
            query: String::new(),
            profile: Some("backend".to_string()),
            language: None,
            kind: None,
            exact: false,
            max: None,
            context: None,
            action: None,
        })
        .expect("stats args");
        assert_eq!(
            stats,
            ["stats", "--format", "json", "--profile", "backend"]
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>()
        );

        let doctor = arc_idx_args(&LocalCodeSearchArgs {
            mode: "doctor".to_string(),
            query: String::new(),
            profile: None,
            language: None,
            kind: None,
            exact: false,
            max: None,
            context: None,
            action: None,
        })
        .expect("doctor args");
        assert_eq!(
            doctor,
            ["doctor", "--format", "json"]
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>()
        );

        let daemon = arc_idx_args(&LocalCodeSearchArgs {
            mode: "daemon".to_string(),
            query: String::new(),
            profile: None,
            language: None,
            kind: None,
            exact: false,
            max: None,
            context: None,
            action: Some("restart".to_string()),
        })
        .expect("daemon args");
        assert_eq!(
            daemon,
            ["daemon", "restart"]
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>()
        );
    }
}
