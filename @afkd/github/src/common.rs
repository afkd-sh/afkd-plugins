//! Drive scaffolding the kind is built from (ADR-0041): the env spellings, the poll's scan
//! budget, the credentials-env builder, the lifecycle-action executor over the
//! [`GithubClient`] seam, the **claim** — a `[afkd-claim]` marker comment decided by
//! [`crate::claim`]'s pure winner rule — and the two seams the rest is written against:
//! the [`Clock`] the claim settles on and the [`Diag`] sink diagnostics go to.
//!
//! Ported from afkd's `crates/github/src/common.rs`. The claim is a marker, not an
//! assignee. GitHub's assignee write is additive, so it cannot double-claim the way a
//! replace-set write can — but a decision read back from it would still be made from a
//! stale read: two instances that both add themselves each see the other as a rival, both
//! release, and nobody takes the issue; and a **human** assignee is indistinguishable from
//! a rival afkd. The marker is written into the issue's append-only comment log, settled,
//! re-read, and won iff a pure function of that shared list names us. The assignee and the
//! `afkd/claimed` label are **visible status**, written after the claim is decided; the
//! label is also the re-pick gate, so the plugin — not a user's `on_claim` list — adds it.
//!
//! GitHub assignment is **additive** (dedicated `add`/`remove` endpoints, not a replace),
//! so `assign_me` adds the bot and `unassign` removes only the bot — neither verb can evict
//! a co-assignee it did not write. The identity is the token's **login**: the assignee
//! verbs, the marker's owner field and the comment author all name it.
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
use crate::client::{GithubClient, GithubError, IssueComment, Repo};
use crate::lifecycle::LifecycleAction;
use crate::run_ref;
use crate::settings::GithubConfig;
use crate::wire::Facts;

/// The in-progress label the claim adds (and `on_fail` removes by name). A fixed internal
/// name, like the `[afkd-claim]` marker.
pub(crate) const CLAIMED_LABEL: &str = "afkd/claimed";

/// Env var carrying the personal access token to the run.
pub(crate) const ENV_TOKEN: &str = "GITHUB_TOKEN";
/// Env var carrying the GitHub host (the raw setting) to the run.
pub(crate) const ENV_HOST: &str = "GITHUB_HOST";
/// Env var carrying the active repository (`owner/name`) to the run.
pub(crate) const ENV_REPO: &str = "GITHUB_REPO";
/// Env var carrying the active issue number to the run.
pub(crate) const ENV_ISSUE_NUMBER: &str = "GITHUB_ISSUE_NUMBER";
/// Env var carrying the active PR number to the run (the PR kind).
pub(crate) const ENV_PR_NUMBER: &str = "GITHUB_PR_NUMBER";
/// Env var carrying the active PR's head branch to the run (the PR kind).
pub(crate) const ENV_PR_BRANCH: &str = "GITHUB_PR_BRANCH";

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
/// stderr, which afkd streams into the service log under `[@afkd/github:err]`.
pub(crate) trait Diag {
    /// Surface one problem.
    fn err(&self, err: &dyn Display);
}

/// The running plugin's [`Diag`]: one `afkd-github: …` line on stderr per problem.
pub(crate) struct StderrDiag;

impl Diag for StderrDiag {
    fn err(&self, err: &dyn Display) {
        eprintln!("afkd-github: {err}");
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
                "github poll: the scan ran past its {}s budget; the rest of it waits for the \
                 next poll",
                POLL_BUDGET.as_secs()
            ));
        }
        spent
    }
}

/// The credentials merged into every run's environment: the token and host (the repo +
/// number are per-unit).
pub(crate) fn creds_env(cfg: &GithubConfig) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert(ENV_TOKEN.to_string(), cfg.token.clone());
    env.insert(ENV_HOST.to_string(), cfg.host.clone());
    env
}

/// The unit's stable identity: `owner/name#number`, **the same across every claim of the
/// same issue** — the agent-session thread (ADR-0067). The claim-journal key carries the
/// claim marker on top of it ([`claim_key_for`]).
pub(crate) fn unit_key(repo: &Repo, number: u64) -> String {
    format!("{}#{number}", repo.full_name())
}

