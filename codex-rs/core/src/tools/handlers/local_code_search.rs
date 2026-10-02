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
use codex_config::config_toml::LocalSearchToml;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;
use serde_json::json;

const TOOL_NAME: &str = "local_code_search";
const SEARCH_BLOCK_MESSAGE: &str = "This command looks like local codebase discovery or a tree walk (`rg`/`grep`/`find`/`fd`, `ls -R`, `git grep`, `git ls-files`, a `.ai-code-index/*.sh` wrapper, or a `python`/`node` one-liner using os.walk, rglob, or readdir). Use the `local_code_search` tool instead (mode: search, symbol, files, ast, stats; doctor or daemon for index hygiene).";
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
                    "search (zoekt), symbol (ctags), files, ast (ast-grep), stats, doctor, or daemon. Prefer search|symbol|ast|files|stats. Use doctor/daemon for index hygiene."
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
            "Local repo discovery via the arc-idx CLI (zoekt, ctags, ast-grep, files, stats). \
Required for codebase search and inventory when the binary is available. \
Do not use rg, grep, find, fd, ls -R, git grep, git ls-files, .ai-code-index shell wrappers, \
or python/node tree walks as the primary search. \
Modes: search, symbol, files, ast, stats, doctor, daemon. Output is JSON."
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
Shell discovery stays available until that binary exists.",
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

/// Shell search is blocked only when local search is enabled and arc-idx exists.
pub(crate) fn local_search_shell_guard_active(local_search: &LocalSearchToml) -> bool {
    local_search.is_enabled() && resolve_existing_arc_idx(local_search.command.as_deref()).is_some()
}

pub(crate) fn arc_idx_executable(configured: Option<&str>) -> PathBuf {
    resolve_existing_arc_idx(configured).unwrap_or_else(|| {
        configured
            .map(str::trim)
            .filter(|command| !command.is_empty())
            .map_or_else(|| PathBuf::from("arc-idx"), PathBuf::from)
    })
}

pub(crate) fn resolve_existing_arc_idx(configured: Option<&str>) -> Option<PathBuf> {
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

pub(crate) fn codebase_search_block_reason(command: &str) -> Option<&'static str> {
    block_reason(command, 0)
}

pub(crate) fn stdin_codebase_search_block_reason(chars: &str) -> Option<&'static str> {
    let lines = chars
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    if lines.len() > 3 {
        return None;
    }
    lines.into_iter().find_map(codebase_search_block_reason)
}

fn block_reason(command: &str, depth: u8) -> Option<&'static str> {
    if depth > 2 {
        return None;
    }
    for statement in split_statements(command) {
        let stage = first_pipeline_stage(&statement);
        if stage_is_code_search(&stage, depth) {
            return Some(SEARCH_BLOCK_MESSAGE);
        }
    }
    None
}

fn stage_is_code_search(stage: &str, depth: u8) -> bool {
    let Some(tokens) = shlex::split(stage) else {
        return false;
    };
    let tokens = skip_env_and_wrappers(tokens);
    let Some((exe, rest)) = tokens.split_first() else {
        return false;
    };
    if token_is_ai_code_index_script(exe)
        || rest
            .iter()
            .any(|token| token_is_ai_code_index_script(token))
    {
        return true;
    }
    if let Some(script) = shell_c_script(exe, rest) {
        return block_reason(script, depth + 1).is_some();
    }
    if is_help_or_version(rest) {
        return false;
    }
    let name = exe_basename(exe);
    match name.as_str() {
        "rg" | "rga" => true,
        "fd" | "fdfind" => true,
        "grep" | "egrep" | "fgrep" => grep_is_code_search(rest),
        "find" => find_is_code_search(rest),
        "ls" => ls_is_recursive(rest),
        "git" => git_is_inventory(rest),
        "node" | "nodejs" => inline_discovery(rest, &["-e", "--eval"]),
        other if is_python(other) => inline_discovery(rest, &["-c"]),
        _ => false,
    }
}

