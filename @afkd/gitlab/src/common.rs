//! Drive scaffolding the kind is built from (ADR-0041): the env spellings, the poll's scan
//! budget, the credentials-env builder, the lifecycle-action executor over the
//! [`GitlabClient`] seam, the **claim** — a `[afkd-claim]` marker note decided by
//! [`crate::claim`]'s pure winner rule — and the two seams the rest is written against:
//! the [`Clock`] the claim settles on and the [`Diag`] sink diagnostics go to.
//!
//! Ported from afkd's `crates/gitlab/src/common.rs`. The claim is a marker, not an
//! assignee: GitLab's assignee write is a **replace-set** `PUT` (`assignee_ids`), so an
//! assign-and-re-read claim could hand *both* contenders a win. The marker is written into
//! the item's append-only note log, settled, re-read, and won iff a pure function of that
//! shared list names us. The assignee and the `afkd::claimed` label are **visible
//! status**, written after the claim is decided; the label is also the re-pick gate, so
//! the plugin — not a user's `on_claim` list — adds it.
//!
//! GitLab's identity is an id **and** a username: the assignee verbs write and compare
//! numeric ids, while the marker's owner field and the note author compare usernames.
//! GitLab has no self-scoped assignee verb, so `assign_me` and `unassign` are
//! **read-modify-write** over the assignee set, and neither can evict a co-assignee it did
//! not write.
//!
//! One thing the built-in has and this does not: afkd's stop. The plugin cannot see it,
//! so a claim never abandons mid-settle; afkd hands a unit polled during a stop straight
//! back with `release`, which is the same end state.

use std::collections::BTreeMap;
use std::fmt::Display;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::claim::{
    claim_key, claim_renewal_text, claim_text, split_claim_key, won_claim, ClaimMarker,
    CLAIM_LIFETIME, CLAIM_SETTLE,
};
use crate::client::{GitlabClient, GitlabError, ItemKind, Note, Project, User};
use crate::lifecycle::LifecycleAction;
use crate::run_ref;
use crate::settings::GitlabConfig;
use crate::wire::Facts;

/// The in-progress label the claim adds (and `on_fail` removes by name). A fixed internal
/// name, like the `[afkd-claim]` marker; GitLab labels are commonly scoped, so this uses
/// the `::` convention. GitLab creates a label on first use, so the add always lands.
pub(crate) const CLAIMED_LABEL: &str = "afkd::claimed";

/// Env var carrying the personal/project access token to the run.
pub(crate) const ENV_TOKEN: &str = "GITLAB_TOKEN";
/// Env var carrying the instance base URL to the run.
pub(crate) const ENV_BASE_URL: &str = "GITLAB_BASE_URL";
/// Env var carrying the active project (raw setting) to the run.
pub(crate) const ENV_PROJECT: &str = "GITLAB_PROJECT";
/// Env var carrying the active issue iid to the run.
pub(crate) const ENV_ISSUE_NUMBER: &str = "GITLAB_ISSUE_NUMBER";

/// How long one `poll`'s scan may run before it stops considering further candidates,
/// leaving them to the next beat. afkd ends the service if a call takes 60 seconds, and
/// each claim attempt settles for a second; this keeps the scan well inside it. A claim
/// already under way is always finished.
pub(crate) const POLL_BUDGET: Duration = Duration::from_secs(20);

/// Lock `m`, recovering the data from a poisoned lock rather than panicking: a panic
/// elsewhere must not take every later caller down with it.
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The time the claim settles on and the poll budget is measured with.
pub(crate) trait Clock {
    /// Block for `d`.
    fn sleep(&self, d: Duration);
    /// The current monotonic instant.
    fn now(&self) -> Instant;
}

/// The real clock.
pub(crate) struct SystemClock;

impl Clock for SystemClock {
    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }

    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// Where a diagnostic goes. stdout is afkd's protocol, so in the running plugin this is
/// stderr, which afkd streams into the service log under `[@afkd/gitlab:err]`.
pub(crate) trait Diag {
    /// Surface one problem.
    fn err(&self, err: &dyn Display);
}

/// The running plugin's [`Diag`]: one `afkd-gitlab: …` line on stderr per problem.
pub(crate) struct StderrDiag;

impl Diag for StderrDiag {
    fn err(&self, err: &dyn Display) {
        eprintln!("afkd-gitlab: {err}");
    }
}

/// One `poll`'s scan against [`POLL_BUDGET`], started when the scan starts.
pub(crate) struct ScanBudget<'a> {
    clock: &'a dyn Clock,
    diag: &'a dyn Diag,
    started: Instant,
}

impl<'a> ScanBudget<'a> {
    pub(crate) fn start(clock: &'a dyn Clock, diag: &'a dyn Diag) -> Self {
        Self {
            clock,
            diag,
            started: clock.now(),
        }
    }