/// The claim-journal key for one claim of a unit (ADR-0059): [`unit_key`]'s two halves
/// plus the claim marker's comment id, so a reaper can delete a crashed run's marker
/// rather than leaving it to block the next claim for an hour.
pub(crate) fn claim_key_for(repo: &Repo, number: u64, marker_id: u64) -> String {
    claim_key(&repo.full_name(), number, marker_id)
}

/// Run a lifecycle moment's actions in order, stopping at the first failure, which is
/// returned for the caller to log. `facts` are what the finishing run did (the pre-run
/// `on_claim` passes [`Facts::none`]); a `comment` interpolates its `@{run:…}` references
/// against them.
pub(crate) fn apply_actions(
    client: &dyn GithubClient,
    repo: &Repo,
    index: u64,
    actions: &[LifecycleAction],
    me: &str,
    facts: &Facts,
) -> Result<(), GithubError> {
    for action in actions {
        do_action(client, repo, index, action, me, facts)?;
    }
    Ok(())
}

/// Carry out one lifecycle action against an issue.
///
/// `LabelRemove` removes the single named label via the dedicated `…/labels/{name}` path
/// (**never** the all-clearing bare `…/labels` path); GitHub needs no name→id lookup.
/// `Unassign` removes only the bot, not an all-clear (GitHub assignment is additive).
pub(crate) fn do_action(
    client: &dyn GithubClient,
    repo: &Repo,
    index: u64,
    action: &LifecycleAction,
    me: &str,
    facts: &Facts,
) -> Result<(), GithubError> {
    match action {
        LifecycleAction::AssignMe => client.add_assignees(repo, index, &[me.to_string()]),
        LifecycleAction::Unassign => client.remove_assignees(repo, index, &[me.to_string()]),
        LifecycleAction::LabelAdd(name) => client.add_label(repo, index, name),
        LifecycleAction::LabelRemove(name) => client.remove_label(repo, index, name),
        LifecycleAction::Close => client.set_state(repo, index, "closed"),
        // Interpolate `@{run:…}` against the finishing run's facts (ADR-0064). The
        // settings reader has already proven every reference legal for this moment.
        LifecycleAction::Comment(text) => client
            .post_comment(repo, index, &run_ref::substitute(text, facts))
            .map(|_| ()),
    }
}

/// What one claim attempt decided.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Claimed {
    /// We hold the unit; the payload is our claim marker's comment id (the thing released
    /// at run end and reaped after a crash).
    Won(u64),
    /// A rival's marker out-orders ours. Our marker is gone; the next candidate may be
    /// tried.
    Lost,
}

/// Claim issue `index` for `me` with a `[afkd-claim]` marker comment.
///
/// Post the marker, settle [`CLAIM_SETTLE`] on `clock` so a rival's concurrent marker
/// becomes visible, re-read the thread, and decide with [`won_claim`] over
/// `(created_at, id)`. Nothing concludes "I won" from its own write, and nothing reads the
/// assignee set — which is what stops a **human** assignee from reading as a rival
/// claimant. Every non-winning exit — a failed re-read, a lost race — deletes our marker
/// on the way out, best-effort: a leaked marker ages out after [`CLAIM_LIFETIME`].
pub(crate) fn claim_issue(
    client: &dyn GithubClient,
    repo: &Repo,
    index: u64,
    me: &str,
    clock: &dyn Clock,
    diag: &dyn Diag,
) -> Result<Claimed, GithubError> {
    let marker = client.post_comment(repo, index, &claim_text(me))?;
    clock.sleep(CLAIM_SETTLE);
    let comments = match client.list_issue_comments(repo, index) {
        Ok(comments) => comments,
        Err(e) => {
            delete_marker(client, repo, marker.id, diag);
            return Err(e);
        }
    };
    if !won_claim(&claim_markers(&comments), marker.id, CLAIM_LIFETIME) {
        delete_marker(client, repo, marker.id, diag);
        return Ok(Claimed::Lost);
    }
    Ok(Claimed::Won(marker.id))
}

