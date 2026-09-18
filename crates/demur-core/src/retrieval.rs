//! Resolving context a pass asked for.
//!
//! A pass returns names. This module decides what a name means and whether
//! the bot is willing to read it. Nothing here executes anything: a request
//! shaped like a command is simply a name that does not resolve to a file,
//! so it fails the way a typo fails and nothing has to detect intent.

use std::path::{Path, PathBuf};

use crate::config::Config;

/// Something a pass asked to see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// An exact repository path.
    File(String),
    /// A name to find a definition of.
    Symbol(String),
}

impl Request {
    /// Parse one request. Anything that is not a recognized form is an
    /// unrecognized name rather than an error, because a pass is allowed to
    /// ask for something the bot does not understand.
    pub fn parse(raw: &str) -> Request {
        match raw.split_once(':') {
            Some(("file", rest)) => Request::File(rest.trim().to_string()),
            Some(("symbol", rest)) => Request::Symbol(rest.trim().to_string()),
            _ => Request::Symbol(raw.trim().to_string()),
        }
    }

    /// How the request reads back to the pass that made it.
    pub fn label(&self) -> String {
        match self {
            Request::File(path) => format!("file:{path}"),
            Request::Symbol(name) => format!("symbol:{name}"),
        }
    }
}

/// What came of a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// Content the bot was willing to read.
    Found {
        /// The request this answers.
        label: String,
        /// Where it came from, for disclosure.
        origin: String,
        /// The content itself, untrusted like any other repository data.
        content: String,
    },
    /// The allowlist refused it. The pass is told, without being told what
    /// the allowlist contains.
    Refused {
        /// The request this answers.
        label: String,
    },
    /// Nothing allowed matched.
    NotFound {
        /// The request this answers.
        label: String,
    },
}

impl Resolution {
    /// The request this resolution answers.
    pub fn label(&self) -> &str {
        match self {
            Resolution::Found { label, .. }
            | Resolution::Refused { label }
            | Resolution::NotFound { label } => label,
        }
    }
}

/// Lines of context returned around a symbol definition.
const SYMBOL_WINDOW: usize = 40;

/// Reads repository content on a pass's behalf, inside an allowlist.
pub struct Retriever {
    root: PathBuf,
    ignore: globset::GlobSet,
    refused: Vec<PathBuf>,
}

impl Retriever {
    /// Build a retriever rooted at the checkout. The root is canonicalized
    /// so that every later comparison is against a real path rather than
    /// one a link could point away from.
    pub fn new(root: &Path, config: &Config) -> Option<Retriever> {
        let root = root.canonicalize().ok()?;
        // Refused wherever they point. A repository can put its key file
        // inside the checkout, and a pull request that induced the bot to
        // read it would have put a credential into a prompt.
        let mut refused = vec![
            root.join(".git"),
            root.join(crate::config::CONFIG_FILE_NAME),
        ];
        for provider in config.providers.values() {
            if let Some(key_file) = &provider.key_file {
                refused.push(key_file.clone());
                if let Ok(canonical) = key_file.canonicalize() {
                    refused.push(canonical);
                }
            }
        }
        Some(Retriever {
            root,
            // The same globs that decide what gets reviewed decide what
            // can be retrieved: content excluded from one is excluded from
            // the other.
            ignore: crate::diff::build_ignore_set(&config.ignore.paths),
            refused,
        })
    }

    /// Resolve one request.
    pub fn resolve(&self, request: &Request) -> Resolution {
        let label = request.label();
        match request {
            Request::File(path) => match self.read_allowed(Path::new(path)) {
                Allowed::Content(content) => Resolution::Found {
                    label,
                    origin: path.clone(),
                    content,
                },
                Allowed::Refused => Resolution::Refused { label },
                Allowed::Missing => Resolution::NotFound { label },
            },
            Request::Symbol(name) => match self.find_definition(name) {
                Some((origin, content)) => Resolution::Found {
                    label,
                    origin,
                    content,
                },
                None => Resolution::NotFound { label },
            },
        }
    }

    /// Read a path if the allowlist admits it.
    fn read_allowed(&self, candidate: &Path) -> Allowed {
        if candidate.is_absolute() {
            return Allowed::Refused;
        }
        let joined = self.root.join(candidate);
        // Canonicalizing resolves both traversal and symbolic links, so a
        // single containment check covers `../..` and a link pointing out
        // of the checkout.
        let Ok(resolved) = joined.canonicalize() else {
            return Allowed::Missing;
        };
        if !resolved.starts_with(&self.root) || !resolved.is_file() {
            return Allowed::Refused;
        }
        if self.refused.iter().any(|path| resolved.starts_with(path)) {
            return Allowed::Refused;
        }
        let relative = resolved
            .strip_prefix(&self.root)
            .unwrap_or(&resolved)
            .to_string_lossy()
            .replace('\\', "/");
        if crate::diff::is_ignored(&self.ignore, &relative) {
            return Allowed::Refused;
        }
        match std::fs::read_to_string(&resolved) {
            Ok(content) => Allowed::Content(content),
            Err(_) => Allowed::Missing,
        }
    }