    /// Whether the scan has run its budget — said on the diagnostic channel when it has,
    /// since the scan stops there.
    pub(crate) fn spent(&self) -> bool {
        let spent = self.clock.now().duration_since(self.started) >= POLL_BUDGET;
        if spent {
            self.diag.err(&format_args!(
                "gitlab poll: the scan ran past its {}s budget; the rest of it waits for the \
                 next poll",
                POLL_BUDGET.as_secs()
            ));
        }
        spent
    }
}

/// The credentials merged into every run's environment: the token and base URL (the
/// project + iid are per-unit).
pub(crate) fn creds_env(cfg: &GitlabConfig) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert(ENV_TOKEN.to_string(), cfg.token.clone());
    env.insert(ENV_BASE_URL.to_string(), cfg.base_url.clone());
    env
}

/// The unit's stable identity: `project#iid`, **the same across every claim of the same
/// item** — the agent-session thread (ADR-0067). The claim-journal key carries the claim
/// marker on top of it ([`claim_key_for`]).
pub(crate) fn unit_key(project: &Project, iid: u64) -> String {
    format!("{}#{iid}", project.raw())
}

/// The claim-journal key for one claim of a unit (ADR-0059): [`unit_key`]'s two halves
/// plus the claim marker's note id, so a reaper can delete a crashed run's marker rather
/// than leaving it to block the next claim for an hour.
pub(crate) fn claim_key_for(project: &Project, iid: u64, marker_id: u64) -> String {
    claim_key(project.raw(), iid, marker_id)
}

/// Run a lifecycle moment's actions in order, stopping at the first failure, which is
/// returned for the caller to log. `facts` are what the finishing run did (the pre-run
/// `on_claim` passes [`Facts::none`]); a `comment` interpolates its `@{run:…}` references
/// against them.
pub(crate) fn apply_actions(
    client: &dyn GitlabClient,
    project: &Project,
    kind: ItemKind,
    iid: u64,
    actions: &[LifecycleAction],
    me: &User,
    facts: &Facts,
) -> Result<(), GitlabError> {
    for action in actions {
        do_action(client, project, kind, iid, action, me, facts)?;
    }
    Ok(())
}

/// Carry out one lifecycle action against an item.
///
/// `LabelRemove` removes the single named label via `remove_labels` (**never** an
/// all-clearing path — GitLab has none); no name→id lookup is needed. `Close` uses the
/// `close` state event. `AssignMe`/`Unassign` are [`assign_me`]/[`unassign`], which read
/// the set before replacing it so neither evicts a human.
pub(crate) fn do_action(
    client: &dyn GitlabClient,
    project: &Project,
    kind: ItemKind,
    iid: u64,
    action: &LifecycleAction,
    me: &User,
    facts: &Facts,
) -> Result<(), GitlabError> {
    match action {
        LifecycleAction::AssignMe => assign_me(client, project, kind, iid, me),
        LifecycleAction::Unassign => unassign(client, project, kind, iid, me),
        LifecycleAction::LabelAdd(name) => client.add_label(project, kind, iid, name),
        LifecycleAction::LabelRemove(name) => client.remove_label(project, kind, iid, name),
        LifecycleAction::Close => client.set_state(project, kind, iid, "close"),
        // Interpolate `@{run:…}` against the finishing run's facts (ADR-0064). The
        // settings reader has already proven every reference legal for this moment.
        LifecycleAction::Comment(text) => client
            .post_comment(project, kind, iid, &run_ref::substitute(text, facts))
            .map(|_| ()),
    }
}

/// Add the bot to an item's assignees, keeping everyone already there.
///
/// GitLab has no self-scoped assignee verb — `assignee_ids` is one replacing `PUT` — so
/// the union is read-modify-write over [`GitlabClient::get_assignees`]. A set that already
/// carries us is left alone rather than re-`PUT`, so a status write that changes nothing
/// costs nothing.
fn assign_me(
    client: &dyn GitlabClient,
    project: &Project,
    kind: ItemKind,
    iid: u64,
    me: &User,
) -> Result<(), GitlabError> {
    let assignees = client.get_assignees(project, kind, iid)?;
    if assignees.iter().any(|u| u.id == me.id) {
        return Ok(());
    }
    let mut ids: Vec<u64> = assignees.iter().map(|u| u.id).collect();
    ids.push(me.id);
    client.set_assignees(project, kind, iid, &ids)
}

