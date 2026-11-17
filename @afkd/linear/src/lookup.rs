//! Turning the names a write is passed into Linear's ids: a team by its key, a state, a
//! project or a milestone by its name, a user by display name or email (or `"me"`), a cycle
//! by its number.
//!
//! A name is matched exactly, and an object's own id is taken in its place, so the ids an
//! ambiguous name is refused with can be passed back. A name that matches nothing is refused
//! naming what there is, and one that matches several listing their ids ([`pick`]). Each kind's
//! lookup reads the candidates whole, through the client of the call, and only for a name the
//! call was given.

use crate::client::LinearClient;
use crate::settings::ME;
use crate::tracker::{LinearError, Team, Tracker};

/// One object a name may name: its id, its name, which a refusal shows, and the other name it
/// answers to, a user's email.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Candidate {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) alias: Option<String>,
}

impl Candidate {
    /// A candidate answering to its `name` alone.
    fn named(id: String, name: String) -> Self {
        Candidate {
            id,
            name,
            alias: None,
        }
    }

    /// Whether `name` is one of its names.
    fn answers_to(&self, name: &str) -> bool {
        self.name == name || self.alias.as_deref() == Some(name)
    }
}

/// What [`pick`] looks a name up among: the kind of object, and where, as a refusal says it
/// (` in team ENG`, or nothing for the whole workspace).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Among<'a> {
    pub(crate) what: &'a str,
    pub(crate) scope: &'a str,
}

/// The one of `candidates` that `name`, `action`'s argument, names: the one whose id it is,
/// else the one that answers to it.
///
/// # Errors
///
/// Returns the sentence refusing a name none of them answers to, which lists the names there
/// are, and one several answer to, which lists their ids.
pub(crate) fn pick<'a>(
    action: &str,
    among: Among<'_>,
    name: &str,
    candidates: &'a [Candidate],
) -> Result<&'a Candidate, String> {
    let Among { what, scope } = among;
    if let Some(by_id) = candidates.iter().find(|c| c.id == name) {
        return Ok(by_id);
    }
    let named: Vec<&Candidate> = candidates.iter().filter(|c| c.answers_to(name)).collect();
    match named.as_slice() {
        [one] => Ok(one),
        [] => {
            let there: Vec<&str> = candidates.iter().map(|c| c.name.as_str()).collect();
            Err(format!(
                "{action}: no {what} named {name:?}{scope}; {}",
                there_are(&there)
            ))
        }
        several => {
            let ids: Vec<&str> = several.iter().map(|c| c.id.as_str()).collect();
            Err(format!(
                "{action}: {} {what}s are named {name:?}{scope}: {}; pass the id of the one you \
                 mean in place of the name",
                several.len(),
                ids.join(", ")
            ))
        }
    }
}

/// What a refusal says there is: `there are: a, b`, or `there are none`.
fn there_are<S: AsRef<str>>(there: &[S]) -> String {
    if there.is_empty() {
        return "there are none".to_string();
    }
    let there: Vec<&str> = there.iter().map(AsRef::as_ref).collect();
    format!("there are: {}", there.join(", "))
}

/// The sentence of a read a lookup made that Linear refused.
fn linear(e: LinearError) -> String {
    e.to_string()
}

/// The team whose key, or id, is `key`, among the teams the credential reaches.
///
/// # Errors
///
/// Returns the sentence of [`pick`], or of the read Linear refused.
pub(crate) fn team(client: &LinearClient, action: &str, key: &str) -> Result<Team, String> {
    let candidates: Vec<Candidate> = client
        .team_list()
        .map_err(linear)?
        .into_iter()
        .map(|(id, key, _)| Candidate::named(id, key))
        .collect();
    let among = Among {
        what: "team",
        scope: "",
    };
    let team = pick(action, among, key, &candidates)?;
    Ok(Team {
        id: team.id.clone(),
        key: team.name.clone(),
    })
}

/// The state of the team `team` named `name`, or whose id it is, among its states in their
/// order: its id and its name.
///
/// # Errors
///
/// Returns the sentence of [`pick`], or of the read Linear refused.
pub(crate) fn state(
    client: &LinearClient,
    action: &str,
    team: &Team,
    name: &str,
) -> Result<Candidate, String> {
    let mut states = client.states(&team.id).map_err(linear)?;
    states.sort_by(|a, b| a.position.total_cmp(&b.position));
    let candidates: Vec<Candidate> = states
        .into_iter()
        .map(|s| Candidate::named(s.id, s.name))
        .collect();
    let scope = format!(" in team {}", team.key);
    let among = Among {
        what: "state",
        scope: &scope,
    };
    pick(action, among, name, &candidates).cloned()
}

/// The id of the user `name` names: `"me"` whoever the credential acts as, else the user whose
/// display name or email, or id, it is, among the users Linear lists, apps among them and
/// disabled accounts not.
///
/// # Errors
///
/// Returns the sentence of [`pick`], or of the read Linear refused.
pub(crate) fn user(client: &LinearClient, action: &str, name: &str) -> Result<String, String> {
    if name == ME {
        return client.viewer().map_err(linear);
    }
    let candidates: Vec<Candidate> = client
        .user_list()
        .map_err(linear)?
        .into_iter()
        .map(|(id, user)| Candidate {
            id,
            name: user.name,
            alias: Some(user.email),
        })
        .collect();
    let among = Among {
        what: "user",
        scope: "",
    };
    Ok(pick(action, among, name, &candidates)?.id.clone())
}