/// Map an issue's comments into the view the claim decision reads: **creation orders**,
/// the **update judges liveness**. The whole list is handed over unfiltered — the marker
/// predicate lives inside the decision.
fn claim_markers(comments: &[IssueComment]) -> Vec<ClaimMarker> {
    comments
        .iter()
        .map(|c| ClaimMarker {
            id: c.id,
            posted_at: c.created_at,
            renewed_at: c.updated_at,
            text: c.body.clone(),
        })
        .collect()
}

/// Delete one claim marker, best-effort: a failure is diagnosed, never propagated (a
/// leaked marker ages out after [`CLAIM_LIFETIME`], and every caller is already on its way
/// out). GitHub's comment delete is **repo-scoped**, so no issue number rides along.
pub(crate) fn delete_marker(
    client: &dyn GithubClient,
    repo: &Repo,
    marker_id: u64,
    diag: &dyn Diag,
) {
    if let Err(e) = client.delete_comment(repo, marker_id) {
        diag.err(&e);
    }
}

/// Renew one claim marker: rewrite it to [`claim_renewal_text`], which moves the comment's
/// `updated_at` and so keeps a rival reading it as live for another [`CLAIM_LIFETIME`].
/// Best-effort, like the delete it sits beside: the fire holding the claim must not fail
/// because the forge would not take an edit, and the next renewal is minutes away.
pub(crate) fn renew_marker(
    client: &dyn GithubClient,
    repo: &Repo,
    marker_id: u64,
    owner: &str,
    renewal: u64,
    diag: &dyn Diag,
) {
    if let Err(e) = client.edit_comment(repo, marker_id, &claim_renewal_text(owner, renewal)) {
        diag.err(&e);
    }
}

/// Release afkd's claim on an issue: remove the `afkd/claimed` status label (by name —
/// never the all-clearing bare path), remove **only the bot** from the assignees, and
/// delete the claim marker. Best-effort (a partial release is no worse than the leaked
/// claim it undoes). GitHub's `remove_assignees` is natively self-scoped, so a status write
/// afkd made is undone while a person's assignment survives.
pub(crate) fn release_claim(
    client: &dyn GithubClient,
    repo: &Repo,
    index: u64,
    me: &str,
    marker_id: u64,
    diag: &dyn Diag,
) {
    if let Err(e) = client.remove_label(repo, index, CLAIMED_LABEL) {
        diag.err(&e);
    }
    if let Err(e) = client.remove_assignees(repo, index, &[me.to_string()]) {
        diag.err(&e);
    }
    delete_marker(client, repo, marker_id, diag);
}