/// Remove **only the bot** from an item's assignees, keeping every other one — the
/// self-scoped counterpart to [`assign_me`], so releasing never clears an assignment afkd
/// did not make.
fn unassign(
    client: &dyn GitlabClient,
    project: &Project,
    kind: ItemKind,
    iid: u64,
    me: &User,
) -> Result<(), GitlabError> {
    let assignees = client.get_assignees(project, kind, iid)?;
    if !assignees.iter().any(|u| u.id == me.id) {
        return Ok(());
    }
    let ids: Vec<u64> = assignees
        .iter()
        .map(|u| u.id)
        .filter(|id| *id != me.id)
        .collect();
    client.set_assignees(project, kind, iid, &ids)
}

/// What one claim attempt decided.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Claimed {
    /// We hold the unit; the payload is our claim marker's note id (the thing released at
    /// run end and reaped after a crash).
    Won(u64),
    /// A rival's marker out-orders ours. Our marker is gone; the next candidate may be
    /// tried.
    Lost,
}

/// Claim item `iid` (of `kind`) for `me` with a `[afkd-claim]` marker note.
///
/// Post the marker, settle [`CLAIM_SETTLE`] on `clock` so a rival's concurrent marker
/// becomes visible, re-read the thread, and decide with [`won_claim`] over
/// `(created_at, id)`. Nothing concludes "I won" from its own write, and nothing reads the
/// assignee set — which is what stops a **human** assignee from reading as a rival
/// claimant. Every non-winning exit — a failed re-read, a lost race — deletes our marker
/// on the way out, best-effort: a leaked marker ages out after [`CLAIM_LIFETIME`].
pub(crate) fn claim_item(
    client: &dyn GitlabClient,
    project: &Project,
    kind: ItemKind,
    iid: u64,
    me: &User,
    clock: &dyn Clock,
    diag: &dyn Diag,
) -> Result<Claimed, GitlabError> {
    let marker = client.post_comment(project, kind, iid, &claim_text(&me.username))?;
    clock.sleep(CLAIM_SETTLE);
    let notes = match client.list_notes(project, kind, iid) {
        Ok(notes) => notes,
        Err(e) => {
            delete_marker(client, project, kind, iid, marker.id, diag);
            return Err(e);
        }
    };
    if !won_claim(&claim_markers(&notes), marker.id, CLAIM_LIFETIME) {
        delete_marker(client, project, kind, iid, marker.id, diag);
        return Ok(Claimed::Lost);
    }
    Ok(Claimed::Won(marker.id))
}

/// Map an item's notes into the view the claim decision reads: **creation orders**, the
/// **update judges liveness**. The whole list is handed over unfiltered — the marker
/// predicate lives inside the decision.
fn claim_markers(notes: &[Note]) -> Vec<ClaimMarker> {
    notes
        .iter()
        .map(|n| ClaimMarker {
            id: n.id,
            posted_at: n.created_at,
            renewed_at: n.updated_at,
            text: n.body.clone(),
        })
        .collect()
}

/// Delete one claim marker, best-effort: a failure is diagnosed, never propagated (a
/// leaked marker ages out after [`CLAIM_LIFETIME`], and every caller is already on its way
/// out). GitLab's note delete is **item-scoped**, so the `kind`/`iid` ride along.
pub(crate) fn delete_marker(
    client: &dyn GitlabClient,
    project: &Project,
    kind: ItemKind,
    iid: u64,
    marker_id: u64,
    diag: &dyn Diag,
) {
    if let Err(e) = client.delete_comment(project, kind, iid, marker_id) {
        diag.err(&e);
    }
}

/// Renew one claim marker: rewrite it to [`claim_renewal_text`], which moves the note's
/// `updated_at` and so keeps a rival reading it as live for another [`CLAIM_LIFETIME`].
/// Best-effort, like the delete it sits beside: the fire holding the claim must not fail
/// because the forge would not take an edit, and the next renewal is minutes away.
// One argument over the lint, as in the built-in: GitLab's item coordinate is three
// values (`project`/`kind`/`iid`), the spelling every call in this module uses, and the
// renewal adds the marker, the owner it renews as, the counter its body carries, and
// `diag`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn renew_marker(
    client: &dyn GitlabClient,
    project: &Project,
    kind: ItemKind,
    iid: u64,
    marker_id: u64,
    owner: &str,
    renewal: u64,
    diag: &dyn Diag,
) {
    if let Err(e) = client.edit_comment(
        project,
        kind,
        iid,
        marker_id,
        &claim_renewal_text(owner, renewal),
    ) {
        diag.err(&e);
    }
}

/// Release afkd's claim on an item: remove the `afkd::claimed` status label (by name),
/// take **only the bot** back out of the assignees, and delete the claim marker.
/// Best-effort (a partial release is no worse than the leaked claim it undoes).
pub(crate) fn release_claim(
    client: &dyn GitlabClient,
    project: &Project,
    kind: ItemKind,
    iid: u64,
    me: &User,
    marker_id: u64,
    diag: &dyn Diag,
) {
    if let Err(e) = client.remove_label(project, kind, iid, CLAIMED_LABEL) {
        diag.err(&e);
    }
    if let Err(e) = unassign(client, project, kind, iid, me) {
        diag.err(&e);
    }
    delete_marker(client, project, kind, iid, marker_id, diag);
}

