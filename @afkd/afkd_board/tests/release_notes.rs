//! The release notes against the README: an operator's `afkd update` lands a release on
//! whatever afkd they run, so the notes the release workflow writes must name the same afkd
//! floor the README's "Install it" section states, and the comment above the notes' `printf`
//! must give that floor's reason. The release workflow's `afkd_floor` job holds the release back
//! until a stable afkd release is at or past that same floor, so its `AFKD_FLOOR` is held to the
//! README too, and so is the shape of the job that keeps it from publishing early or failing
//! quietly. The release workflow's `test` job runs this suite before it builds, so a floor that
//! drifts never publishes.

use std::fs;

const README: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/README.md");
const WORKFLOW: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../.github/workflows/release-afkd_board.yml"
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

/// The lines of the job `name` of `workflow`: those after its `  name:` line that are blank or
/// indented under it, up to the next job or comment at the jobs' own indent.
fn job(workflow: &str, name: &str) -> Result<Vec<String>, String> {
    let head = format!("  {name}:");
    let mut lines = workflow.lines().skip_while(|l| *l != head);
    if lines.next().is_none() {
        return Err(format!("no `{name}` job"));
    }
    Ok(lines
        .take_while(|l| l.trim().is_empty() || l.starts_with("    "))
        .map(str::to_string)
        .collect())
}

/// The `AFKD_FLOOR` the `afkd_floor` job sets, once the job and the release it holds back are
/// checked: it authenticates `gh` with the workflow's token, fails on a `gh` failure rather than
/// losing it inside a pipe (`pipefail`, and the `gh` its own command), takes the stable tags with
/// a `grep` that finds none as `ready=false` (`|| true`), writes `ready`, and the release needs
/// the job and runs only when `ready == 'true'`.
fn floor_gate(workflow: &str) -> Result<String, String> {
    let floor = job(workflow, "afkd_floor")?;
    let has = |text: &str| floor.iter().any(|l| l.contains(text));
    let at = |text: &str| floor.iter().position(|l| l.contains(text));
    let set: Vec<&str> = floor
        .iter()
        .filter_map(|l| l.trim().strip_prefix("AFKD_FLOOR:"))
        .map(|v| v.trim().trim_matches('"'))
        .collect();
    let floor_version = one_version(set, "`AFKD_FLOOR` in the `afkd_floor` job")?;
    if !has("GH_TOKEN: ${{ github.token }}") {
        return Err("the `afkd_floor` job does not set GH_TOKEN to the workflow's token".into());
    }
    let (Some(pipefail), Some(tags)) = (at("set -euo pipefail"), at("tags=$(gh release list"))
    else {
        return Err(
            "the `afkd_floor` script has no `set -euo pipefail` or no `tags=$(gh …)`".into(),
        );
    };
    if pipefail > tags || floor[tags].contains('|') {
        return Err(
            "the `afkd_floor` script reads the tags with `gh` as its own command, after \
             `set -euo pipefail`, so a `gh` failure fails the job"
                .into(),
        );
    }
    for flag in [
        "--repo afkd-sh/afkd",
        "--exclude-pre-releases",
        "--exclude-drafts",
    ] {
        if !floor[tags].contains(flag) {
            return Err(format!(
                "the `afkd_floor` script's `gh release list` has no `{flag}`"
            ));
        }
    }
    let Some(grep) = floor.iter().find(|l| l.contains("grep -E")) else {
        return Err("the `afkd_floor` script takes the stable tags with no `grep -E`".into());
    };
    if !grep.contains("|| true") {
        return Err("the `afkd_floor` script's `grep` has no `|| true`, so no stable tag fails the job rather than giving `ready=false`".into());
    }
    if !has(r#"echo "ready=$ready" >> "$GITHUB_OUTPUT""#) {
        return Err("the `afkd_floor` script does not write `ready` to $GITHUB_OUTPUT".into());
    }
    let release = job(workflow, "release")?;
    if !release
        .iter()
        .any(|l| l.trim() == "needs: [build, afkd_floor]")
    {
        return Err("the `release` job does not need `[build, afkd_floor]`".into());
    }
    let gated = release.iter().any(|l| {
        l.trim().starts_with("if:") && l.contains("needs.afkd_floor.outputs.ready == 'true'")
    });
    if !gated {
        return Err("the `release` job does not run only when `ready == 'true'`".into());
    }
    Ok(floor_version)
}

#[test]
fn the_release_waits_for_the_afkd_floor() {
    let read = |path| fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let readme = install_floor(&read(README)).unwrap_or_else(|e| panic!("{README}: {e}"));
    let workflow = read(WORKFLOW);
    let floor = floor_gate(&workflow).unwrap_or_else(|e| panic!("{WORKFLOW}: {e}"));
    assert_eq!(
        floor, readme,
        "{WORKFLOW}'s `afkd_floor` job waits for afkd {floor}, but {README} says installing it \
         needs afkd {readme}: change `AFKD_FLOOR` to the README's floor"
    );
    for (why, broken) in [
        (
            "no pipefail",
            workflow.replace("set -euo pipefail", "set -eu"),
        ),
        (
            "no GH_TOKEN",
            workflow.replace("GH_TOKEN: ${{ github.token }}", "GH: x"),
        ),
        (
            "a grep that fails with no stable tag",
            workflow.replace("<<<\"$tags\" || true", "<<<\"$tags\""),
        ),
        (
            "a gh inside a pipe",
            workflow.replace("'.[].tagName')", "'.[].tagName' | sort)"),
        ),
        (
            "a release not gated on ready",
            workflow.replace(" && needs.afkd_floor.outputs.ready == 'true'", ""),
        ),
        (
            "a release that does not need the floor",
            workflow.replace("needs: [build, afkd_floor]", "needs: build"),
        ),
        (
            "a floor that is no version",
            workflow.replace("AFKD_FLOOR: ", "AFKD_FLOOR: 0."),
        ),
    ] {
        let got = floor_gate(&broken);
        assert!(got.is_err(), "a workflow with {why} reads as {got:?}");
    }
}

#[test]
fn a_floor_is_read_where_its_phrase_puts_it() {
    let readme = "# @afkd/afkd_board\n\n## Install it\n\n```console\n\
        $ afkd install @afkd/afkd_board\n```\n\n\
        Nothing is compiled on your host. Installing it needs **afkd\n0.2.693 or newer**, the \
        first whose manifests declare a `list[str]` an action returns.\n\nThe configs below are \
        written in the afk language of **afkd 0.2.704 or newer**.\n\n## Configure it\n\n\
        Installing it needs **afkd 9.9.9 or newer** on a host of its own.\n";
    assert_eq!(install_floor(readme), Ok("0.2.693".to_string()), "{readme}");

    let workflow =
        "        run: |\n          ls -l $assets\n          # An operator's `afkd update` \
        lands the new manifest on whatever afkd they run, so\n          # the notes name the \
        afkd it needs: 0.2.693 is the first whose manifests\n          # declare a `list[str]` \
        an action returns.\n          printf '`afkd install @afkd/afkd_board`.\\n\\nNeeds afkd \
        0.2.693 or newer, the first whose manifests declare a `list[str]` an action \
        returns.\\n\\nCut from %s.\\n' \"$GITHUB_SHA\" > notes.md\n          gh release delete \
        board --yes --cleanup-tag || true\n";
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
