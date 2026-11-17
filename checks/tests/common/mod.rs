//! What every repository-wide check reads: the repository root, and every path git tracks
//! under it.

use std::path::Path;
use std::process::Command;

/// The repository root, one up from this crate.
pub(crate) const REPO_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/..");

/// Every path `git ls-files -z` lists at the repository root.
pub(crate) fn tracked() -> Vec<String> {
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