/// Release one claim named by a whole journal key (ADR-0059) — the `release` call.
///
/// `None` — the key is not a [`claim_key_for`] shape, so it names nothing releasable and
/// afkd forgets it. `Some(true)` — released (best-effort). There is no `Some(false)` arm
/// here: GitLab addresses a single `project`, so the key's location half is not
/// reconstructed into a call — the release always targets this kind's own project, as the
/// built-in's does.
pub(crate) fn release_stale(
    client: &dyn GitlabClient,
    project: &Project,
    kind: ItemKind,
    key: &str,
    me: &User,
    diag: &dyn Diag,
) -> Option<bool> {
    let (_location, iid, marker_id) = split_claim_key(key)?;
    release_claim(client, project, kind, iid, me, marker_id, diag);
    Some(true)
}

/// A [`Clock`] for tests: `sleep` returns at once, records the wait, and moves `now` on
/// by it; [`advance`](Self::advance) moves `now` without a sleep, standing in for a slow
/// forge round trip.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct FakeClock {
    elapsed: Mutex<Duration>,
    sleeps: Mutex<Vec<Duration>>,
    base: std::sync::OnceLock<Instant>,
}

#[cfg(test)]
impl FakeClock {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Every `sleep` asked for, in order.
    pub(crate) fn sleeps(&self) -> Vec<Duration> {
        lock(&self.sleeps).clone()
    }

    /// Move `now` on by `d`.
    pub(crate) fn advance(&self, d: Duration) {
        *lock(&self.elapsed) += d;
    }
}

#[cfg(test)]
impl Clock for FakeClock {
    fn sleep(&self, d: Duration) {
        lock(&self.sleeps).push(d);
        self.advance(d);
    }

    fn now(&self) -> Instant {
        *self.base.get_or_init(Instant::now) + *lock(&self.elapsed)
    }
}

/// A [`Diag`] for tests: every line, kept.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct CaptureDiag {
    lines: Mutex<Vec<String>>,
}

#[cfg(test)]
impl CaptureDiag {
    pub(crate) fn lines(&self) -> Vec<String> {
        lock(&self.lines).clone()
    }
}

#[cfg(test)]
impl Diag for CaptureDiag {
    fn err(&self, err: &dyn Display) {
        lock(&self.lines).push(err.to_string());
    }
}

#[cfg(test)]
mod tests {
    //! No network: these drive the in-memory `MockClient`.

    use super::*;
    use crate::claim::is_claim;
    use crate::client::{Action, MockClient};

    fn project() -> Project {
        Project::new("group/widgets")
    }

    fn me() -> User {
        User {
            id: 1,
            username: "me".into(),
        }
    }

    /// A contender identity: GitLab's claim identity is the whole user record, and the
    /// marker's owner field is the **username**.
    fn bot(id: u64, username: &str) -> User {
        User {
            id,
            username: username.into(),
        }
    }

    /// The second every same-second race is staged in — a real wall-clock instant,
    /// decades from the epoch the seeded fixtures sit at.
    const T: u64 = 1_700_000_000;

    /// Claim issue `iid` for `owner` against `c` on a fake clock — the shape every claim
    /// test drives.
    fn claim(c: &MockClient, iid: u64, owner: &User) -> Claimed {
        claim_item(
            c,
            &project(),
            ItemKind::Issue,
            iid,
            owner,
            &FakeClock::new(),
            &CaptureDiag::default(),
        )
        .expect("the claim ran")
    }

    /// The `[afkd-claim]` markers left on issue `iid`, as `(id, body)`. Read through the
    /// trait's own list call — the same thing the claim reads — so no test-only accessor
    /// can disagree with production about what is on the thread.
    fn markers_on(c: &MockClient, iid: u64) -> Vec<(u64, String)> {
        c.list_notes(&project(), ItemKind::Issue, iid)
            .expect("read the thread")
            .into_iter()
            .filter(|n| is_claim(&n.body))
            .map(|n| (n.id, n.body))
            .collect()
    }

    /// The note ids deleted, in order.
    fn deleted(c: &MockClient) -> Vec<u64> {
        c.actions()
            .iter()
            .filter_map(|a| match a {
                Action::DeleteComment { id, .. } => Some(*id),
                _ => None,
            })
            .collect()
    }

    /// The id of the surviving marker `owner` posted on `iid` — the winner's, by
    /// construction (a loser deletes its own on the way out).
    fn surviving_marker(c: &MockClient, iid: u64, owner: &str) -> u64 {
        let text = claim_text(owner);
        markers_on(c, iid)
            .into_iter()
            .find(|(_, body)| *body == text)
            .map(|(id, _)| id)
            .unwrap_or_else(|| panic!("no surviving marker for {owner}"))
    }

