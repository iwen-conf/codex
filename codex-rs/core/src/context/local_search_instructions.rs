use super::ContextualUserFragment;
use codex_protocol::models::ContentItemKind;

const OPEN_TAG: &str = "<kag_local_search>";
const CLOSE_TAG: &str = "</kag_local_search>";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LocalSearchInstructions {
    available: bool,
}

impl LocalSearchInstructions {
    pub(crate) fn new(available: bool) -> Self {
        Self { available }
    }
}

impl ContextualUserFragment for LocalSearchInstructions {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("kag.local_search_instructions".to_string())
    }

    fn role(&self) -> &'static str {
        "developer"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        (OPEN_TAG, CLOSE_TAG)
    }

    fn body(&self) -> String {
        if self.available {
            "\n## Local code search\n\
The `local_code_search` tool is available for this workspace. Use it as the primary tool for local repository discovery: `search` for content, `symbol` for symbols, `files` for file discovery, `ast` for structural search, and `stats` for index inventory.\n\
Prefer `local_code_search` over shell-based repository discovery such as `rg`, `grep`, `find`, `fd`, `git grep`, or recursive directory walks. Shell commands remain appropriate for builds, tests, formatters, git operations, and other non-discovery work.\n"
                .to_string()
        } else {
            "\n## Local code search\n\
The `local_code_search` tool is not available in the current execution environment. Use the normal Codex shell and execution tools for repository discovery as needed.\n"
                .to_string()
        }
    }
}
