//! The release notes against the README: an operator's `afkd update` lands a release on
//! whatever afkd they run, so the notes the release workflow writes must name the same afkd
//! floor the README's "Install it" section states, and the comment above the notes' `printf`
//! must give that floor's reason. The release workflow's `test` job runs this suite before it
//! builds, so a floor that drifts never publishes.

use std::fs;

const README: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/README.md");
const WORKFLOW: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../.github/workflows/release-linear.yml"
);

/// Every stretch of `text` that follows an occurrence of `lead` and runs up to the next `end`.
fn version_after<'t>(text: &'t str, lead: &str, end: &str) -> Vec<&'t str> {
    text.match_indices(lead)
        .filter_map(|(at, _)| {
            let rest = &text[at + lead.len()..];
            rest.find(end).map(|stop| &rest[..stop])
        })
        .collect()
}

/// Whether `v` is three `.`-separated runs of ASCII digits, `0.2.693`.
fn is_version(v: &str) -> bool {
    let parts: Vec<&str> = v.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// The one version a `version_after` found, or why there is not exactly one.
fn one_version(found: Vec<&str>, what: &str) -> Result<String, String> {
    match found.as_slice() {
        [v] if is_version(v) => Ok((*v).to_string()),
        [v] => Err(format!("{what} is {v:?}, not a version")),
        [] => Err(format!("no {what}")),
        many => Err(format!("{what} is stated {} times: {many:?}", many.len())),
    }
}

/// The afkd floor the README's `## Install it` section states, "Installing it needs **afkd X
/// or newer**", read with its whitespace collapsed so a line wrap inside the phrase does not
/// matter. The configs' language floor, worded otherwise, is never it.
fn install_floor(readme: &str) -> Result<String, String> {
    let mut lines = readme.lines().skip_while(|l| *l != "## Install it");
    if lines.next().is_none() {
        return Err("no `## Install it` section".to_string());
    }
    let section = lines
        .take_while(|l| !l.starts_with("## "))
        .flat_map(str::split_whitespace)
        .collect::<Vec<_>>()
        .join(" ");
    one_version(
        version_after(&section, "Installing it needs **afkd ", " or newer**"),
        "install floor in `## Install it`",
    )
}

/// The afkd floor the notes state, "Needs afkd X or newer" in the one `printf` that writes
/// `notes.md`, and the comment directly above that `printf`, joined into one line.
fn notes_floor(workflow: &str) -> Result<(String, String), String> {
    let lines: Vec<&str> = workflow.lines().collect();
    let printfs: Vec<usize> = (0..lines.len())
        .filter(|&i| {
            let line = lines[i].trim_start();
            line.starts_with("printf ") && line.contains("> notes.md")
        })
        .collect();
    let at = match printfs.as_slice() {
        [at] => *at,
        [] => return Err("no `printf` writes notes.md".to_string()),
        many => return Err(format!("{} `printf`s write notes.md", many.len())),
    };
    let floor = one_version(
        version_after(lines[at], "Needs afkd ", " or newer"),
        "floor in the notes",
    )?;
    let mut comment: Vec<&str> = lines[..at]
        .iter()
        .rev()
        .map(|l| l.trim_start())
        .take_while(|l| l.starts_with('#'))
        .map(|l| l.trim_start_matches('#').trim())
        .collect();
    comment.reverse();
    Ok((floor, comment.join(" ")))
}

#[test]
fn the_release_notes_name_the_afkd_the_readme_requires() {
    let read = |path| fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let readme = install_floor(&read(README)).unwrap_or_else(|e| panic!("{README}: {e}"));
    let (notes, comment) =
        notes_floor(&read(WORKFLOW)).unwrap_or_else(|e| panic!("{WORKFLOW}: {e}"));
    assert_eq!(
        notes, readme,
        "the release notes {WORKFLOW} writes name afkd {notes} or newer, but {README} says \
         installing it needs afkd {readme}: change the `Needs afkd` sentence in the Release \
         step's `printf`, and the comment above it, to the README's floor"
    );
    assert!(
        comment.contains(&format!("{readme} is the first")),
        "the comment above the notes' `printf` in {WORKFLOW} does not give why afkd {readme} \
         is the floor: {comment:?}"
    );
}

#[test]
fn a_floor_is_read_where_its_phrase_puts_it() {
    let readme =
        "# @afkd/linear\n\n## Install it\n\n```console\n$ afkd install @afkd/linear\n```\n\n\
        Nothing is compiled on your host. Installing it needs **afkd\n0.2.693 or newer**, the \
        first whose manifests declare a `list[str]` an action returns.\n\nThe configs below are \
        written in the afk language of **afkd 0.2.704 or newer**.\n\n## Configure it\n\n\
        Installing it needs **afkd 9.9.9 or newer** on a host of its own.\n";
    assert_eq!(install_floor(readme), Ok("0.2.693".to_string()), "{readme}");

    let workflow =
        "        run: |\n          ls -l $assets\n          # An operator's `afkd update` \
        lands the new manifest on whatever afkd they run, so\n          # the notes name the \
        afkd it needs: 0.2.693 is the first whose manifests\n          # declare a `list[str]` \
        an action returns.\n          printf '`afkd install @afkd/linear`.\\n\\nNeeds afkd \
        0.2.693 or newer, the first whose manifests declare a `list[str]` an action \
        returns.\\n\\nCut from %s.\\n' \"$GITHUB_SHA\" > notes.md\n          gh release delete \
        linear --yes --cleanup-tag || true\n";
    assert_eq!(
        notes_floor(workflow),
        Ok((
            "0.2.693".to_string(),
            "An operator's `afkd update` lands the new manifest on whatever afkd they run, so \
             the notes name the afkd it needs: 0.2.693 is the first whose manifests declare a \
             `list[str]` an action returns."
                .to_string()
        )),
        "{workflow}"
    );

    let misses = [
        (
            "outside its section",
            install_floor(
                "## Install it\n\nRun it.\n\n## Use it\n\nInstalling it needs **afkd 0.2.693 or \
                 newer**.\n",
            ),
        ),
        (
            "twice in its section",
            install_floor(
                "## Install it\n\nInstalling it needs **afkd 0.2.693 or newer**; Installing it \
                 needs **afkd 0.2.704 or newer**.\n",
            ),
        ),
        (
            "not a version",
            install_floor("## Install it\n\nInstalling it needs **afkd 0.2.x or newer**.\n"),
        ),
        (
            "no section",
            install_floor("## Use it\n\nInstalling it needs **afkd 0.2.693 or newer**.\n"),
        ),
    ];
    for (why, got) in misses {
        assert!(got.is_err(), "a README floor {why} reads as {got:?}");
    }
    for (why, workflow) in [
        (
            "no `Needs afkd`",
            "printf 'Cut from %s.\\n' \"$GITHUB_SHA\" > notes.md\n",
        ),
        (
            "not a version",
            "printf 'Needs afkd 0.2.x or newer.\\n' > notes.md\n",
        ),
        (
            "no `printf`",
            "echo 'Needs afkd 0.2.693 or newer.' > notes.md\n",
        ),
    ] {
        let got = notes_floor(workflow);
        assert!(got.is_err(), "a notes floor with {why} reads as {got:?}");
    }
}
