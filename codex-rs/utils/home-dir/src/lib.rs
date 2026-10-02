use codex_utils_absolute_path::AbsolutePathBuf;
use dirs::home_dir;
use std::path::PathBuf;

/// Returns the path to the configuration directory.
///
/// Precedence is `KAG_HOME`, then `CODEX_HOME` (kept so existing tests and
/// docs keep working), then `~/.kag` when neither is set.
///
/// - If `KAG_HOME` or `CODEX_HOME` is set, the value must exist and be a
///   directory. The value will be canonicalized and this function will Err
///   otherwise.
/// - If neither is set, this function does not verify that the directory exists.
pub fn find_codex_home() -> std::io::Result<AbsolutePathBuf> {
    let kag_home = non_empty_env("KAG_HOME");
    let codex_home = non_empty_env("CODEX_HOME");
    let (home_env, label) = select_home_env(kag_home.as_deref(), codex_home.as_deref());
    find_codex_home_from_env(home_env, label)
}

fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|val| !val.is_empty())
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.filter(|val| !val.is_empty())
}

/// `KAG_HOME` wins when it is non-empty; otherwise `CODEX_HOME`.
fn select_home_env<'a>(
    kag_home: Option<&'a str>,
    codex_home: Option<&'a str>,
) -> (Option<&'a str>, &'static str) {
    if let Some(value) = non_empty(kag_home) {
        (Some(value), "KAG_HOME")
    } else if let Some(value) = non_empty(codex_home) {
        (Some(value), "CODEX_HOME")
    } else {
        (None, "KAG_HOME")
    }
}

fn find_codex_home_from_env(
    home_env: Option<&str>,
    label: &str,
) -> std::io::Result<AbsolutePathBuf> {
    // Honor `KAG_HOME`, then `CODEX_HOME`, so users and tests can override the
    // default `~/.kag` location.
    match home_env {
        Some(val) => {
            let path = PathBuf::from(val);
            let metadata = std::fs::metadata(&path).map_err(|err| match err.kind() {
                std::io::ErrorKind::NotFound => std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("{label} points to {val:?}, but that path does not exist"),
                ),
                _ => std::io::Error::new(
                    err.kind(),
                    format!("failed to read {label} {val:?}: {err}"),
                ),
            })?;

            if !metadata.is_dir() {
                Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("{label} points to {val:?}, but that path is not a directory"),
                ))
            } else {
                let canonical = path.canonicalize().map_err(|err| {
                    std::io::Error::new(
                        err.kind(),
                        format!("failed to canonicalize {label} {val:?}: {err}"),
                    )
                })?;
                AbsolutePathBuf::from_absolute_path(canonical)
            }
        }
        None => {
            let mut p = home_dir().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "Could not find home directory",
                )
            })?;
            p.push(".kag");
            AbsolutePathBuf::from_absolute_path(p)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::find_codex_home_from_env;
    use super::select_home_env;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use dirs::home_dir;
    use pretty_assertions::assert_eq;
    use std::fs;
    use std::io::ErrorKind;
    use tempfile::TempDir;

    #[test]
    fn find_codex_home_env_missing_path_is_fatal() {
        let temp_home = TempDir::new().expect("temp home");
        let missing = temp_home.path().join("missing-codex-home");
        let missing_str = missing
            .to_str()
            .expect("missing codex home path should be valid utf-8");

        let err = find_codex_home_from_env(Some(missing_str), "CODEX_HOME")
            .expect_err("missing CODEX_HOME");
        assert_eq!(err.kind(), ErrorKind::NotFound);
        assert!(
            err.to_string().contains("CODEX_HOME"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn find_codex_home_env_file_path_is_fatal() {
        let temp_home = TempDir::new().expect("temp home");
        let file_path = temp_home.path().join("codex-home.txt");
        fs::write(&file_path, "not a directory").expect("write temp file");
        let file_str = file_path
            .to_str()
            .expect("file codex home path should be valid utf-8");

        let err = find_codex_home_from_env(Some(file_str), "KAG_HOME").expect_err("file KAG_HOME");
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert!(
            err.to_string().contains("not a directory"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn find_codex_home_env_valid_directory_canonicalizes() {
        let temp_home = TempDir::new().expect("temp home");
        let temp_str = temp_home
            .path()
            .to_str()
            .expect("temp codex home path should be valid utf-8");

        let resolved = find_codex_home_from_env(Some(temp_str), "KAG_HOME").expect("valid home");
        let expected = temp_home
            .path()
            .canonicalize()
            .expect("canonicalize temp home");
        let expected = AbsolutePathBuf::from_absolute_path(expected).expect("absolute home");
        assert_eq!(resolved, expected);
    }

    #[test]
    fn find_codex_home_without_env_uses_default_home_dir() {
        let resolved =
            find_codex_home_from_env(/*home_env*/ None, "KAG_HOME").expect("default home");
        let mut expected = home_dir().expect("home dir");
        expected.push(".kag");
        let expected = AbsolutePathBuf::from_absolute_path(expected).expect("absolute home");
        assert_eq!(resolved, expected);
    }

    #[test]
    fn kag_home_precedes_codex_home() {
        assert_eq!(
            select_home_env(Some("/kag"), Some("/codex")),
            (Some("/kag"), "KAG_HOME")
        );
        assert_eq!(
            select_home_env(Some(""), Some("/codex")),
            (Some("/codex"), "CODEX_HOME")
        );
        assert_eq!(
            select_home_env(None, Some("/codex")),
            (Some("/codex"), "CODEX_HOME")
        );
        assert_eq!(select_home_env(None, None), (None, "KAG_HOME"));
    }
}
