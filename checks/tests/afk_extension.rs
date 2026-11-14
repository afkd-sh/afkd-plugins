//! The extension contract afkd holds itself to, held here too: **every file of the afk
//! language ends in `.afk`, and no text names one by the extension it had before**.
//!
//! afkd reads the language only from `.afk` files since it took a name of its own, so a stray
//! old spelling in a README, a manifest, a release step or a drift test is a settings file
//! the daemon never reads or a config a test writes where afkd does not look. So the gate
//! reads every file `git ls-files -z` lists:
//!
//! - no path ends in the old extension ([`OLD`]);
//! - no text holds it where it ends a file name or names the extension ([`hits`]): after a
//!   stem (`daemon`, `{NAME}`), after a `*`, or bare (a `` ` `` before it), and not followed
//!   by a name character or a `-`, or by a `.` and a letter. A `/` after it ends a file name
//!   too when a stem is before it.
//!
//! What is not a hit, and why:
//!
//! - **afkd's directories and marker files.** The project's runtime dir (`.afkd/runs/…`, in
//!   the plugins' park and stop notes) and the helpers' temp files (`.afkd-linear-…`) are
//!   spelled with the daemon's name. A bare spelling before a `/`, or one right after a `/` or
//!   a `~`, is a directory; one followed by `-` is a marker or a temp file.
//! - **A domain or a key path** (`app.afkd.sh`): a `.` and a letter follow.
//! - **A field read off a value**, listed with its file in [`NOT_FILES`]; a pair that no
//!   longer matches fails the gate.
//!
//! This file spells the old extension to forbid it, so it is outside its own walk.

use std::path::Path;
use std::process::Command;

/// The repository root, one up from this crate.
const REPO_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/..");

/// The extension the language's files had before they took the language's own name.
const OLD: &str = ".afkd";

/// This file, which names [`OLD`] to forbid it.
const THIS_FILE: &str = "checks/tests/afk_extension.rs";

