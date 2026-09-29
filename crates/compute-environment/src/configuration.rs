//! Environment configuration: runtime inputs, distinct from workspace state.
//!
//! ```text
//! Environment
//!   ├── declared state         what should exist          (contents)
//!   ├── configuration inputs   what its processes are given   (this module)
//!   └── workspace              portable filesystem state
//! ```
//!
//! **Configuration is not a file.** A `.env` file is one *source* of
//! configuration: it is parsed, validated, and imported as named variables
//! (`EnvironmentRecord.config`, the existing semantic record), and the file need
//! not remain anywhere. Values reach a process through its environment when the
//! ordinary reconciler starts it; they are never written into the workspace, so a
//! checkpoint of the workspace does not hold them, and fork and restore never
//! carry them.
//!
//! **Values are never shown unless known to be public.** Each variable is either
//! sensitive or public. Everything is sensitive unless its name says otherwise
//! ([`is_public`], a short explicit rule, not detection: Compute cannot tell what
//! a value is) or the caller says so. A sensitive value is never returned by any
//! API, CLI, event, or receipt: only that it is configured, its source, and the
//! generation. Files a workload writes into its own workspace remain workspace
//! data; Compute cannot recognise secrets in them and does not claim to.
//!
//! # The `.env` syntax accepted
//!
//! Deliberately small, and identical on every run:
//!
//! * lines end in `\n` or `\r\n`; a leading byte-order mark is ignored;
//! * blank lines and lines whose first non-blank character is `#` are ignored;
//! * `NAME=VALUE`, optionally prefixed `export `; `NAME` is
//!   `[A-Za-z_][A-Za-z0-9_]*`, up to 256 characters; whitespace around `=` is
//!   skipped;
//! * an unquoted `VALUE` ends at the end of the line or at whitespace followed
//!   by `#` (a comment), and is trimmed; it may be empty and may contain `=`;
//! * `'VALUE'` is literal, without escapes;
//! * `"VALUE"` understands `\\`, `\"`, `\n`, `\r`, and `\t`, and no other escape;
//! * after a closing quote only whitespace or a comment may follow;
//! * a quoted value ends on its own line: there are no multi-line values;
//! * there is no `${...}` expansion, `$` is literal;
//! * a name given twice in one file is an error; across files, a later file
//!   overrides an earlier one, in the order the files were given;
//! * a malformed line is an error, and an error names the file and line number
//!   and the reason, **never any part of the line**.
//!
//! Values are UTF-8 and may not contain NUL.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::EnvironmentError;

/// The most variables an import accepts, and the longest name.
const MAX_VARIABLES: usize = 1024;
const MAX_NAME: usize = 256;

/// One `.env` file's content, named by the label it is reported under: the
/// file's base name, never a path.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    pub name: String,
    pub content: String,
}

impl std::fmt::Debug for ConfigFile {
    // Content is configuration: it never reaches a log through `Debug`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfigFile")
            .field("name", &self.name)
            .field("content", &"<redacted>")
            .finish()
    }
}

/// Parse one `.env` file into `(name, value)` pairs in file order. See the
/// module documentation for the syntax. An error never contains a value.
pub fn parse_dotenv(file: &str, text: &str) -> Result<Vec<(String, String)>, EnvironmentError> {
    let bad =
        |line: usize, why: &str| EnvironmentError::Invalid(format!("{file}, line {line}: {why}"));
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut seen = BTreeSet::new();
    let mut pairs = vec![];
    for (index, raw) in text.split('\n').enumerate() {
        let number = index + 1;
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        let line = line.trim_start();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = match line.strip_prefix("export") {
            Some(rest) if rest.starts_with([' ', '\t']) => rest.trim_start(),
            _ => line,
        };
        let Some((name, rest)) = line.split_once('=') else {
            return Err(bad(number, "expected NAME=VALUE"));
        };
        let name = name.trim_end_matches([' ', '\t']);
        if !valid_name(name) {
            return Err(bad(number, "the name is not [A-Za-z_][A-Za-z0-9_]*"));
        }
        let rest = rest.trim_start_matches([' ', '\t']);
        let value = match rest.chars().next() {
            Some('"') => quoted(rest, '"').map_err(|why| bad(number, why))?,
            Some('\'') => quoted(rest, '\'').map_err(|why| bad(number, why))?,
            _ => unquoted(rest),
        };
        if value.contains('\0') {
            return Err(bad(number, "the value contains a NUL"));
        }
        if !seen.insert(name.to_owned()) {
            return Err(EnvironmentError::Invalid(format!(
                "{file}, line {number}: {name} is defined twice in this file"
            )));
        }
        if pairs.len() >= MAX_VARIABLES {
            return Err(bad(number, "too many variables"));
        }
        pairs.push((name.to_owned(), value));
    }
    Ok(pairs)
}

fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    name.len() <= MAX_NAME
        && chars
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Up to the end of the line, or whitespace then `#`; trimmed.
fn unquoted(rest: &str) -> String {
    let mut end = rest.len();
    if rest.starts_with('#') {
        end = 0;
    } else {
        let bytes = rest.as_bytes();
        for (index, byte) in bytes.iter().enumerate() {
            if *byte == b'#' && index > 0 && matches!(bytes[index - 1], b' ' | b'\t') {
                end = index;
                break;
            }
        }
    }
    rest[..end].trim().to_owned()
}

/// A quoted value starting at `rest`'s opening quote; only a comment may follow
/// the closing quote.
fn quoted(rest: &str, quote: char) -> Result<String, &'static str> {
    let mut value = String::new();
    let mut chars = rest[1..].char_indices();
    let mut end = None;
    while let Some((index, c)) = chars.next() {
        match c {
            c if c == quote => {
                end = Some(index + 1 + c.len_utf8());
                break;
            }
            '\\' if quote == '"' => match chars.next() {
                Some((_, '\\')) => value.push('\\'),
                Some((_, '"')) => value.push('"'),
                Some((_, 'n')) => value.push('\n'),
                Some((_, 'r')) => value.push('\r'),
                Some((_, 't')) => value.push('\t'),
                Some(_) => return Err("an escape other than \\\\ \\\" \\n \\r \\t"),
                None => return Err("the quote is not closed"),
            },
            c => value.push(c),
        }
    }
    let end = end.ok_or("the quote is not closed")?;
    let after = rest[end..].trim_start_matches([' ', '\t']);
    if !after.is_empty() && !after.starts_with('#') {
        return Err("text after the closing quote");
    }
    Ok(value)
}

/// The variables of several files, later files overriding earlier ones, and
/// the names that were overridden.
pub fn merge_dotenv(
    files: &[ConfigFile],
) -> Result<(BTreeMap<String, (String, String)>, Vec<String>), EnvironmentError> {
    let mut merged = BTreeMap::<String, (String, String)>::new();
    let mut overridden = BTreeSet::new();
    for file in files {
        for (name, value) in parse_dotenv(&file.name, &file.content)? {
            if merged
                .insert(name.clone(), (value, file.name.clone()))
                .is_some()
            {
                overridden.insert(name);
            }
        }
    }
    Ok((merged, overridden.into_iter().collect()))
}

/// Whether a variable's *name* marks it as one whose value is safe to show.
/// The rule is short and fails closed: a name that looks like it holds a secret
/// is never public, and a name that is not on the list is not either. It is a
/// default for display, not a judgement about the value; the caller overrides it
/// per variable (`public` / `secret`).
pub fn is_public(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    const SECRETIVE: [&str; 11] = [
        "SECRET",
        "TOKEN",
        "KEY",
        "PASSWORD",
        "PASSWD",
        "CREDENTIAL",
        "PRIVATE",
        "AUTH",
        "SIGN",
        "CERT",
        "SALT",
    ];
    if SECRETIVE.iter().any(|word| upper.contains(word)) {
        return false;
    }
    const EXACT: [&str; 14] = [
        "PORT",
        "HOST",
        "HOSTNAME",
        "NODE_ENV",
        "APP_ENV",
        "ENV",
        "ENVIRONMENT",
        "LOG_LEVEL",
        "DEBUG",
        "TZ",
        "LANG",
        "LC_ALL",
        "REGION",
        "TIMEOUT",
    ];
    const SUFFIXES: [&str; 8] = [
        "_MODE", "_ENV", "_PORT", "_HOST", "_LEVEL", "_REGION", "_TIMEOUT", "_ENABLED",
    ];
    EXACT.contains(&upper.as_str()) || SUFFIXES.iter().any(|suffix| upper.ends_with(suffix))
}

