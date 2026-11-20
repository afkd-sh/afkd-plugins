//! The citation contract afkd's docs hold themselves to, held here too: **no text sends a
//! reader to the design record, `lang-v2`, for a rule**.
//!
//! The record is history: afkd's site does not publish it (`https://afkd.sh/docs/lang-v2/`
//! answers 404), and the rules live in the language reference, `https://afkd.sh/docs/reference/`,
//! which a README links by a section's anchor and a comment cites as `reference §N.M`. So the
//! gate reads every file `git ls-files -z` lists, and refuses ([`hits`]):
//!
//! - a link to `afkd.sh/docs/lang-v2`;
//! - a `lang-v2` with a `§` on its line (`lang-v2 §20.3`);
//! - a `lang-v2` that ends its line, or is followed on it by nothing but spaces and `(`, when
//!   the next line starts with a `§` once its comment leader is removed: a citation a comment
//!   wrapped between the name and the section.
//!
//! What is not a hit: a name that cites no section, such as the record's file (`docs/lang-v2.md`)
//! a test reads. A line that names the record as where a copied text came from, not as the
//! source of a rule, is listed with its file in [`RECORD_SOURCES`]; an entry that no longer
//! matches a line fails the gate.
//!
//! This file spells the record to forbid it, so it is outside its own walk.

mod common;

use std::path::Path;

use common::{tracked, REPO_ROOT};

/// The design record's name.
const RECORD: &str = "lang-v2";

/// What precedes [`RECORD`] in a link to the page afkd's site no longer publishes.
const PAGE_PREFIX: &str = "afkd.sh/docs/";

/// The leaders a comment line starts with, longest first, so `///` is not read as `//`.
const COMMENT_LEADERS: &[&str] = &["///", "//!", "//", "*", "#"];

/// This file, which names [`RECORD`] to forbid it.
const THIS_FILE: &str = "checks/tests/record_citations.rs";

/// Each line that names the record as where a copied text came from, as `(path, fragment of
/// its line)`: Trello's drift test copied its §24 fixture from the record.
const RECORD_SOURCES: &[(&str, &str)] = &[(
    "drift/@afkd/trello/tests/drift.rs",
    "copied from `docs/lang-v2.md` at afkd master",
)];

/// Whether `line`, once its comment leader is removed, starts with a `§`.
fn starts_with_a_section(line: &str) -> bool {
    let line = line.trim_start();
    let line = COMMENT_LEADERS
        .iter()
        .find_map(|leader| line.strip_prefix(leader))
        .unwrap_or(line);
    line.trim_start().starts_with('§')
}

/// The byte offsets in `text` where it links the page afkd does not publish, or cites a
/// section of [`RECORD`] on the line or on the next one.
fn hits(text: &str) -> Vec<usize> {
    text.match_indices(RECORD)
        .filter_map(|(at, _)| {
            let before = &text[..at];
            if before.ends_with(PAGE_PREFIX) {
                return Some(at - PAGE_PREFIX.len());
            }
            let line_start = before.rfind('\n').map_or(0, |n| n + 1);
            let after = &text[at + RECORD.len()..];
            let (rest, next) = after.split_once('\n').unwrap_or((after, ""));
            let cited_here = text[line_start..at].contains('§') || rest.contains('§');
            let wrapped = rest.chars().all(|c| c == ' ' || c == '(')
                && starts_with_a_section(next.split('\n').next().unwrap_or(""));
            (cited_here || wrapped).then_some(at)
        })
        .collect()
}

/// The line of `text` that holds byte offset `at`, and its 1-based number.
fn line_at(text: &str, at: usize) -> (usize, &str) {
    let start = text[..at].rfind('\n').map_or(0, |n| n + 1);
    let end = text[at..].find('\n').map_or(text.len(), |n| at + n);
    (text[..at].matches('\n').count() + 1, &text[start..end])
}