fn token_is_ai_code_index_script(token: &str) -> bool {
    let normalized = token.replace('\\', "/").to_ascii_lowercase();
    normalized.contains(".ai-code-index/") && normalized.ends_with(".sh")
}

fn is_python(name: &str) -> bool {
    name == "python" || name == "python2" || name.starts_with("python3")
}

fn ls_is_recursive(args: &[String]) -> bool {
    args.iter().any(|arg| {
        if arg == "--recursive" {
            return true;
        }
        let Some(flags) = arg.strip_prefix('-') else {
            return false;
        };
        !arg.starts_with("--") && flags.contains('R')
    })
}

fn git_is_inventory(args: &[String]) -> bool {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            index += 1;
            continue;
        }
        if matches!(
            arg.as_str(),
            "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace" | "--config-env"
        ) {
            index += 2;
            continue;
        }
        if arg.starts_with("--git-dir=")
            || arg.starts_with("--work-tree=")
            || arg.starts_with("--namespace=")
            || (arg.starts_with("-c") && arg.contains('='))
        {
            index += 1;
            continue;
        }
        if arg.starts_with('-') {
            index += 1;
            continue;
        }
        return arg == "grep" || arg == "ls-files";
    }
    false
}

fn inline_discovery(args: &[String], flags: &[&str]) -> bool {
    inline_flag_value(args, flags).is_some_and(|code| discovery_needles(&code))
}

fn inline_flag_value(args: &[String], flags: &[&str]) -> Option<String> {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            return None;
        }
        for flag in flags {
            if arg == *flag {
                return args.get(index + 1).cloned();
            }
            if flag.starts_with("--")
                && let Some(value) = arg
                    .strip_prefix(flag)
                    .and_then(|rest| rest.strip_prefix('='))
                && !value.is_empty()
            {
                return Some(value.to_string());
            }
        }
        for flag in flags {
            if flag.starts_with("--") || arg.starts_with("--") {
                continue;
            }
            if let Some(value) = arg.strip_prefix(flag)
                && !value.is_empty()
            {
                return Some(value.to_string());
            }
        }
        index += 1;
    }
    None
}

fn discovery_needles(code: &str) -> bool {
    let lower = code.to_ascii_lowercase();
    const NEEDLES: &[&str] = &[
        "os.walk",
        "os.listdir",
        "os.scandir",
        "rglob",
        "glob.glob",
        "glob.iglob",
        ".ai-code-index",
        "fs.readdir",
        "readdirsync",
        "readdir(",
        "recursive:true",
        "recursive: true",
        "fast-glob",
        "glob.sync",
        "walkdir",
    ];
    NEEDLES.iter().any(|needle| lower.contains(needle))
}

fn skip_env_and_wrappers(mut tokens: Vec<String>) -> Vec<String> {
    while tokens.first().is_some_and(|token| is_env_assignment(token)) {
        tokens.remove(0);
    }
    while tokens.first().is_some_and(|token| {
        matches!(
            exe_basename(token).as_str(),
            "command" | "exec" | "time" | "nice" | "nohup"
        )
    }) {
        tokens.remove(0);
    }
    tokens
}

fn is_env_assignment(token: &str) -> bool {
    let Some((name, _)) = token.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => {
            chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        }
        _ => false,
    }
}

fn shell_c_script<'a>(exe: &str, args: &'a [String]) -> Option<&'a str> {
    if !matches!(exe_basename(exe).as_str(), "bash" | "sh" | "zsh" | "dash") {
        return None;
    }
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "-c" {
            return args.get(index + 1).map(String::as_str);
        }
        if arg == "--" {
            return None;
        }
        if let Some(flags) = arg.strip_prefix('-')
            && !arg.starts_with("--")
            && flags.contains('c')
        {
            return args.get(index + 1).map(String::as_str);
        }
        if arg.starts_with('-') {
            index += 1;
            continue;
        }
        break;
    }
    None
}