/// Names Compute owns: an import skips them and says so, rather than failing
/// the rest.
pub fn reserved(name: &str) -> Option<&'static str> {
    if name == "PORT" {
        Some("reserved: Compute gives a process its declared port as PORT")
    } else if name.starts_with("COMPUTE_") {
        Some("reserved: names starting COMPUTE_ belong to Compute")
    } else {
        None
    }
}

/// The environment's configuration, without values a caller may not see.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ConfigurationView {
    pub environment: String,
    /// The contents generation the configuration last changed at.
    pub generation: u64,
    pub variables: Vec<ConfigurationInput>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigurationInput {
    pub name: String,
    pub configured: bool,
    pub sensitive: bool,
    /// `.env`, `.env.local`, `cli`, `api`, or `declared`.
    pub source: String,
    /// Only for a public variable. A sensitive value is never returned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

/// Import `.env` files into an environment's configuration: parsed and
/// validated whole, then applied as one new generation, or not at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigImportRequest {
    pub files: Vec<ConfigFile>,
    /// Variables to treat as public (their values may be shown).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub public: Vec<String>,
    /// Variables to treat as sensitive, whatever their names say.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secret: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigImportReport {
    pub environment: String,
    /// The configuration generation after the import (unchanged when nothing
    /// changed).
    pub generation: u64,
    /// True when every variable already held the imported value and treatment.
    pub unchanged: bool,
    pub imported: Vec<ImportedVariable>,
    /// Variables not imported, with why: reserved names.
    pub skipped: Vec<SkippedVariable>,
    /// Names defined in more than one file (the later file won).
    pub overridden: Vec<String>,
    pub files: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportedVariable {
    pub name: String,
    pub sensitive: bool,
    pub source: String,
    /// New, or its value or treatment differs from what was configured.
    pub changed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedVariable {
    pub name: String,
    pub reason: String,
}

/// Set and remove variables one by one (the CLI's `--set` and `--unset`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigChange {
    #[serde(default)]
    pub set: BTreeMap<String, String>,
    #[serde(default)]
    pub unset: Vec<String>,
    #[serde(default)]
    pub public: Vec<String>,
    #[serde(default)]
    pub secret: Vec<String>,
    /// Where the change came from: `cli` or `api`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// What `.env`-style files in an environment's workspace ask for and provide:
/// names only, never a value, which never leave the computer.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ConfigurationDiscovery {
    pub environment: String,
    pub files: Vec<DiscoveredFile>,
    pub variables: Vec<DiscoveredVariable>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredFile {
    /// Relative to the workspace.
    pub path: String,
    /// `values` (`.env`, `.env.local`, `.env.development`, `.env.production`)
    /// or `requirements` (`.env.example`): the latter names what is expected
    /// without supplying it.
    pub kind: String,
    pub variables: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredVariable {
    pub name: String,
    /// `configured` (the environment has it), `available` (a workspace `.env`
    /// file supplies it; not imported), or `missing`.
    pub status: String,
    /// How it would be treated if imported: not public unless its name says so.
    pub sensitive: bool,
    /// The files that mention it.
    pub files: Vec<String>,
}

/// A variable a checkpoint's environment had configured: a name and how it was
/// treated, never a value. What a restore into a new environment needs supplied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigurationRequirement {
    pub name: String,
    pub sensitive: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Vec<(String, String)>, String> {
        parse_dotenv(".env", text).map_err(|error| error.to_string())
    }

    fn pairs(text: &str) -> Vec<(String, String)> {
        parse(text).unwrap()
    }

    fn p(name: &str, value: &str) -> (String, String) {
        (name.into(), value.into())
    }

    #[test]
    fn plain_quoted_and_empty_values() {
        assert_eq!(
            pairs("A=1\nB = spaced out \nC=\nD=\"quoted value\"\nE='single #kept'\nF=a=b=c\n"),
            vec![
                p("A", "1"),
                p("B", "spaced out"),
                p("C", ""),
                p("D", "quoted value"),
                p("E", "single #kept"),
                p("F", "a=b=c")
            ]
        );
    }

    #[test]
    fn comments_blank_lines_export_and_bom() {
        let text = "\u{feff}# leading\n\n  # indented\nexport A=1 # trailing\nB=2#not a comment\nC=# whole\nD=\"x\" # after quote\n";
        assert_eq!(
            pairs(text),
            vec![
                p("A", "1"),
                p("B", "2#not a comment"),
                p("C", ""),
                p("D", "x")
            ]
        );
    }

    #[test]
    fn newlines_crlf_and_escapes() {
        assert_eq!(
            pairs("A=1\r\nB=\"line\\nbreak\\ttab \\\"q\\\" \\\\\"\r\nC=last"),
            vec![
                p("A", "1"),
                p("B", "line\nbreak\ttab \"q\" \\"),
                p("C", "last")
            ]
        );
        assert_eq!(
            pairs("A=$HOME/${X}"),
            vec![p("A", "$HOME/${X}")],
            "no expansion"
        );
    }

    #[test]
    fn unicode_is_kept() {
        assert_eq!(
            pairs("GREETING=\"héllo 世界 🌍\"\nLABEL=naïve"),
            vec![p("GREETING", "héllo 世界 🌍"), p("LABEL", "naïve")]
        );
    }

    #[test]
    fn malformed_input_is_rejected_without_echoing_any_of_it() {
        let secret = "sk_live_TOPSECRET";
        for (text, why) in [
            (format!("{secret}\n"), "expected NAME=VALUE"),
            (format!("1BAD={secret}\n"), "the name is not"),
            (format!("BAD NAME={secret}\n"), "the name is not"),
            (format!("=\"{secret}\"\n"), "the name is not"),
            (format!("A=\"{secret}\n"), "the quote is not closed"),
            (format!("A='{secret}\n"), "the quote is not closed"),
            (
                format!("A=\"{secret}\" trailing\n"),
                "text after the closing quote",
            ),
            (format!("A=\"{secret}\\q\"\n"), "an escape other than"),
            (format!("A={secret}\0\n"), "NUL"),
        ] {
            let error = parse(&text).unwrap_err();
            assert!(error.contains(why), "{why}: {error}");
            assert!(error.contains(".env, line 1"), "{error}");
            assert!(
                !error.contains(secret),
                "the error echoes the line: {error}"
            );
        }
        let error = parse("A=1\nB=2\nnot a line\n").unwrap_err();
        assert!(error.contains("line 3"), "{error}");
    }

    #[test]
    fn a_name_twice_in_a_file_is_an_error_and_across_files_the_later_wins() {
        let error = parse("A=1\nB=2\nA=3\n").unwrap_err();
        assert!(
            error.contains("line 3") && error.contains("A is defined twice"),
            "{error}"
        );
        let files = [
            ConfigFile {
                name: ".env".into(),
                content: "A=1\nB=base\n".into(),
            },
            ConfigFile {
                name: ".env.local".into(),
                content: "B=local\nC=3\n".into(),
            },
        ];
        let (merged, overridden) = merge_dotenv(&files).unwrap();
        assert_eq!(merged["A"], ("1".into(), ".env".into()));
        assert_eq!(merged["B"], ("local".into(), ".env.local".into()));
        assert_eq!(overridden, vec!["B".to_string()]);
        // Order given is the order applied, and it is deterministic.
        let reversed = [files[1].clone(), files[0].clone()];
        assert_eq!(merge_dotenv(&reversed).unwrap().0["B"].0, "base");
        assert_eq!(merge_dotenv(&files).unwrap(), merge_dotenv(&files).unwrap());
    }

    #[test]
    fn everything_is_sensitive_unless_its_name_says_it_is_public() {
        for name in [
            "DATABASE_URL",
            "STRIPE_SECRET_KEY",
            "API_TOKEN",
            "REDIS_URL",
            "ANYTHING",
            "SECRET_MODE",
            "AUTH_ENV",
        ] {
            assert!(!is_public(name), "{name}");
        }
        for name in [
            "PORT",
            "NODE_ENV",
            "APP_MODE",
            "LOG_LEVEL",
            "DB_HOST",
            "feature_enabled",
        ] {
            assert!(is_public(name), "{name}");
        }
        assert!(
            reserved("PORT").is_some()
                && reserved("COMPUTE_X").is_some()
                && reserved("A").is_none()
        );
    }

    #[test]
    fn a_configuration_file_never_reaches_a_log() {
        let file = ConfigFile {
            name: ".env".into(),
            content: "TOKEN=hunter2".into(),
        };
        assert!(!format!("{file:?}").contains("hunter2"));
    }
}