/// The project named `name`, or whose id it is, among the workspace's: its id and its name.
///
/// # Errors
///
/// Returns the sentence of [`pick`], or of the read Linear refused.
pub(crate) fn project(
    client: &LinearClient,
    action: &str,
    name: &str,
) -> Result<Candidate, String> {
    let candidates: Vec<Candidate> = client
        .project_list(None)
        .map_err(linear)?
        .into_iter()
        .map(|(id, project)| Candidate::named(id, project.name))
        .collect();
    let among = Among {
        what: "project",
        scope: "",
    };
    pick(action, among, name, &candidates).cloned()
}

/// The id of the milestone named `name`, or whose id it is, of the project `project`.
///
/// # Errors
///
/// Returns the sentence of [`pick`], or of the read Linear refused.
pub(crate) fn milestone(
    client: &LinearClient,
    action: &str,
    project: &Candidate,
    name: &str,
) -> Result<String, String> {
    let candidates: Vec<Candidate> = client
        .milestone_list(&project.id)
        .map_err(linear)?
        .into_iter()
        .map(|(id, name)| Candidate::named(id, name))
        .collect();
    let scope = format!(" in project {}", project.name);
    let among = Among {
        what: "milestone",
        scope: &scope,
    };
    Ok(pick(action, among, name, &candidates)?.id.clone())
}

/// The id of the cycle numbered `number` of the team `team`.
///
/// # Errors
///
/// Returns the sentence refusing a number the team has no cycle of, which lists the numbers
/// there are, or of the read Linear refused.
pub(crate) fn cycle(
    client: &LinearClient,
    action: &str,
    team: &Team,
    number: i64,
) -> Result<String, String> {
    let mut cycles = client.cycle_list(&team.id).map_err(linear)?;
    cycles.sort_by_key(|(_, cycle)| cycle.number);
    if let Some((id, _)) = cycles.iter().find(|(_, cycle)| cycle.number == number) {
        return Ok(id.clone());
    }
    let there: Vec<String> = cycles.iter().map(|(_, c)| c.number.to_string()).collect();
    Err(format!(
        "{action}: no cycle {number} in team {}; {}",
        team.key,
        there_are(&there)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(id: &str, name: &str, alias: Option<&str>) -> Candidate {
        Candidate {
            id: id.into(),
            name: name.into(),
            alias: alias.map(str::to_string),
        }
    }

    /// A name that answers to one candidate is its id, by its name or its alias; an id is
    /// taken in a name's place, ahead of a candidate that answers to it as a name; none is
    /// refused listing the names there are in their order, or that there are none; and
    /// several listing their ids.
    #[test]
    fn a_name_picks_one_or_is_refused() {
        let users = [
            candidate("u-1", "sam", Some("sam@acme.example")),
            candidate("u-2", "陳大文", Some("chen@acme.example")),
            candidate("u-3", "sam", Some("sam.k@acme.example")),
            candidate("u-4", "u-1", Some("odd@acme.example")),
        ];
        let among = Among {
            what: "user",
            scope: "",
        };
        let pick = |name: &str| pick("update_issue", among, name, &users).map(|c| c.id.clone());
        assert_eq!(pick("陳大文"), Ok("u-2".into()));
        assert_eq!(pick("chen@acme.example"), Ok("u-2".into()));
        assert_eq!(pick("sam.k@acme.example"), Ok("u-3".into()));
        assert_eq!(pick("u-3"), Ok("u-3".into()));
        assert_eq!(pick("u-1"), Ok("u-1".into()), "an id before a name");
        assert_eq!(
            pick("sam"),
            Err(
                "update_issue: 2 users are named \"sam\": u-1, u-3; pass the id of the one you \
                 mean in place of the name"
                    .into()
            )
        );
        assert_eq!(
            pick("Sam"),
            Err("update_issue: no user named \"Sam\"; there are: sam, 陳大文, sam, u-1".into()),
            "a name is matched exactly"
        );
        let states = [
            candidate("s-1", "Todo", None),
            candidate("s-2", "Done", None),
        ];
        let in_eng = Among {
            what: "state",
            scope: " in team ENG",
        };
        assert_eq!(
            super::pick("update_issue", in_eng, "Doing", &states).map(|c| c.id.clone()),
            Err("update_issue: no state named \"Doing\" in team ENG; there are: Todo, Done".into())
        );
        let in_project = Among {
            what: "milestone",
            scope: " in project Trackers 🧭",
        };
        assert_eq!(
            super::pick("update_issue", in_project, "B", &[]).map(|c| c.id.clone()),
            Err(
                "update_issue: no milestone named \"B\" in project Trackers 🧭; there are none"
                    .into()
            )
        );
    }
}