    /// Search allowed files for something that looks like a definition of
    /// `name`. This is a textual search, not an index: it is approximate on
    /// purpose, and an unanswered request is reported rather than hidden.
    fn find_definition(&self, name: &str) -> Option<(String, String)> {
        if name.is_empty() || name.contains(['/', ' ', '\t']) {
            return None;
        }
        let mut stack = vec![self.root.clone()];
        while let Some(dir) = stack.pop() {
            let entries = std::fs::read_dir(&dir).ok()?;
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    if path.file_name().is_some_and(|name| name == ".git") {
                        continue;
                    }
                    stack.push(path);
                    continue;
                }
                let Ok(relative) = path.strip_prefix(&self.root) else {
                    continue;
                };
                let relative = relative.to_string_lossy().replace('\\', "/");
                let Allowed::Content(content) = self.read_allowed(Path::new(&relative)) else {
                    continue;
                };
                if let Some(window) = definition_window(&content, name) {
                    return Some((relative, window));
                }
            }
        }
        None
    }
}

enum Allowed {
    Content(String),
    Refused,
    Missing,
}

/// Keywords that introduce a definition across the languages this is
/// likely to meet. Approximate by design.
const DEFINITION_KEYWORDS: &[&str] = &[
    "fn",
    "func",
    "def",
    "function",
    "class",
    "struct",
    "enum",
    "trait",
    "interface",
    "type",
    "impl",
    "const",
    "let",
    "var",
    "public",
    "private",
    "protected",
    "static",
    "async",
];

/// The lines around the first line that looks like a definition of `name`.
fn definition_window(content: &str, name: &str) -> Option<String> {
    let lines: Vec<&str> = content.lines().collect();
    let index = lines.iter().position(|line| is_definition(line, name))?;
    let start = index.saturating_sub(2);
    let end = (index + SYMBOL_WINDOW).min(lines.len());
    Some(lines[start..end].join("\n"))
}