    /// Two contenders claiming the same issue **in the same second**: exactly one wins
    /// and the loser leaves no marker.
    ///
    /// The post clock is frozen, so both markers carry an identical `created_at` and the
    /// only thing that *can* decide is `won_claim`'s `(created_at, id)` tie-break on the
    /// ids the forge minted. Sequential calls against one mock are how the race is staged
    /// (a unit test has no true concurrency).
    #[test]
    fn two_contenders_in_the_same_second_yield_exactly_one_winner() {
        let c = MockClient::new(1, "bot-a");
        c.add_issue(7, "T", "B", &["afkd::ready"]);
        c.set_clock(T);

        let first = claim(&c, 7, &bot(1, "bot-a"));
        let second = claim(&c, 7, &bot(2, "bot-b"));

        // Exactly one win, and it is the earlier id — the tie-break, not the order of the
        // calls (both markers carry second `T`).
        assert_eq!(first, Claimed::Won(surviving_marker(&c, 7, "bot-a")));
        assert_eq!(second, Claimed::Lost);
        // One marker survives on the thread, and it is the *winner's*.
        assert_eq!(
            markers_on(&c, 7)
                .into_iter()
                .map(|(_, body)| body)
                .collect::<Vec<_>>(),
            vec![claim_text("bot-a")]
        );
        let winner = surviving_marker(&c, 7, "bot-a");
        assert_eq!(deleted(&c).len(), 1, "one delete: {:?}", deleted(&c));
        assert!(
            !deleted(&c).contains(&winner),
            "the loser deleted its own marker, never the winner's"
        );
    }

    /// The N-way shape of the same rule: five contenders in one second leave one winner,
    /// four losses, and one marker.
    #[test]
    fn n_contenders_in_the_same_second_yield_exactly_one_winner() {
        let c = MockClient::new(1, "bot-1");
        c.add_issue(3, "T", "B", &["afkd::ready"]);
        c.set_clock(T);

        // A non-ASCII owner among them: the identity is handed through verbatim, and the
        // decision must not care what it says.
        let owners = ["bot-1", "björn-öst[bot]", "bot-3", "bot-4", "bot-5"];
        let outcomes: Vec<Claimed> = owners
            .iter()
            .enumerate()
            .map(|(i, o)| claim(&c, 3, &bot(i as u64 + 1, o)))
            .collect();

        assert_eq!(
            outcomes.iter().filter(|o| o == &&Claimed::Lost).count(),
            4,
            "{outcomes:?}"
        );
        assert!(
            matches!(outcomes[0], Claimed::Won(_)),
            "the first-posted (smallest id) wins: {outcomes:?}"
        );
        assert_eq!(
            markers_on(&c, 3)
                .into_iter()
                .map(|(_, body)| body)
                .collect::<Vec<_>>(),
            vec![claim_text("bot-1")],
            "one marker left, the winner's"
        );
        assert_eq!(deleted(&c).len(), 4, "each loser deleted its own");
    }

    /// The claim settles exactly [`CLAIM_SETTLE`] between its post and its re-read, on the
    /// clock it was handed.
    #[test]
    fn the_claim_settles_one_claim_settle_on_its_clock() {
        let c = MockClient::new(1, "me");
        c.add_issue(7, "T", "B", &[]);
        let clock = FakeClock::new();
        let won = claim_item(
            &c,
            &project(),
            ItemKind::Issue,
            7,
            &me(),
            &clock,
            &CaptureDiag::default(),
        )
        .unwrap();
        assert!(matches!(won, Claimed::Won(_)));
        assert_eq!(clock.sleeps(), [CLAIM_SETTLE]);
    }

    /// The decision is a pure function of the **re-read**: our own post succeeded and
    /// handed us an id, yet an earlier live rival that landed during the settle takes the
    /// unit. The claim path records only a note — no assignee or label write can be
    /// deciding it.
    #[test]
    fn the_claim_never_concludes_it_won_from_its_own_write() {
        let c = MockClient::new(1, "me");
        c.add_issue(7, "T", "B", &["afkd::ready"]);
        c.set_clock(T);
        // A rival marker that landed between our post and our re-read, one second earlier
        // and so strictly ahead of us in the order.
        c.rival_claims_next(99, "rival", 42, T - 1);

        assert_eq!(claim(&c, 7, &me()), Claimed::Lost);
        assert_eq!(
            markers_on(&c, 7),
            vec![(42, claim_text("rival"))],
            "our marker is gone; the rival's is untouched"
        );
        assert!(
            !c.actions()
                .iter()
                .any(|a| matches!(a, Action::Assign { .. } | Action::Label { .. })),
            "the claim writes no assignee and no label at all: {:?}",
            c.actions()
        );
    }