/// The index in [`RECORD_SOURCES`] of the entry that excuses `line` of `path`, if one does.
fn record_source(path: &str, line: &str) -> Option<usize> {
    RECORD_SOURCES
        .iter()
        .position(|&(listed, fragment)| listed == path && line.contains(fragment))
}

/// The text of `path` under the repository root, or `None` for a file with none to read: one
/// deleted in the working tree but not yet from the index, or a binary (an image, a font).
fn read(path: &str) -> Option<String> {
    match std::fs::read_to_string(Path::new(REPO_ROOT).join(path)) {
        Ok(text) => Some(text),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::InvalidData
            ) =>
        {
            None
        }
        Err(e) => panic!("cannot read {path}: {e}"),
    }
}

#[test]
fn no_tracked_text_cites_the_design_record() {
    let mut found = Vec::new();
    for path in tracked().iter().filter(|path| *path != THIS_FILE) {
        let Some(text) = read(path) else { continue };
        for at in hits(&text) {
            let (number, line) = line_at(&text, at);
            if record_source(path, line).is_none() {
                found.push(format!("{path}:{number}: {}", line.trim()));
            }
        }
    }
    assert!(
        found.is_empty(),
        "{} lines cite the design record; link the language reference, \
         https://afkd.sh/docs/reference/, or cite it as `reference §N.M`:\n{}",
        found.len(),
        found.join("\n")
    );
}

#[test]
fn every_record_source_still_matches_a_line() {
    let stale: Vec<String> = RECORD_SOURCES
        .iter()
        .filter(|&&(path, fragment)| {
            !read(path).is_some_and(|text| text.lines().any(|line| line.contains(fragment)))
        })
        .map(|(path, fragment)| format!("{path}: {fragment}"))
        .collect();
    assert!(
        stale.is_empty(),
        "these sources match no line any more; drop them from RECORD_SOURCES:\n{}",
        stale.join("\n")
    );
}

#[test]
fn a_link_and_a_cited_section_are_hits() {
    for text in [
        "[the afk language](https://afkd.sh/docs/lang-v2/) for the configs",
        "Both kinds' slots are code ([lang-v2 §12.6](https://afkd.sh/docs/lang-v2/)), each",
        "group (lang-v2 §3.6, §20.3), so the v1 identities",
        "packages are flat, lang-v2 §20.3",
        "share one namespace (lang-v2\n/// §16.3), and afkd",
        "share one namespace (lang-v2\n * §x), and afkd",
        "share one namespace (lang-v2\n    # §4.1), and afkd",
        "share one namespace lang-v2  \n§4.1",
    ] {
        assert!(!hits(text).is_empty(), "{text}");
    }
}

#[test]
fn a_link_cited_twice_on_a_line_is_two_hits() {
    let text = "([lang-v2 §12.6](https://afkd.sh/docs/lang-v2/))";
    assert_eq!(hits(text).len(), 2, "{text}");
}

#[test]
fn the_records_file_and_an_uncited_name_are_not_hits() {
    for text in [
        r#"let path = afkd.join("docs/lang-v2.md");"#,
        "panic!(\"lang-v2.md has no heading\")",
        "(lang-v2\n/// and then some prose)",
        "lang-v2 is history\n§4.1 is a heading",
        "the reference §13.3 states it",
        "https://afkd.sh/docs/reference/#103-slots",
    ] {
        assert!(hits(text).is_empty(), "{text}: {:?}", hits(text));
    }
}

#[test]
fn a_source_is_excused_only_in_its_own_file() {
    let (path, fragment) = RECORD_SOURCES[0];
    let line = format!("/// §24's fixture, {fragment}, up to the");
    assert_eq!(hits(&line).len(), 1, "{line}");
    assert_eq!(record_source(path, &line), Some(0), "in {path}");
    assert_eq!(
        record_source("@afkd/web_top/fold.mjs", &line),
        None,
        "elsewhere"
    );
}