fn is_definition(line: &str, name: &str) -> bool {
    let trimmed = line.trim_start();
    let Some(at) = trimmed.find(name) else {
        return false;
    };
    // The name must stand alone rather than be part of a longer word.
    let before = trimmed[..at].chars().next_back();
    let after = trimmed[at + name.len()..].chars().next();
    let bounded = before.is_none_or(|c| !c.is_alphanumeric() && c != '_')
        && after.is_none_or(|c| !c.is_alphanumeric() && c != '_');
    if !bounded {
        return false;
    }
    let prefix = &trimmed[..at];
    DEFINITION_KEYWORDS
        .iter()
        .any(|keyword| prefix.split_whitespace().any(|word| word == *keyword))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checkout() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(
            dir.path().join("src/auth.rs"),
            "use x;\n\npub fn verify_token(t: &str) -> bool {\n    t.len() > 3\n}\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("Cargo.lock"), "lockfile\n").unwrap();
        std::fs::write(dir.path().join(".git/config"), "secret\n").unwrap();
        std::fs::write(dir.path().join(".demur.toml"), "config\n").unwrap();
        dir
    }

    fn config_for(dir: &Path, key_file: Option<&Path>) -> Config {
        let key_line = key_file
            .map(|p| format!("key_file = \"{}\"\n", p.display()))
            .unwrap_or_default();
        let text = format!(
            r#"
[ignore]
paths = ["**/Cargo.lock"]

[providers.p]
family = "openai"
base_url = "https://api.test/v1"
key_env = "K"
{key_line}
[models.triage]
provider = "p"
name = "m"
input_price = 1.0
output_price = 1.0

[models.deep]
provider = "p"
name = "m"
input_price = 1.0
output_price = 1.0

[models.verdict]
provider = "p"
name = "m"
input_price = 1.0
output_price = 1.0
"#
        );
        let _ = dir;
        Config::from_toml(&text).expect("config parses")
    }

    fn retriever(dir: &tempfile::TempDir, key_file: Option<&Path>) -> Retriever {
        Retriever::new(dir.path(), &config_for(dir.path(), key_file)).unwrap()
    }

    #[test]
    fn a_file_inside_the_checkout_resolves() {
        let dir = checkout();
        let found = retriever(&dir, None).resolve(&Request::File("src/auth.rs".to_string()));
        let Resolution::Found { content, .. } = found else {
            panic!("expected content, got {found:?}");
        };
        assert!(content.contains("verify_token"));
    }

    #[test]
    fn traversal_out_of_the_checkout_is_refused() {
        let dir = checkout();
        let r = retriever(&dir, None);
        for path in ["../outside.txt", "../../etc/passwd", "src/../../escape"] {
            let outcome = r.resolve(&Request::File(path.to_string()));
            assert!(
                !matches!(outcome, Resolution::Found { .. }),
                "{path} must not resolve: {outcome:?}"
            );
        }
    }

    #[test]
    fn an_absolute_path_is_refused() {
        let dir = checkout();
        let outcome = retriever(&dir, None).resolve(&Request::File("/etc/hostname".to_string()));
        assert!(matches!(outcome, Resolution::Refused { .. }), "{outcome:?}");
    }

    #[test]
    fn a_symlink_leaving_the_checkout_is_refused() {
        let dir = checkout();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "sensitive\n").unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret.txt"),
            dir.path().join("link.txt"),
        )
        .unwrap();
        let outcome = retriever(&dir, None).resolve(&Request::File("link.txt".to_string()));
        assert!(
            matches!(outcome, Resolution::Refused { .. }),
            "a link out of the checkout must be refused: {outcome:?}"
        );
    }

    #[test]
    fn the_version_control_directory_is_refused() {
        let dir = checkout();
        let outcome = retriever(&dir, None).resolve(&Request::File(".git/config".to_string()));
        assert!(matches!(outcome, Resolution::Refused { .. }), "{outcome:?}");
    }

    #[test]
    fn the_configuration_file_is_refused() {
        let dir = checkout();
        let outcome = retriever(&dir, None).resolve(&Request::File(".demur.toml".to_string()));
        assert!(matches!(outcome, Resolution::Refused { .. }), "{outcome:?}");
    }

    #[test]
    fn an_ignored_path_is_refused() {
        let dir = checkout();
        let outcome = retriever(&dir, None).resolve(&Request::File("Cargo.lock".to_string()));
        assert!(
            matches!(outcome, Resolution::Refused { .. }),
            "content excluded from review is excluded from retrieval: {outcome:?}"
        );
    }

    #[test]
    fn the_key_file_is_refused_inside_the_checkout() {
        // Nothing stops a repository from putting its key file in the tree.
        let dir = checkout();
        let key = dir.path().join("secrets.key");
        std::fs::write(&key, "sk-live-do-not-read\n").unwrap();
        let outcome =
            retriever(&dir, Some(&key)).resolve(&Request::File("secrets.key".to_string()));
        assert!(
            matches!(outcome, Resolution::Refused { .. }),
            "the provider key must never be retrievable: {outcome:?}"
        );
    }

    #[test]
    fn a_symbol_resolves_to_its_definition() {
        let dir = checkout();
        let found = retriever(&dir, None).resolve(&Request::Symbol("verify_token".to_string()));
        let Resolution::Found {
            content, origin, ..
        } = found
        else {
            panic!("expected a definition, got {found:?}");
        };
        assert_eq!(origin, "src/auth.rs");
        assert!(content.contains("pub fn verify_token"));
    }

    #[test]
    fn an_unknown_symbol_is_reported_not_found() {
        let dir = checkout();
        let outcome = retriever(&dir, None).resolve(&Request::Symbol("nonexistent".to_string()));
        assert!(
            matches!(outcome, Resolution::NotFound { .. }),
            "{outcome:?}"
        );
    }

    #[test]
    fn a_name_shaped_like_a_command_simply_does_not_resolve() {
        // Nothing detects malice. A command is a name that names no file.
        let dir = checkout();
        let r = retriever(&dir, None);
        for raw in [
            "file:$(rm -rf /)",
            "symbol:; cat /etc/passwd",
            "file:https://example.com/x",
            "sh -c 'echo hi'",
        ] {
            let outcome = r.resolve(&Request::parse(raw));
            assert!(
                !matches!(outcome, Resolution::Found { .. }),
                "{raw} must not resolve: {outcome:?}"
            );
        }
    }

    #[test]
    fn requests_parse_into_names() {
        assert_eq!(
            Request::parse("file:src/a.rs"),
            Request::File("src/a.rs".to_string())
        );
        assert_eq!(
            Request::parse("symbol:foo"),
            Request::Symbol("foo".to_string())
        );
        // An unprefixed request is a symbol, never a path.
        assert_eq!(Request::parse("foo"), Request::Symbol("foo".to_string()));
    }

    #[test]
    fn a_partial_word_is_not_a_definition() {
        assert!(is_definition("fn verify_token()", "verify_token"));
        assert!(!is_definition("fn verify_token_inner()", "verify_token"));
        assert!(!is_definition("    verify_token();", "verify_token"));
    }
}
