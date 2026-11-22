//! The plugin wire, as this plugin speaks it: the requests afkd sends, the replies
//! written back, and the one budget every reply line lives under.
//!
//! The protocol is afkd's (`docs/plugins.md`, "The trigger protocol"): one
//! newline-delimited JSON request at a time on stdin, one reply line each on stdout.
//! Nothing here is `deny_unknown_fields`, because the wire is additive within a `proto`:
//! a newer afkd's extra field must not break this plugin.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The plugin protocol version this plugin speaks.
pub(crate) const PROTO: u32 = 2;

/// The longest reply line afkd reads, **before** its newline: afkd caps a line at 64 KiB
/// including the `\n`, and a line of exactly 64 KiB not ending in one is over the cap.
pub(crate) const MAX_REPLY: usize = 64 * 1024 - 1;

/// One request, internally tagged on `call`.
#[derive(Debug, Deserialize)]
#[serde(tag = "call", rename_all = "snake_case")]
pub(crate) enum Request {
    /// Sent once, when the service arms: the protocol, the kind, the settings block, and
    /// `state_dir`, the plugin's own directory, which outlives the child (an older afkd
    /// leaves it out). The service's name, its roster and its owner ride along too; this
    /// kind reads none of them, so they are left undecoded.
    Hello {
        proto: u32,
        kind: String,
        settings: serde_json::Value,
        #[serde(default)]
        state_dir: Option<String>,
    },
    /// Sent once a beat.
    Poll,
    /// Sent after a unit's run ends. Its `outcome` and `facts` ride along too; the end of
    /// a run is the same whatever it was here — the hooks afkd runs after it are what
    /// differ — so they are left undecoded.
    Finish { key: String },
    /// A leftover journal key, or a unit handed back by a stop.
    Release { key: String },
    /// A live claim's periodic renewal, counting from 1.
    Renew { key: String, renewal: u64 },
    /// The mid-run watch's read of a unit's comments.
    Comments { key: String },
    /// One action a slot of afkd's calls, by its own name, its arguments bound by
    /// parameter name — the item it acts on among them, as the handle of a unit this
    /// plugin handed over. Answered by every proto 2 plugin, so it is never listed.
    Call {
        action: String,
        #[serde(default)]
        args: serde_json::Map<String, serde_json::Value>,
    },
    /// A call this plugin does not know — `classify` and `attempt_failed`, which the kind
    /// does not list, or one from a later afkd.
    #[serde(other)]
    Unknown,
}

impl Request {
    /// The name `hello` lists this call under, when it is one a kind may or may not
    /// answer; `None` for the calls every kind answers, and for [`Unknown`](Self::Unknown).
    pub(crate) fn optional_call(&self) -> Option<&'static str> {
        match self {
            Request::Release { .. } => Some("release"),
            Request::Renew { .. } => Some("renew"),
            Request::Comments { .. } => Some("comments"),
            Request::Hello { .. }
            | Request::Poll
            | Request::Finish { .. }
            | Request::Call { .. }
            | Request::Unknown => None,
        }
    }
}

/// One work item handed over in a `poll` reply.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct WireUnit {
    pub(crate) id: String,
    pub(crate) key: String,
    /// The claimed item's handle fields, every one the manifest declares for its kind.
    pub(crate) fields: Fields,
    pub(crate) thread: String,
    pub(crate) seen: Vec<String>,
    #[serde(rename = "self")]
    pub(crate) me: String,
    pub(crate) env: BTreeMap<String, String>,
    pub(crate) files: Vec<WireFile>,
}

/// A claimed item's handle fields, as the kind that claimed it declares them: each kind's
/// struct serializes alone, with no tag, as the `fields` object afkd types.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub(crate) enum Fields {
    Issue(IssueFields),
    Mr(MrFields),
}

/// The fields of a `gitlab.Issue` handle, in the manifest's order, as the claiming poll read
/// the issue. Each is always sent — an issue with no label has `[]` — so afkd never finds
/// one of them missing.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct IssueFields {
    pub(crate) title: String,
    pub(crate) url: String,
    pub(crate) number: u64,
    pub(crate) labels: Vec<String>,
}

/// The fields of a `gitlab.Merge_Request` handle, in the manifest's order, as the claiming
/// poll read the merge request.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct MrFields {
    pub(crate) title: String,
    pub(crate) url: String,
    pub(crate) number: u64,
    pub(crate) branch: String,
}

/// One file of the attempt's task folder, as data: afkd writes it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct WireFile {
    /// Where it lands, under `task/`.
    pub(crate) path: String,
    #[serde(flatten)]
    pub(crate) source: FileSource,
}

