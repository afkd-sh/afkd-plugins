//! The secret contract afkd's own docs hold themselves to, held here too: **no README reads a
//! secret from the environment with a literal fallback**.
//!
//! afkd's docs send a reader to each plugin's README as that plugin's reference, and a reader
//! copies its configs. A config that reads `env.get("GITEA_TOKEN") ?? ""` loads when the
//! variable is unset, and then talks to the service with an empty credential, or with one
//! anyone can read in the README. So each README reads a secret the way afkd's docs do, as
//! `env.get("NAME") ?? fail("NAME is unset")`, which fails the load naming the variable. The
//! gate reads every line of every `README.md` git tracks, prose and comments included, since
//! a reader copies a read from either, with afkd's own rule:
//!
//! - a **secret** is a variable one of whose `_`-separated words is in [`SECRET_NAME_WORDS`];
//! - a **literal fallback** is an `env.get("NAME")` followed, past any spaces, by `??` and a
//!   string literal ([`literal_secret_fallback`]).
//!
//! An OAuth client id (`LINEAR_CLIENT_ID`) names no secret word, so its fallback stands.

mod common;

use std::path::Path;

use common::{tracked, REPO_ROOT};

/// The words that make an environment variable a secret: a name one of whose `_`-separated
/// words is one of these, such as `DEPLOY_HOOK_SECRET` or `LAB_PW`, holds a credential.
const SECRET_NAME_WORDS: &[&str] = &["SECRET", "TOKEN", "PASSWORD", "PASSWD", "KEY", "PW"];

/// The secret `line` reads from the environment with a literal fallback, if it reads one:
/// the `NAME` of an `env.get("NAME") ?? "…"` whose name is a secret's by
/// [`SECRET_NAME_WORDS`].
fn literal_secret_fallback(line: &str) -> Option<&str> {
    const READ: &str = "env.get(\"";
    line.match_indices(READ).find_map(|(at, _)| {
        let (name, rest) = line[at + READ.len()..].split_once("\")")?;
        let literal = rest
            .trim_start()
            .strip_prefix("??")?
            .trim_start()
            .starts_with('"');
        let secret = name
            .split('_')
            .any(|word| SECRET_NAME_WORDS.contains(&word));
        (literal && secret).then_some(name)
    })
}

/// Every `README.md` git tracks, by its path under the repository root: the root's, each
/// plugin's and any under a plugin's own tree.
fn readmes() -> Vec<String> {
    tracked()
        .into_iter()
        .filter(|path| path == "README.md" || path.ends_with("/README.md"))
        .collect()
}

#[test]
fn a_secret_fallback_is_found_by_its_name_and_its_literal() {
    let hits = [
        // The plugins' READMEs read these.
        (
            r#"GITEA_TOKEN :: env.get("GITEA_TOKEN") ?? """#,
            "GITEA_TOKEN",
        ),
        (
            r#"GITHUB_TOKEN :: env.get("GITHUB_TOKEN") ?? """#,
            "GITHUB_TOKEN",
        ),
        (
            r#"GITLAB_TOKEN :: env.get("GITLAB_TOKEN") ?? """#,
            "GITLAB_TOKEN",
        ),
        (
            r#"LINEAR_API_KEY :: env.get("LINEAR_API_KEY") ?? """#,
            "LINEAR_API_KEY",
        ),
        (
            r#"LINEAR_CLIENT_SECRET :: env.get("LINEAR_CLIENT_SECRET") ?? """#,
            "LINEAR_CLIENT_SECRET",
        ),
        (
            r#"BOT_TOKEN :: env.get("TELEGRAM_BOT_TOKEN") ?? """#,
            "TELEGRAM_BOT_TOKEN",
        ),
        (
            r#"TRELLO_API_KEY :: env.get("TRELLO_API_KEY") ?? """#,
            "TRELLO_API_KEY",
        ),
        (
            r#"TRELLO_TOKEN   :: env.get("TRELLO_TOKEN") ?? """#,
            "TRELLO_TOKEN",
        ),
        // A setting's value, a non-empty literal.
        (
            r#"  secret env.get("DEPLOY_HOOK_SECRET") ?? "3f7c""#,
            "DEPLOY_HOOK_SECRET",
        ),
        // A literal of wide and accented text, in a call's argument.
        (
            r#"lab := afkd.connect("lab", password=env.get("LAB_PW") ?? "s3cret — 秘密")"#,
            "LAB_PW",
        ),
        // Prose, backticked, with no spaces round `??`.
        (
            r#"`HOME ?? ""`, then `env.get("GH_TOKEN")??"x"`"#,
            "GH_TOKEN",
        ),
    ];
    for (line, name) in hits {
        assert_eq!(literal_secret_fallback(line), Some(name), "{line}");
    }
    let misses = [
        // An OAuth client id is no secret.
        r#"LINEAR_CLIENT_ID     :: env.get("LINEAR_CLIENT_ID") ?? """#,
        r#"HOME :: env.get("HOME") ?? "/root""#,
        // A read that fails the load, in the one-line form and through a proc.
        r#"GITEA_TOKEN :: env.get("GITEA_TOKEN") ?? fail("GITEA_TOKEN is unset")"#,
        r#"TRELLO_TOKEN   :: required_env("TRELLO_TOKEN")"#,
        r##"  env.get(name) ?? fail("#{name} is unset")"##,
        r#"password env.get("AFKD_PASSWORD") ?? nil"#,
        // No fallback at all.
        r#"`env.get("GITEA_BOT")`. `gitea.assign_me(issue)` assigns the token's own login"#,
        // A word that only starts like a secret's, and one that names none.
        r#"layout := env.get("KEYBOARD") ?? "x""#,
        r#"hook := env.get("HOOK_API") ?? "dev-hook""#,
    ];
    for line in misses {
        assert_eq!(literal_secret_fallback(line), None, "{line}");
    }
}

#[test]
fn no_readme_gives_a_secret_a_literal_fallback() {
    let root = Path::new(REPO_ROOT);
    let readmes = readmes();
    let mut must_read = vec!["README.md".to_owned()];
    must_read.extend(tracked().iter().filter_map(|path| {
        let plugin = path.strip_suffix("/afkd-plugin.toml")?;
        Some(format!("{plugin}/README.md"))
    }));
    let unread: Vec<&String> = must_read
        .iter()
        .filter(|path| !readmes.contains(path))
        .collect();
    assert!(
        unread.is_empty(),
        "the sweep reads the root's README and every plugin's, but not: {unread:?}"
    );

    let mut found = Vec::new();
    for path in &readmes {
        let text = match std::fs::read_to_string(root.join(path)) {
            Ok(text) => text,
            // Deleted in the working tree but not yet from the index: not in the tree.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => panic!("cannot read {path}: {e}"),
        };
        for (at, line) in text.lines().enumerate() {
            if let Some(name) = literal_secret_fallback(line) {
                found.push(format!("{path}:{}: {name}: {}", at + 1, line.trim()));
            }
        }
    }
    assert!(
        found.is_empty(),
        "{} secret read(s) fall back to a literal; read each as \
         `env.get(\"NAME\") ?? fail(\"NAME is unset\")`, which fails the load naming it:\n  {}",
        found.len(),
        found.join("\n  ")
    );
}