/// Release one claim named by a whole journal key (ADR-0059) — the `release` call.
///
/// `None` — the key is not a [`claim_key_for`] shape, so it names nothing releasable and
/// afkd forgets it. `Some(false)` — the location half has no `owner/name` shape, so the
/// entry is left to retry. `Some(true)` — released (best-effort). As in the built-in, the
/// release addresses the repository the key names.
pub(crate) fn release_stale(
    client: &dyn GithubClient,
    key: &str,
    me: &str,
    diag: &dyn Diag,
) -> Option<bool> {
    let (location, number, marker_id) = split_claim_key(key)?;
    // A location half with no `owner/name` shape cannot be reconstructed into a call, but
    // the key is well-formed — leave it to retry rather than dropping it.
    let Some(repo) = Repo::parse(location) else {
        return Some(false);
    };
    release_claim(client, &repo, number, me, marker_id, diag);
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

    fn repo() -> Repo {
        Repo {
            owner: "acme".into(),
            name: "widgets".into(),
        }
    }

    /// The second every same-second race is staged in — a real wall-clock instant,
    /// decades from the epoch the seeded fixtures sit at.
    const T: u64 = 1_700_000_000;

    /// Claim issue `index` for `owner` against `c` on a fake clock — the shape every claim
    /// test drives.
    fn claim(c: &MockClient, index: u64, owner: &str) -> Claimed {
        claim_issue(
            c,
            &repo(),
            index,
            owner,
            &FakeClock::new(),
            &CaptureDiag::default(),
        )
        .expect("the claim ran")
    }

    /// The `[afkd-claim]` markers left on issue `index`, as `(id, body)`. Read through the
    /// trait's own list call — the same thing the claim reads — so no test-only accessor
    /// can disagree with production about what is on the thread.
    fn markers_on(c: &MockClient, index: u64) -> Vec<(u64, String)> {
        c.list_issue_comments(&repo(), index)
            .expect("read the thread")
            .into_iter()
            .filter(|c| is_claim(&c.body))
            .map(|c| (c.id, c.body))
            .collect()
    }

    /// The comment ids deleted, in order.
    fn deleted(c: &MockClient) -> Vec<u64> {
        c.actions()
            .iter()
            .filter_map(|a| match a {
                Action::DeleteComment { id } => Some(*id),
                _ => None,
            })
            .collect()
    }

    /// The id of the surviving marker `owner` posted on `index` — the winner's, by
    /// construction (a loser deletes its own on the way out).
    fn surviving_marker(c: &MockClient, index: u64, owner: &str) -> u64 {
        let text = claim_text(owner);
        markers_on(c, index)
            .into_iter()
            .find(|(_, body)| *body == text)
            .map(|(id, _)| id)
            .unwrap_or_else(|| panic!("no surviving marker for {owner}"))
    }

    /// Two contenders claiming the same issue **in the same second**: exactly one wins and
    /// the loser leaves no marker.
    ///
    /// The post clock is frozen, so both markers carry an identical `created_at` and the
    /// only thing that *can* decide is `won_claim`'s `(created_at, id)` tie-break on the
    /// ids the forge minted. Sequential calls against one mock are how the race is staged
    /// (a unit test has no true concurrency).
    #[test]
    fn two_contenders_in_the_same_second_yield_exactly_one_winner() {
        let c = MockClient::new("bot-a");
        c.add_issue(7, "T", "B", &["afkd/ready"]);
        c.set_clock(T);

        let first = claim(&c, 7, "bot-a");
        let second = claim(&c, 7, "bot-b");

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
        let c = MockClient::new("bot-1");
        c.add_issue(7, "T", "B", &["afkd/ready"]);
        c.set_clock(T);

        // A non-ASCII owner among them: the identity is handed through verbatim, and the
        // decision must not care what it says.
        let owners = ["bot-1", "björn-öst[bot]", "bot-3", "bot-4", "bot-5"];
        let outcomes: Vec<Claimed> = owners.iter().map(|o| claim(&c, 7, o)).collect();

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
            markers_on(&c, 7)
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
        let c = MockClient::new("me");
        c.add_issue(7, "T", "B", &[]);
        let clock = FakeClock::new();
        let won = claim_issue(&c, &repo(), 7, "me", &clock, &CaptureDiag::default()).unwrap();
        assert!(matches!(won, Claimed::Won(_)));
        assert_eq!(clock.sleeps(), [CLAIM_SETTLE]);
    }

    /// The decision is a pure function of the **re-read**: our own post succeeded and
    /// handed us an id, yet an earlier live rival that landed during the settle takes the
    /// unit. The claim path records only a comment — no assignee or label write can be
    /// deciding it.
    #[test]
    fn the_claim_never_concludes_it_won_from_its_own_write() {
        let c = MockClient::new("me");
        c.add_issue(7, "T", "B", &["afkd/ready"]);
        c.set_clock(T);
        // A rival marker that landed between our post and our re-read, one second earlier
        // and so strictly ahead of us in the order.
        c.rival_claims_next("rival", 42, T - 1);

        assert_eq!(claim(&c, 7, "me"), Claimed::Lost);
        assert_eq!(
            markers_on(&c, 7),
            vec![(42, claim_text("rival"))],
            "our marker is gone; the rival's is untouched"
        );
        assert!(
            !c.actions().iter().any(|a| matches!(
                a,
                Action::AddAssignees { .. } | Action::RemoveAssignees { .. } | Action::Label { .. }
            )),
            "the claim writes no assignee and no label at all: {:?}",
            c.actions()
        );
    }

    /// The marker order is each comment's **creation** time: a rival created before ours
    /// but *edited* after it still wins, and the mirror image still loses. Under
    /// `updated_at` both assertions would flip.
    #[test]
    fn an_edited_older_claim_still_orders_first() {
        // Created before ours, edited long after: still first.
        let c = MockClient::new("me");
        c.add_issue(7, "T", "B", &["afkd/ready"]);
        c.set_clock(T);
        c.add_comment_edited(7, 42, "rival", &claim_text("rival"), T - 10, T + 9_000);
        assert_eq!(claim(&c, 7, "me"), Claimed::Lost);

        // Created after ours, edited before it: never a threat.
        let c = MockClient::new("me");
        c.add_issue(7, "T", "B", &["afkd/ready"]);
        c.set_clock(T);
        c.add_comment_edited(7, 42, "rival", &claim_text("rival"), T + 10, T - 10);
        assert!(matches!(claim(&c, 7, "me"), Claimed::Won(_)));
    }

    /// A failed re-read deletes our marker before propagating: the error is still the
    /// caller's to log, but the issue is not left locked for an hour by a claim nobody is
    /// acting on.
    #[test]
    fn a_failed_reread_deletes_our_marker_and_propagates() {
        let c = MockClient::new("me");
        c.add_issue(7, "T", "B", &["afkd/ready"]);
        c.fail("list comments");

        let err = claim_issue(
            &c,
            &repo(),
            7,
            "me",
            &FakeClock::new(),
            &CaptureDiag::default(),
        )
        .expect_err("the re-read failure propagates");
        assert_eq!(err.stage(), "list comments");

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
            let c = MockClient::new("me");
            c.add_issue(7, "T", "B", &["afkd/ready"]);
            c.set_clock(T);
            c.add_comment_body(7, 42, "crashed-bot", &claim_text("crashed-bot"), T - age);

            let outcome = claim(&c, 7, "me");
            assert_eq!(
                matches!(outcome, Claimed::Lost),
                blocks,
                "a marker {age}s old: {outcome:?}"
            );
        }
    }

    /// Liveness is the marker's **last edit**: one created hours ago but renewed a minute
    /// ago — the built-in's renewal text — still blocks; the same marker unrenewed past
    /// the lifetime does not.
    #[test]
    fn a_renewed_marker_is_live_however_old_its_creation() {
        let lifetime = CLAIM_LIFETIME.as_secs();
        for (renewed_ago, blocks) in [(60, true), (lifetime + 1, false)] {
            let c = MockClient::new("me");
            c.add_issue(7, "T", "B", &["afkd/ready"]);
            c.set_clock(T);
            c.add_comment_edited(
                7,
                42,
                "björn-öst[bot]",
                &claim_renewal_text("björn-öst[bot]", 40),
                T - 4 * lifetime,
                T - renewed_ago,
            );
            let outcome = claim(&c, 7, "me");
            assert_eq!(
                matches!(outcome, Claimed::Lost),
                blocks,
                "renewed {renewed_ago}s ago: {outcome:?}"
            );
        }
    }

    /// Releasing drops the label, our own marker, and **only** afkd's own assignee row: a
    /// human's assignment survives untouched (GitHub's remove is self-scoped), and a
    /// comment that is not ours is left on the thread.
    #[test]
    fn release_claim_drops_the_label_and_our_marker_and_only_our_assignee() {
        let c = MockClient::new("me");
        c.add_issue_assigned(
            7,
            "T",
            "B",
            &["afkd/ready", "afkd/claimed"],
            &["陳大文", "me"],
        );
        c.add_comment_body(7, 42, "me", &claim_text("me"), 100);
        c.add_comment_body(7, 43, "陳大文", "any progress?", 200);

        release_claim(&c, &repo(), 7, "me", 42, &CaptureDiag::default());

        assert!(!c.has_label(7, CLAIMED_LABEL));
        assert!(c.has_label(7, "afkd/ready"), "the source label is kept");
        assert!(markers_on(&c, 7).is_empty());
        assert_eq!(
            c.assignees_of(7),
            vec!["陳大文".to_string()],
            "only afkd's own assignee row is taken back"
        );
        assert_eq!(
            c.list_issue_comments(&repo(), 7).unwrap().len(),
            1,
            "only the marker was deleted"
        );
    }

    /// A release is best-effort: a failing label removal is diagnosed and the rest of the
    /// release still happens.
    #[test]
    fn a_release_carries_on_past_a_failed_step() {
        let c = MockClient::new("me");
        c.add_issue_assigned(7, "T", "B", &["afkd/claimed"], &["me"]);
        c.add_comment_body(7, 42, "me", &claim_text("me"), 100);
        c.fail("remove label");
        let diag = CaptureDiag::default();

        release_claim(&c, &repo(), 7, "me", 42, &diag);

        assert_eq!(
            diag.lines(),
            ["github remove label: no response (mock failure)"]
        );
        assert!(c.assignees_of(7).is_empty());
        assert!(markers_on(&c, 7).is_empty());
    }

    /// The reaper's key handling over the three shapes it can be handed: a well-formed key
    /// releases — unassigning the `me` it is handed and no one else — an unparseable
    /// location half is left to retry, and a key that is not a claim key at all names
    /// nothing releasable. Neither of the last two writes anything.
    #[test]
    fn release_stale_reads_the_three_key_shapes() {
        let c = MockClient::new("me");
        c.add_issue_assigned(7, "T", "B", &["afkd/claimed"], &["陳大文", "me"]);
        c.add_comment_body(7, 4242, "me", &claim_text("me"), 100);
        let diag = CaptureDiag::default();

        assert_eq!(
            release_stale(&c, "acme/widgets#7#4242", "me", &diag),
            Some(true)
        );
        assert!(markers_on(&c, 7).is_empty(), "the marker was reaped");
        assert!(!c.has_label(7, CLAIMED_LABEL));
        assert_eq!(c.assignees_of(7), vec!["陳大文".to_string()]);

        let before = c.actions().len();
        // A location half with no `owner/name` shape: retried, not dropped.
        for key in ["not-a-repo#7#4242", "acme/sub/widgets#7#4242"] {
            assert_eq!(release_stale(&c, key, "me", &diag), Some(false), "{key:?}");
        }
        // The pre-marker two-part key an older binary persisted: not read, not migrated
        // (ADR-0069) — it names nothing releasable, so it is dropped.
        for key in ["acme/widgets#7", "garbage", "", "acme/widgets#seven#1"] {
            assert_eq!(release_stale(&c, key, "me", &diag), None, "{key:?}");
        }
        assert_eq!(
            c.actions().len(),
            before,
            "no write for a key it cannot use"
        );
        assert!(diag.lines().is_empty(), "{:?}", diag.lines());
    }

    #[test]
    fn do_action_assign_me_adds_only_the_bot() {
        let c = MockClient::new("me");
        c.add_issue_assigned(7, "T", "B", &[], &["陳大文"]);
        do_action(
            &c,
            &repo(),
            7,
            &LifecycleAction::AssignMe,
            "me",
            &Facts::none(),
        )
        .unwrap();
        assert_eq!(
            c.actions(),
            [Action::AddAssignees {
                index: 7,
                assignees: vec!["me".into()],
            }]
        );
        assert_eq!(
            c.assignees_of(7),
            vec!["陳大文".to_string(), "me".to_string()]
        );
    }

    #[test]
    fn do_action_label_remove_removes_by_name() {
        let c = MockClient::new("me");
        c.add_issue(7, "T", "B", &["afkd/claimed"]);
        do_action(
            &c,
            &repo(),
            7,
            &LifecycleAction::LabelRemove("afkd/claimed".into()),
            "me",
            &Facts::none(),
        )
        .unwrap();
        // The recorded action names the label, never the all-clearing bare path.
        assert!(c
            .actions()
            .iter()
            .any(|a| matches!(a, Action::Unlabel { name, .. } if name == "afkd/claimed")));
        assert!(!c.has_label(7, "afkd/claimed"));
    }

    #[test]
    fn do_action_unassign_removes_only_the_bot() {
        let c = MockClient::new("me");
        c.add_issue_assigned(7, "T", "B", &[], &["me", "human"]);
        do_action(
            &c,
            &repo(),
            7,
            &LifecycleAction::Unassign,
            "me",
            &Facts::none(),
        )
        .unwrap();
        // Only the bot is removed; a human co-assignee is left untouched.
        assert_eq!(c.assignees_of(7), vec!["human".to_string()]);
    }

    #[test]
    fn do_action_close_sets_state_closed() {
        let c = MockClient::new("me");
        c.add_issue(7, "T", "B", &[]);
        do_action(
            &c,
            &repo(),
            7,
            &LifecycleAction::Close,
            "me",
            &Facts::none(),
        )
        .unwrap();
        assert!(c
            .actions()
            .iter()
            .any(|a| matches!(a, Action::State { state, .. } if state == "closed")));
    }

    #[test]
    fn do_action_comment_posts_the_text() {
        let c = MockClient::new("me");
        c.add_issue(7, "T", "B", &[]);
        do_action(
            &c,
            &repo(),
            7,
            &LifecycleAction::Comment("handled by afkd".into()),
            "me",
            &Facts::none(),
        )
        .unwrap();
        assert!(c
            .actions()
            .iter()
            .any(|a| matches!(a, Action::Comment { body, .. } if body == "handled by afkd")));
    }

    #[test]
    fn apply_actions_stops_at_the_first_failure() {
        // A lifecycle moment's actions run in order; the first failure aborts the rest and
        // is returned, so a half-applied moment is surfaced (not silently finished). Here
        // the label add fails, so the following Close never runs.
        let c = MockClient::new("me");
        c.add_issue(7, "T", "B", &["afkd/ready"]);
        c.fail("add label");
        let err = apply_actions(
            &c,
            &repo(),
            7,
            &[
                LifecycleAction::LabelAdd("afkd/claimed".into()),
                LifecycleAction::Close,
            ],
            "me",
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
        let c = MockClient::new("me");
        c.add_issue(7, "T", "B", &[]);
        let facts = Facts {
            duration_ms: 5_000,
            cost: 1.5,
            turns: Some(3),
            run_name: Some("260722-141802-issue-7-1".into()),
        };
        do_action(
            &c,
            &repo(),
            7,
            &LifecycleAction::Comment(
                "done in @{run:duration} — @{run:cost}, @{run:turns} turns — \
                 log: .afkd/runs/afkd::selfdev/@{run:name}/run.log"
                    .into(),
            ),
            "me",
            &facts,
        )
        .unwrap();
        assert!(
            c.actions().iter().any(|a| matches!(
                a,
                Action::Comment { body, .. }
                    if body == "done in 5.00s — $1.50, 3 turns — \
                                log: .afkd/runs/afkd::selfdev/260722-141802-issue-7-1/run.log"
            )),
            "{:?}",
            c.actions()
        );
    }

    /// The env and key spellings the skill and the claim journal read, pinned: the
    /// credentials under their `GITHUB_*` names (the host as written, not resolved), and
    /// the thread and journal key built from `owner/name`.
    #[test]
    fn the_env_and_key_spellings_are_the_built_ins() {
        let cfg = GithubConfig {
            host: "ghe.example.com".into(),
            token: "PAT".into(),
            ..GithubConfig::default()
        };
        assert_eq!(
            creds_env(&cfg),
            BTreeMap::from([
                ("GITHUB_HOST".to_string(), "ghe.example.com".to_string()),
                ("GITHUB_TOKEN".to_string(), "PAT".to_string()),
            ])
        );
        assert_eq!(
            (ENV_REPO, ENV_ISSUE_NUMBER, CLAIMED_LABEL),
            ("GITHUB_REPO", "GITHUB_ISSUE_NUMBER", "afkd/claimed")
        );
        assert_eq!(
            (ENV_PR_NUMBER, ENV_PR_BRANCH),
            ("GITHUB_PR_NUMBER", "GITHUB_PR_BRANCH")
        );
        let repo = repo();
        assert_eq!(unit_key(&repo, 7), "acme/widgets#7");
        assert_eq!(claim_key_for(&repo, 7, 90210), "acme/widgets#7#90210");
        assert_eq!(
            split_claim_key(&claim_key_for(&repo, 7, 90210)),
            Some(("acme/widgets", 7, 90210))
        );
    }
}