/// Where afkd gets a file's bytes: the text the reply carries, or a file on this machine it
/// copies in byte for byte on each attempt.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum FileSource {
    /// Inline text.
    Text(String),
    /// The absolute path of a file to copy.
    From(String),
}

impl WireFile {
    /// A file at `path` holding `text`.
    pub(crate) fn text(path: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            source: FileSource::Text(text.into()),
        }
    }

    /// A file at `path` that afkd copies from the absolute path `from`.
    pub(crate) fn from_path(path: impl Into<String>, from: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            source: FileSource::From(from.into()),
        }
    }
}

/// One comment in a `comments` reply.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct WireComment {
    pub(crate) id: String,
    pub(crate) author: String,
    pub(crate) author_name: String,
    pub(crate) body: String,
    pub(crate) at: String,
}

/// The `poll` reply that hands `unit` over, serialized.
pub(crate) fn fire_line(unit: &WireUnit) -> String {
    serde_json::json!({ "fire": true, "unit": unit }).to_string()
}

/// The `comments` reply over `comments`, serialized.
pub(crate) fn comments_line(comments: &[WireComment]) -> String {
    serde_json::json!({ "comments": comments }).to_string()
}

/// Fit a `poll` reply under [`MAX_REPLY`] by cutting the unit's `task/README.md` text, if it has
/// to be cut at all, and return the line. A file afkd copies in (`from`) is a path and is
/// never cut.
///
/// The cut keeps the longest prefix — at a char boundary — that still fits beside a
/// trailer saying the brief was cut and where the whole issue is. `None` when not even
/// the trailer alone fits, which only a unit whose *other* fields overflow can reach.
pub(crate) fn fit_poll(unit: &WireUnit) -> Option<String> {
    let line = fire_line(unit);
    if line.len() <= MAX_REPLY {
        return Some(line);
    }
    let brief = unit.files.iter().position(|f| f.path == crate::TASK_FILE)?;
    let FileSource::Text(text) = &unit.files[brief].source else {
        return None;
    };
    let with_prefix = |n: usize| {
        let mut cut = unit.clone();
        cut.files[brief].source = FileSource::Text(format!(
            "{}\n\n[afkd-gitlab: this brief was cut at {n} of {} bytes to fit afkd's 64 KiB \
             plugin line — read the whole issue with the gitlab skill]",
            &text[..n],
            text.len()
        ));
        fire_line(&cut)
    };
    // The serialized length only grows with the prefix, so the longest prefix that fits
    // is a binary search over byte offsets, each rounded down to a char boundary.
    let floor = |mut n: usize| {
        while !text.is_char_boundary(n) {
            n -= 1;
        }
        n
    };
    if with_prefix(0).len() > MAX_REPLY {
        return None;
    }
    let (mut fits, mut over) = (0, text.len());
    while over - fits > 1 {
        let mid = floor(fits + (over - fits) / 2);
        if mid == fits {
            break;
        }
        if with_prefix(mid).len() <= MAX_REPLY {
            fits = mid;
        } else {
            over = mid;
        }
    }
    Some(with_prefix(fits))
}