fn is_help_or_version(args: &[String]) -> bool {
    !args.is_empty()
        && args.iter().all(|arg| {
            matches!(
                arg.as_str(),
                "--help" | "-h" | "--version" | "-V" | "-version" | "--"
            )
        })
}

fn grep_is_code_search(args: &[String]) -> bool {
    if has_recursive_grep(args) {
        return true;
    }
    grep_paths(args)
        .iter()
        .any(|path| looks_like_directory_search(path))
}

fn has_recursive_grep(args: &[String]) -> bool {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            break;
        }
        if arg == "--recursive" || arg == "-d" || arg == "--directories" {
            if arg == "-d" || arg == "--directories" {
                if args.get(index + 1).map(String::as_str) == Some("recurse") {
                    return true;
                }
            } else {
                return true;
            }
        }
        if let Some(value) = arg.strip_prefix("--directories=")
            && value == "recurse"
        {
            return true;
        }
        if let Some(flags) = arg.strip_prefix('-')
            && !arg.starts_with("--")
            && flags.chars().any(|flag| flag == 'r' || flag == 'R')
        {
            return true;
        }
        index += 1;
    }
    false
}

fn grep_paths(args: &[String]) -> Vec<&str> {
    let value_flags = [
        "-e",
        "-f",
        "--regexp",
        "--file",
        "-m",
        "--max-count",
        "--include",
        "--exclude",
        "--exclude-dir",
        "-d",
        "--directories",
    ];
    let mut positionals = Vec::new();
    let mut index = 0;
    let mut pattern_from_flag = false;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            positionals.extend(args.iter().skip(index + 1).map(String::as_str));
            break;
        }
        if let Some(flag) = value_flags.iter().find(|flag| arg == **flag) {
            if matches!(*flag, "-e" | "-f" | "--regexp" | "--file") {
                pattern_from_flag = true;
            }
            index += 2;
            continue;
        }
        if arg.starts_with("--regexp=") || arg.starts_with("--file=") {
            pattern_from_flag = true;
            index += 1;
            continue;
        }
        if arg.starts_with('-') && arg != "-" {
            index += 1;
            continue;
        }
        positionals.push(arg.as_str());
        index += 1;
    }
    if pattern_from_flag {
        positionals
    } else {
        positionals.into_iter().skip(1).collect()
    }
}

fn looks_like_directory_search(path: &str) -> bool {
    let path = path.trim_end_matches(['/', '\\']);
    if path.is_empty() || path == "." || path == ".." {
        return true;
    }
    let name = Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path);
    !name.contains('.')
}

fn find_is_code_search(args: &[String]) -> bool {
    if args.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "-name" | "-iname" | "-path" | "-ipath" | "-regex" | "-iregex" | "-wholename" | "-type"
        )
    }) {
        return true;
    }
    args.iter()
        .find(|arg| !arg.starts_with('-'))
        .is_some_and(|path| path == "." || path == ".." || path == "./" || path == ".\\")
}

fn exe_basename(token: &str) -> String {
    let name = Path::new(token)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(token);
    name.strip_suffix(".exe")
        .unwrap_or(name)
        .to_ascii_lowercase()
}

fn split_statements(input: &str) -> Vec<String> {
    let mut statements = Vec::new();
    let mut current = String::new();
    let mut chars = input.chars().peekable();
    let mut quote: Option<char> = None;
    while let Some(ch) = chars.next() {
        if let Some(active) = quote {
            current.push(ch);
            if ch == active {
                quote = None;
            }
            continue;
        }
        match ch {
            '\'' | '"' => {
                quote = Some(ch);
                current.push(ch);
            }
            ';' | '\n' => push_piece(&mut statements, &mut current),
            '&' if chars.peek() == Some(&'&') => {
                chars.next();
                push_piece(&mut statements, &mut current);
            }
            '|' if chars.peek() == Some(&'|') => {
                chars.next();
                push_piece(&mut statements, &mut current);
            }
            _ => current.push(ch),
        }
    }
    push_piece(&mut statements, &mut current);
    statements
}