/// Each spelling of [`OLD`] that names no file, as `(path, fragment of its line)`: a frame's
/// `afkd` field, the daemon's own version in web_top's layout suite.
const NOT_FILES: &[(&str, &str)] = &[("@afkd/web_top/layout.test.mjs", r#"f.afkd === "welcome""#)];

/// Every path `git ls-files -z` lists at the repository root.
fn tracked() -> Vec<String> {
    let root = Path::new(REPO_ROOT);
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "-z"])
        .output()
        .unwrap_or_else(|e| panic!("cannot run `git ls-files` in {}: {e}", root.display()));
    assert!(
        output.status.success(),
        "`git ls-files` failed in {}: the gate needs a git checkout: {}",
        root.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    let listing = String::from_utf8(output.stdout).expect("git lists UTF-8 paths");
    let files: Vec<String> = listing
        .split('\0')
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
        .collect();
    assert!(
        files
            .iter()
            .any(|path| path == "@afkd/telegram/afkd-plugin.toml"),
        "`git ls-files` listed {} paths but not @afkd/telegram/afkd-plugin.toml: the walk is \
         not reaching the tree",
        files.len()
    );
    files
}

/// Whether `c` continues a name, so an [`OLD`] before it is not where a file name ends.
fn continues_a_name(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The byte offsets in `text` where [`OLD`] ends a file name or names the extension.
fn hits(text: &str) -> Vec<usize> {
    text.match_indices(OLD)
        .map(|(at, _)| at)
        .filter(|&at| {
            let before = &text[..at];
            let mut after = text[at + OLD.len()..].chars();
            let ends_here = match after.next() {
                // A stem before it makes it the extension on a directory; a bare one is afkd's
                // runtime dir (`.afkd/runs`).
                Some('/') => before.chars().next_back().is_some_and(continues_a_name),
                Some(c) if continues_a_name(c) || c == '-' => false,
                Some('.') => !after.next().is_some_and(|c| c.is_ascii_alphabetic()),
                _ => true,
            };
            let under_a_directory = before.ends_with('/') || before.ends_with('~');
            ends_here && !under_a_directory
        })
        .collect()
}

/// The line of `text` that holds byte offset `at`, and its 1-based number.
fn line_at(text: &str, at: usize) -> (usize, &str) {
    let start = text[..at].rfind('\n').map_or(0, |n| n + 1);
    let end = text[at..].find('\n').map_or(text.len(), |n| at + n);
    (text[..at].matches('\n').count() + 1, &text[start..end])
}

/// Whether the hit at `at` in `path`'s `text` is one of [`NOT_FILES`]: its fragment covers
/// the hit.
fn not_a_file(path: &str, text: &str, at: usize) -> Option<usize> {
    NOT_FILES.iter().position(|&(listed, fragment)| {
        listed == path
            && text
                .match_indices(fragment)
                .any(|(start, _)| start <= at && at < start + fragment.len())
    })
}

#[test]
fn no_tracked_path_ends_in_the_old_extension() {
    let old: Vec<String> = tracked()
        .into_iter()
        .filter(|path| path.ends_with(OLD))
        .collect();
    assert!(
        old.is_empty(),
        "{} tracked files still end in {OLD}; a file of the afk language ends in .afk:\n{}",
        old.len(),
        old.join("\n")
    );
}

#[test]
fn no_tracked_text_names_a_file_by_the_old_extension() {
    let root = Path::new(REPO_ROOT);
    let mut found = Vec::new();
    let mut used = vec![false; NOT_FILES.len()];
    for path in tracked().iter().filter(|path| *path != THIS_FILE) {
        let text = match std::fs::read_to_string(root.join(path)) {
            Ok(text) => text,
            // Deleted in the working tree but not yet from the index: not in the tree.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            // A binary (an image, a font): no text to name a file in.
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => continue,
            Err(e) => panic!("cannot read {path}: {e}"),
        };
        for at in hits(&text) {
            if let Some(listed) = not_a_file(path, &text, at) {
                used[listed] = true;
            } else {
                let (number, line) = line_at(&text, at);
                found.push(format!("{path}:{number}: {}", line.trim()));
            }
        }
    }
    assert!(
        found.is_empty(),
        "{} lines name a file by {OLD}; the afk language's files end in .afk:\n{}",
        found.len(),
        found.join("\n")
    );
    let stale: Vec<String> = NOT_FILES
        .iter()
        .zip(&used)
        .filter(|(_, used)| !**used)
        .map(|((path, fragment), _)| format!("{path}: {fragment}"))
        .collect();
    assert!(
        stale.is_empty(),
        "these spellings match no line any more; drop them from NOT_FILES:\n{}",
        stale.join("\n")
    );
}

#[test]
fn a_file_name_a_glob_and_the_bare_extension_are_hits() {
    for text in [
        "// ~/.config/afkd/plugins/@afkd/telegram.afkd",
        "config_dir(home).join(\"daemon.afkd\")",
        r#"format!("{NAME}.afkd")"#,
        r#"match "**/*.afkd""#,
        "every `.afkd` file",
        "ends in .afkd.",
        "`selfdev/selfdev.afkd` declares `develop`",
        "plugins/@local/drafts.afkd/x",
    ] {
        assert_eq!(hits(text).len(), 1, "{text}");
    }
}

#[test]
fn a_directory_a_temp_file_and_a_domain_are_not_hits() {
    for text in [
        "log: .afkd/runs/260925-095800-unit-7-1/run.log",
        "~/.afkd/runs",
        "<work_dir>/.afkd",
        r#"f".afkd-linear-{os.getpid()}""#,
        r#"".{}.afkd-trello-{}""#,
        "https://app.afkd.sh/mcp",
    ] {
        assert!(hits(text).is_empty(), "{text}: {:?}", hits(text));
    }
}

#[test]
fn a_listed_spelling_is_excused_only_in_its_own_file() {
    let (path, fragment) = NOT_FILES[0];
    let text = format!("const v = {fragment};");
    let at = hits(&text)[0];
    assert_eq!(not_a_file(path, &text, at), Some(0), "in {path}");
    assert_eq!(
        not_a_file("@afkd/web_top/fold.test.mjs", &text, at),
        None,
        "elsewhere"
    );
}