/// Fit a `comments` reply under [`MAX_REPLY`] by keeping the **newest** comments that fit,
/// oldest-first as `comments` is ordered. Returns the line and how many of the oldest were
/// left out.
pub(crate) fn fit_comments(comments: &[WireComment]) -> (String, usize) {
    // `{"comments":[` + the elements joined by `,` + `]}`: compact JSON serializes an
    // array element exactly as it serializes alone, so the line's length is additive.
    let frame = comments_line(&[]).len();
    let mut len = frame;
    let mut kept = 0;
    for comment in comments.iter().rev() {
        let size = serde_json::to_string(comment).map_or(usize::MAX, |s| s.len());
        let step = size.saturating_add(usize::from(kept > 0));
        if len.saturating_add(step) > MAX_REPLY {
            break;
        }
        len += step;
        kept += 1;
    }
    let dropped = comments.len() - kept;
    (comments_line(&comments[dropped..]), dropped)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(text: &str) -> WireUnit {
        WireUnit {
            id: "7".into(),
            key: "acme/sub.group/widgets#7#1000001".into(),
            fields: Fields::Issue(IssueFields {
                title: "修复 the retry storm 🚨".into(),
                url: "https://gitlab.example.com/acme/sub.group/widgets/-/issues/7".into(),
                number: 7,
                labels: vec!["afkd::ready".into(), "優先::high".into()],
            }),
            thread: "acme/sub.group/widgets#7".into(),
            seen: vec!["41".into(), "42".into()],
            me: "björn-öst[bot]".into(),
            env: BTreeMap::from([("GITLAB_ISSUE_NUMBER".to_string(), "7".to_string())]),
            files: vec![
                WireFile::text(crate::TASK_FILE, text),
                WireFile::text("task/issue/number", "7"),
            ],
        }
    }

    /// afkd's own request shapes decode, the calls this plugin never lists fall to
    /// `Unknown`, and an extra field from a later afkd is ignored.
    #[test]
    fn afkd_requests_decode() {
        let finish: Request = serde_json::from_str(
            r#"{"call":"finish","id":"7","key":"acme/sub.group/widgets#7#9","outcome":"park",
                "facts":{"signal":"fault","reason":"parked: awaiting a human reply",
                         "duration_ms":168000,"cost":0.4217,"turns":null,"tokens":null,
                         "run_name":"260925-100400-unit-7-1"},"later":1}"#,
        )
        .unwrap();
        let Request::Finish { key } = finish else {
            panic!("{finish:?}");
        };
        assert_eq!(key, "acme/sub.group/widgets#7#9");
        assert!(matches!(
            serde_json::from_str(r#"{"call":"poll"}"#).unwrap(),
            Request::Poll
        ));
        // The two calls the kind never lists fall to `Unknown`, and so name no optional
        // call — they are refused whether or not the service is armed.
        for unlisted in [
            r#"{"call":"attempt_failed","key":"k","n":1,"max":2,"reason":null}"#,
            r#"{"call":"classify","key":"k","scratch":"/tmp/s","outcome":"failed"}"#,
        ] {
            let request: Request = serde_json::from_str(unlisted).unwrap();
            assert!(matches!(request, Request::Unknown), "{unlisted}");
            assert_eq!(request.optional_call(), None);
        }
    }

    /// The `hello` afkd writes since it began telling a tracker plugin its service, its
    /// roster and its owner: the three fields this kind does not read are tolerated, and
    /// the ones it does decode as before.
    #[test]
    fn a_hello_with_the_service_roster_and_owner_decodes() {
        let hello: Request = serde_json::from_str(
            r#"{"call":"hello","proto":2,"kind":"issue","service":"監視::triage",
                "roster":["監視::triage","監視::nightly"],"owner":"björn-öst",
                "settings":{"project":"acme/sub.group/widgets","token":"PAT"}}"#,
        )
        .unwrap();
        let Request::Hello {
            proto,
            kind,
            settings,
            state_dir,
        } = hello
        else {
            panic!("{hello:?}");
        };
        assert_eq!((proto, kind.as_str()), (2, "issue"));
        assert_eq!(settings["project"], "acme/sub.group/widgets");
        assert_eq!(state_dir, None);
    }

    /// A `hello` carries the plugin's own directory as `state_dir`, and one without it (an
    /// older afkd) decodes to none.
    #[test]
    fn a_hello_decodes_with_and_without_a_state_dir() {
        let with: Request = serde_json::from_str(
            r#"{"call":"hello","proto":2,"kind":"mr","settings":{},
                "state_dir":"/home/you/.local/state/afkd/plugin-state/@afkd/gitlab 監視"}"#,
        )
        .unwrap();
        assert!(matches!(
            with,
            Request::Hello { state_dir: Some(dir), .. }
                if dir == "/home/you/.local/state/afkd/plugin-state/@afkd/gitlab 監視"
        ));
        let without: Request =
            serde_json::from_str(r#"{"call":"hello","proto":2,"kind":"mr","settings":{}}"#)
                .unwrap();
        assert!(matches!(
            without,
            Request::Hello {
                state_dir: None,
                ..
            }
        ));
    }

    /// A file serializes as its path and exactly one source: `text` for inline text, `from`
    /// for a file afkd copies in.
    #[test]
    fn a_file_serializes_with_one_source() {
        assert_eq!(
            serde_json::to_value(WireFile::text("task/README.md", "# 修复")).unwrap(),
            serde_json::json!({"path": "task/README.md", "text": "# 修复"})
        );
        assert_eq!(
            serde_json::to_value(WireFile::from_path(
                "task/screen_shot.png",
                "/state/task-files/k/screen_shot.png"
            ))
            .unwrap(),
            serde_json::json!({
                "path": "task/screen_shot.png",
                "from": "/state/task-files/k/screen_shot.png"
            })
        );
    }

    /// A `call` decodes with its arguments as afkd bound them, the item's handle among
    /// them; a top-level `key` an older afkd sent is not read, and an action with no
    /// parameters may leave `args` out.
    #[test]
    fn a_call_decodes_with_its_args() {
        let call: Request = serde_json::from_str(
            r#"{"call":"call","action":"add_label",
                "args":{"issue":{"id":"7","key":"acme/widgets#7#1000001"},
                        "label":"afkd/reviewed ✅"},
                "key":"acme/widgets#8#1000002","later":1}"#,
        )
        .unwrap();
        assert_eq!(call.optional_call(), None);
        let Request::Call { action, args } = call else {
            panic!("{call:?}");
        };
        assert_eq!(action, "add_label");
        assert_eq!(args["issue"]["key"], "acme/widgets#7#1000001");
        assert_eq!(args["label"], "afkd/reviewed ✅");
        assert!(
            !args.contains_key("key"),
            "the stray key is not an argument"
        );

        let bare: Request = serde_json::from_str(r#"{"call":"call","action":"mr_close"}"#).unwrap();
        assert!(matches!(
            bare,
            Request::Call { action, args } if action == "mr_close" && args.is_empty()
        ));
    }

    #[test]
    fn a_reply_that_fits_is_left_alone() {
        let u = unit("修复 the retry storm 🚨\n\nIt retries forever.");
        assert_eq!(fit_poll(&u), Some(fire_line(&u)));
    }

    /// A brief far over the line, made of multi-byte text so a byte cut would split a
    /// character: the line fits, decodes, ends in the trailer, and keeps as much of the
    /// brief as the budget allows (within one character of the limit).
    #[test]
    fn an_oversized_brief_is_cut_at_a_char_boundary_to_fit() {
        let body = "看起来不对 🚨 — ".repeat(8_000);
        let u = unit(&body);
        let line = fit_poll(&u).expect("the cut fits");
        assert!(line.len() <= MAX_REPLY, "{}", line.len());
        assert!(
            line.len() > MAX_REPLY - 8,
            "the cut wastes budget: {}",
            line.len()
        );
        let reply: serde_json::Value = serde_json::from_str(&line).unwrap();
        let text = reply["unit"]["files"][0]["text"].as_str().unwrap();
        assert!(
            text.ends_with("read the whole issue with the gitlab skill]"),
            "{}",
            &text[text.len() - 200..]
        );
        assert!(
            text.contains(&format!("of {} bytes", body.len())),
            "names the whole size"
        );
        assert!(body.starts_with(text.split("\n\n[afkd-gitlab:").next().unwrap()));
        // Everything but the brief is untouched.
        assert_eq!(reply["unit"]["key"], "acme/sub.group/widgets#7#1000001");
        assert_eq!(reply["unit"]["files"][1]["text"], "7");
    }

    /// A cut brief leaves the files afkd copies in whole: their `from` entries ride the
    /// line, and their paths count against what the brief may keep.
    #[test]
    fn a_cut_brief_leaves_the_from_entries_whole() {
        let mut u = unit(&"看起来不对 🚨 — ".repeat(8_000));
        u.files.push(WireFile::from_path(
            "task/screen shot 修复.png",
            "/state/task-files/acme%2Fwidgets%237/screen shot 修复.png",
        ));
        let line = fit_poll(&u).expect("the cut fits");
        assert!(line.len() <= MAX_REPLY && line.len() > MAX_REPLY - 8);
        let reply: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert!(reply["unit"]["files"][0]["text"]
            .as_str()
            .unwrap()
            .contains("this brief was cut"));
        assert_eq!(
            reply["unit"]["files"][2],
            serde_json::json!({
                "path": "task/screen shot 修复.png",
                "from": "/state/task-files/acme%2Fwidgets%237/screen shot 修复.png"
            })
        );
    }

    #[test]
    fn a_comments_reply_keeps_the_newest_that_fit() {
        let comments: Vec<WireComment> = (0..400)
            .map(|i| WireComment {
                id: i.to_string(),
                author: "álvaro".into(),
                author_name: "álvaro".into(),
                body: "x".repeat(300),
                at: "2026-09-25T09:58:00Z".into(),
            })
            .collect();
        let (line, dropped) = fit_comments(&comments);
        assert!(line.len() <= MAX_REPLY);
        assert!(dropped > 0 && dropped < comments.len());
        let reply: serde_json::Value = serde_json::from_str(&line).unwrap();
        let kept = reply["comments"].as_array().unwrap();
        assert_eq!(kept.len(), 400 - dropped);
        assert_eq!(kept.last().unwrap()["id"], "399", "the newest survive");
        assert_eq!(kept[0]["id"], dropped.to_string());
        // One more comment would not have fit.
        assert!(comments_line(&comments[dropped - 1..]).len() > MAX_REPLY);
    }
}