    /// The marker order is each note's **creation** time: a rival created before ours but
    /// *edited* after it still wins, and the mirror image still loses. Under `updated_at`
    /// both assertions would flip.
    #[test]
    fn an_edited_older_claim_still_orders_first() {
        // Created before ours, edited long after: still first.
        let c = MockClient::new(1, "me");
        c.add_issue(7, "T", "B", &["afkd::ready"]);
        c.set_clock(T);
        c.add_note_edited(
            ItemKind::Issue,
            7,
            42,
            99,
            "rival",
            &claim_text("rival"),
            T - 10,
            T + 9_000,
        );
        assert_eq!(claim(&c, 7, &me()), Claimed::Lost);

        // Created after ours, edited before it: never a threat.
        let c = MockClient::new(1, "me");
        c.add_issue(7, "T", "B", &["afkd::ready"]);
        c.set_clock(T);
        c.add_note_edited(
            ItemKind::Issue,
            7,
            42,
            99,
            "rival",
            &claim_text("rival"),
            T + 10,
            T - 10,
        );
        assert!(matches!(claim(&c, 7, &me()), Claimed::Won(_)));
    }

    /// A failed re-read deletes our marker before propagating: the error is still the
    /// caller's to log, but the item is not left locked for an hour by a claim nobody is
    /// acting on.
    #[test]
    fn a_failed_reread_deletes_our_marker_and_propagates() {
        let c = MockClient::new(1, "me");
        c.add_issue(7, "T", "B", &["afkd::ready"]);
        c.fail("list notes");

        let err = claim_item(
            &c,
            &project(),
            ItemKind::Issue,
            7,
            &me(),
            &FakeClock::new(),
            &CaptureDiag::default(),
        )
        .expect_err("the re-read failure propagates");
        assert_eq!(err.stage(), "list notes");

        c.clear_failure();
        assert!(markers_on(&c, 7).is_empty(), "{:?}", markers_on(&c, 7));
        assert_eq!(deleted(&c).len(), 1);
    }

    /// A marker older than the claim lifetime does not block a new claim, and one exactly
    /// *at* the boundary still does. Liveness is measured against our own post time, so
    /// the fixture is hermetic.
    #[test]
    fn a_marker_past_the_claim_lifetime_does_not_block_a_new_claim() {
        let lifetime = CLAIM_LIFETIME.as_secs();
        for (age, blocks) in [(lifetime + 1, false), (lifetime, true)] {
            let c = MockClient::new(1, "me");
            c.add_issue(7, "T", "B", &["afkd::ready"]);
            c.set_clock(T);
            c.add_note_body(
                ItemKind::Issue,
                7,
                42,
                99,
                "crashed-bot",
                &claim_text("crashed-bot"),
                T - age,
            );

            let outcome = claim(&c, 7, &me());
            assert_eq!(
                matches!(outcome, Claimed::Lost),
                blocks,
                "a marker {age}s old: {outcome:?}"
            );
        }
    }

    /// `assign_me` **unions** the bot into the assignee set rather than replacing it, so a
    /// human already on the item keeps their assignment — and a set that already carries
    /// us is not re-`PUT` at all.
    #[test]
    fn do_action_assign_me_unions_the_bot_in() {
        let c = MockClient::new(1, "me");
        c.add_issue_assigned(7, "T", "B", &[], &[(99, "josefandersson")]);
        do_action(
            &c,
            &project(),
            ItemKind::Issue,
            7,
            &LifecycleAction::AssignMe,
            &me(),
            &Facts::none(),
        )
        .unwrap();
        assert_eq!(c.assignee_ids(ItemKind::Issue, 7), vec![99, 1]);

        let before = c.actions().len();
        do_action(
            &c,
            &project(),
            ItemKind::Issue,
            7,
            &LifecycleAction::AssignMe,
            &me(),
            &Facts::none(),
        )
        .unwrap();
        assert_eq!(
            c.actions().len(),
            before,
            "no redundant write: {:?}",
            c.actions()
        );
    }

    /// `unassign` takes back **only** the bot's own row; the human co-assignee survives —
    /// and a set without the bot is not written at all.
    #[test]
    fn do_action_unassign_removes_only_the_bot() {
        let c = MockClient::new(1, "me");
        c.add_issue_assigned(7, "T", "B", &[], &[(99, "josefandersson"), (1, "me")]);
        do_action(
            &c,
            &project(),
            ItemKind::Issue,
            7,
            &LifecycleAction::Unassign,
            &me(),
            &Facts::none(),
        )
        .unwrap();
        assert_eq!(c.assignee_ids(ItemKind::Issue, 7), vec![99]);
        assert!(c.actions().iter().any(|a| matches!(
            a,
            Action::Assign { ids, .. } if ids == &vec![99]
        )));

        let before = c.actions().len();
        do_action(
            &c,
            &project(),
            ItemKind::Issue,
            7,
            &LifecycleAction::Unassign,
            &me(),
            &Facts::none(),
        )
        .unwrap();
        assert_eq!(c.actions().len(), before, "nothing to take back");
    }