fn first_pipeline_stage(statement: &str) -> String {
    let mut current = String::new();
    let mut chars = statement.chars().peekable();
    let mut quote: Option<char> = None;
    while let Some(ch) = chars.next() {
        if let Some(active) = quote {
            current.push(ch);
            if ch == active {
                quote = None;
            }
            continue;
        }
        match ch {
            '\'' | '"' => {
                quote = Some(ch);
                current.push(ch);
            }
            '|' => break,
            _ => current.push(ch),
        }
    }
    current
}

fn push_piece(pieces: &mut Vec<String>, current: &mut String) {
    let trimmed = current.trim();
    if !trimmed.is_empty() {
        pieces.push(trimmed.to_string());
    }
    current.clear();
}

#[cfg(test)]
mod tests {
    use super::LocalCodeSearchArgs;
    use super::arc_idx_args;
    use super::codebase_search_block_reason;
    use super::stdin_codebase_search_block_reason;

    fn blocked(command: &str) -> bool {
        codebase_search_block_reason(command).is_some()
    }

    #[test]
    fn blocks_obvious_code_search() {
        assert!(blocked("rg pattern"));
        assert!(blocked("rg --files"));
        assert!(blocked("rg --files src"));
        assert!(blocked("  FOO=1 rg -n foo src"));
        assert!(blocked("grep -R pattern ."));
        assert!(blocked("grep -rin TODO src"));
        assert!(blocked("grep -n pattern src"));
        assert!(blocked("find . -name '*.rs'"));
        assert!(blocked("find . -type f"));
        assert!(blocked("fd pattern"));
        assert!(blocked("fd -e rs foo"));
        assert!(blocked("rg foo | head"));
        assert!(blocked("echo hello && rg foo"));
        assert!(blocked("bash -lc 'rg --files'"));
        assert!(blocked("command rg pattern"));
        assert!(blocked("ls -R"));
        assert!(blocked("ls -laR src"));
        assert!(blocked("git grep TODO src"));
        assert!(blocked("git -C repo ls-files"));
        assert!(blocked("./.ai-code-index/search.sh pattern"));
        assert!(blocked("bash .ai-code-index/files.sh"));
        assert!(blocked("python -c \"import os; os.walk('.')\""));
        assert!(blocked(
            "python3 -c 'from pathlib import Path; Path(\".\").rglob(\"*.rs\")'"
        ));
        assert!(blocked(
            "node -e \"require('fs').readdirSync('.',{recursive:true})\""
        ));
        assert!(blocked("node --eval \"require('fs').readdir('.')\""));
    }

    #[test]
    fn allows_non_search_commands() {
        assert!(!blocked("rg --version"));
        assert!(!blocked("rg --help"));
        assert!(!blocked("grep --version"));
        assert!(!blocked("grep pattern file.rs"));
        assert!(!blocked("grep pattern src/lib.rs"));
        assert!(!blocked("cat file | rg pattern"));
        assert!(!blocked("find /var/log -mtime -1"));
        assert!(!blocked("find --version"));
        assert!(!blocked("fd --version"));
        assert!(!blocked("cargo test"));
        assert!(!blocked("make rg"));
        assert!(!blocked("./scripts/build.sh"));
        assert!(!blocked("echo rg pattern"));
        assert!(!blocked("git status"));
        assert!(!blocked("git diff"));
        assert!(!blocked("ls -l"));
        assert!(!blocked("ls src"));
        assert!(!blocked("python script.py"));
        assert!(!blocked("python -c \"print(1)\""));
        assert!(!blocked("node -e \"console.log('rg')\""));
        assert!(!blocked("node dist/index.js"));
        assert!(!blocked("pytest"));
        assert!(!blocked("npm test"));
        assert!(!blocked("cargo build --features rg"));
    }

    #[test]
    fn stdin_guard_ignores_large_pastes() {
        let paste = "line\n".repeat(5);
        assert!(stdin_codebase_search_block_reason(&paste).is_none());
        assert!(stdin_codebase_search_block_reason("rg --files\n").is_some());
    }

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