    /// Releasing drops the status label, our own marker, and **only** afkd's own assignee
    /// row: a human's assignment survives untouched, and a note that is not ours is left
    /// on the thread.
    #[test]
    fn release_claim_drops_the_label_and_our_marker_and_only_our_assignee() {
        let c = MockClient::new(1, "me");
        c.add_issue_assigned(
            7,
            "T",
            "B",
            &["afkd::ready", "afkd::claimed"],
            &[(99, "josefandersson"), (1, "me")],
        );
        c.add_note_body(ItemKind::Issue, 7, 42, 1, "me", &claim_text("me"), 100);
        c.add_note_body(
            ItemKind::Issue,
            7,
            43,
            99,
            "josefandersson",
            "any progress?",
            200,
        );

        release_claim(
            &c,
            &project(),
            ItemKind::Issue,
            7,
            &me(),
            42,
            &CaptureDiag::default(),
        );

        assert!(!c.has_label(ItemKind::Issue, 7, CLAIMED_LABEL));
        assert!(
            c.has_label(ItemKind::Issue, 7, "afkd::ready"),
            "the source label is kept"
        );
        assert!(markers_on(&c, 7).is_empty());
        assert_eq!(
            c.assignee_ids(ItemKind::Issue, 7),
            vec![99],
            "only afkd's own assignee row is taken back"
        );
        assert_eq!(
            c.list_notes(&project(), ItemKind::Issue, 7).unwrap().len(),
            1,
            "only the marker was deleted"
        );
    }

    /// A release is best-effort: a failing label removal is diagnosed and the rest of the
    /// release still happens.
    #[test]
    fn a_release_carries_on_past_a_failed_step() {
        let c = MockClient::new(1, "me");
        c.add_issue_assigned(7, "T", "B", &["afkd::claimed"], &[(1, "me")]);
        c.add_note_body(ItemKind::Issue, 7, 42, 1, "me", &claim_text("me"), 100);
        c.fail("remove label");
        let diag = CaptureDiag::default();

        release_claim(&c, &project(), ItemKind::Issue, 7, &me(), 42, &diag);

        assert_eq!(
            diag.lines(),
            ["gitlab remove label: no response (mock failure)"]
        );
        assert!(c.assignee_ids(ItemKind::Issue, 7).is_empty());
        assert!(markers_on(&c, 7).is_empty());
    }

    /// The reaper's key handling over the shapes it can be handed: a well-formed
    /// three-part key releases — unassigning the `me` it is handed and no one else — and
    /// anything that is not a claim key at all names nothing releasable. GitLab is
    /// single-project, so there is no `Some(false)` retry arm.
    #[test]
    fn release_stale_reads_the_key_shapes() {
        let c = MockClient::new(1, "me");
        c.add_issue_assigned(
            7,
            "T",
            "B",
            &["afkd::claimed"],
            &[(99, "josefandersson"), (1, "me")],
        );
        c.add_note_body(ItemKind::Issue, 7, 4242, 1, "me", &claim_text("me"), 100);

        assert_eq!(
            release_stale(
                &c,
                &project(),
                ItemKind::Issue,
                "group/widgets#7#4242",
                &me(),
                &CaptureDiag::default()
            ),
            Some(true)
        );
        assert!(markers_on(&c, 7).is_empty(), "the marker was reaped");
        assert!(!c.has_label(ItemKind::Issue, 7, CLAIMED_LABEL));
        assert_eq!(c.assignee_ids(ItemKind::Issue, 7), vec![99]);

        // The pre-marker two-part key an older binary persisted: not read, not migrated
        // (ADR-0069) — it names nothing releasable, so it is dropped. Neither it nor any
        // other shape writes anything.
        let before = c.actions().len();
        for key in ["group/widgets#7", "garbage", "", "group/widgets#seven#1"] {
            assert_eq!(
                release_stale(
                    &c,
                    &project(),
                    ItemKind::Issue,
                    key,
                    &me(),
                    &CaptureDiag::default()
                ),
                None,
                "{key:?} names nothing releasable"
            );
        }
        assert_eq!(c.actions().len(), before);
    }

    #[test]
    fn do_action_label_remove_removes_by_name() {
        let c = MockClient::new(1, "me");
        c.add_issue(7, "T", "B", &["afkd::claimed"]);
        do_action(
            &c,
            &project(),
            ItemKind::Issue,
            7,
            &LifecycleAction::LabelRemove("afkd::claimed".into()),
            &me(),
            &Facts::none(),
        )
        .unwrap();
        assert!(c.actions().iter().any(|a| matches!(
            a,
            Action::Unlabel { name, .. } if name == "afkd::claimed"
        )));
        assert!(!c.has_label(ItemKind::Issue, 7, "afkd::claimed"));
    }

    #[test]
    fn do_action_close_sets_the_close_state_event() {
        let c = MockClient::new(1, "me");
        c.add_issue(7, "T", "B", &[]);
        do_action(
            &c,
            &project(),
            ItemKind::Issue,
            7,
            &LifecycleAction::Close,
            &me(),
            &Facts::none(),
        )
        .unwrap();
        assert!(c.actions().iter().any(|a| matches!(
            a,
            Action::State { event, .. } if event == "close"
        )));
    }

    #[test]
    fn do_action_comment_posts_the_text() {
        let c = MockClient::new(1, "me");
        c.add_issue(3, "T", "B", &[]);
        do_action(
            &c,
            &project(),
            ItemKind::Issue,
            3,
            &LifecycleAction::Comment("handled by afkd".into()),
            &me(),
            &Facts::none(),
        )
        .unwrap();
        assert!(c.actions().iter().any(|a| matches!(
            a,
            Action::Comment { kind: ItemKind::Issue, body, .. } if body == "handled by afkd"
        )));
    }

    #[test]
    fn apply_actions_stops_at_the_first_failure() {
        // A lifecycle moment's actions run in order; the first failure aborts the rest and
        // is returned, so a half-applied moment is surfaced (not silently finished). Here
        // the label add fails, so the following Close never runs.
        let c = MockClient::new(1, "me");
        c.add_issue(7, "T", "B", &["afkd::ready"]);
        c.fail("add label");
        let err = apply_actions(
            &c,
            &project(),
            ItemKind::Issue,
            7,
            &[
                LifecycleAction::LabelAdd("afkd::claimed".into()),
                LifecycleAction::Close,
            ],
            &me(),
            &Facts::none(),
        )
        .expect_err("the failing label add aborts the moment");
        assert_eq!(err.stage(), "add label");
        assert!(
            !c.actions()
                .iter()
                .any(|a| matches!(a, Action::State { .. })),
            "actions after the first failure are not applied"
        );
    }

    #[test]
    fn do_action_comment_substitutes_run_facts() {
        // A `@{run:…}` comment posts the rendered facts at run end (ADR-0064), the fire's
        // own run directory name among them.
        let c = MockClient::new(1, "me");
        c.add_issue(3, "T", "B", &[]);
        let facts = Facts {
            duration_ms: 5_000,
            cost: 1.5,
            turns: Some(3),
            run_name: Some("260722-141802-issue-3-1".into()),
        };
        do_action(
            &c,
            &project(),
            ItemKind::Issue,
            3,
            &LifecycleAction::Comment(
                "done in @{run:duration} — @{run:cost}, @{run:turns} turns — \
                 log: .afkd/runs/afkd::selfdev/@{run:name}/run.log"
                    .into(),
            ),
            &me(),
            &facts,
        )
        .unwrap();
        assert!(
            c.actions().iter().any(|a| matches!(
                a,
                Action::Comment { body, .. }
                    if body == "done in 5.00s — $1.50, 3 turns — \
                                log: .afkd/runs/afkd::selfdev/260722-141802-issue-3-1/run.log"
            )),
            "{:?}",
            c.actions()
        );
    }

    /// The env and key spellings the skill and the claim journal read, pinned: the
    /// credentials under their `GITLAB_*` names, and the thread and journal key built from
    /// the raw project setting, not its encoded form.
    #[test]
    fn the_env_and_key_spellings_are_the_built_ins() {
        let cfg = GitlabConfig {
            base_url: "https://gitlab.example.com".into(),
            token: "PAT".into(),
            ..GitlabConfig::default()
        };
        assert_eq!(
            creds_env(&cfg),
            BTreeMap::from([
                (
                    "GITLAB_BASE_URL".to_string(),
                    "https://gitlab.example.com".to_string()
                ),
                ("GITLAB_TOKEN".to_string(), "PAT".to_string()),
            ])
        );
        let project = Project::new("acme/sub.group/widgets");
        assert_eq!(unit_key(&project, 7), "acme/sub.group/widgets#7");
        assert_eq!(
            claim_key_for(&project, 7, 90210),
            "acme/sub.group/widgets#7#90210"
        );
        assert_eq!(
            split_claim_key(&claim_key_for(&project, 7, 90210)),
            Some(("acme/sub.group/widgets", 7, 90210))
        );
    }
}
