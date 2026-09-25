//! Drive scaffolding the issue kind is built from (ADR-0031): the env spellings, the
//! credentials-env builder, the lifecycle-action executor over the [`GiteaClient`] seam,
//! the **claim** — a `[afkd-claim]` marker comment decided by [`crate::claim`]'s pure
//! winner rule — and the three seams the rest is written against: the [`Clock`] the
//! claim settles on, the [`Diag`] sink diagnostics go to, and the [`ClaimFault`] verdict.
//!
//! Ported from afkd's `crates/gitea/src/common.rs`. The claim is a marker, not an
//! assignee: Gitea's assignee write is a *replace set*, so an assign-and-re-read claim
//! decides from its own write and two instances can both conclude they won. The marker is
//! written into the issue's append-only comment log, settled, re-read, and won iff a pure
//! function of that shared list names us. The assignee and the `afkd/claimed` label are
//! **visible status**, written after the claim is decided; the label is also the re-pick
//! gate, so the plugin — not a user's `on_claim` list — adds it.
//!
//! One thing the built-in has and this does not: afkd's stop. The plugin cannot see it,
//! so a claim never abandons mid-settle; afkd hands a unit polled during a stop straight
//! back with `release`, which is the same end state.

use std::collections::BTreeMap;
use std::fmt::Display;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

use crate::claim::{
    claim_key, claim_renewal_text, claim_text, is_claim, split_claim_key, won_claim, ClaimMarker,
    CLAIM_LIFETIME, CLAIM_SETTLE,
};
use crate::client::{GiteaClient, GiteaError, IssueComment, Label, Repo};
use crate::feedback::FeedbackItem;
use crate::lifecycle::LifecycleAction;
use crate::run_ref;
use crate::settings::GiteaConfig;
use crate::wire::Facts;

/// The in-progress label the claim adds (and `on_fail` removes by id). A fixed internal
/// name, like the `[afkd-claim]` marker. It is also the **re-pick gate** the kind's
/// `eligible` reads, so the plugin creates it in a repository that does not define it
/// ([`ensure_labels`]) — Gitea would otherwise drop the write and re-claim the unit on
/// every poll, forever.
pub(crate) const CLAIMED_LABEL: &str = "afkd/claimed";

/// The label added when a run **parks** an issue awaiting human input, and removed again
/// on the next claim. Managed by the plugin itself so the awaiting state stays
/// authoritative regardless of any user `on_park` actions.
pub(crate) const AWAITING_LABEL: &str = "afkd/awaiting-reply";

/// The colour every afkd-managed label is created with (`CreateLabelOption` requires a
/// `color` beside the name).
pub(crate) const AFKD_LABEL_COLOR: &str = "#7057ff";

/// Env var carrying the personal access token to the run.
pub(crate) const ENV_TOKEN: &str = "GITEA_TOKEN";
/// Env var carrying the instance base URL to the run.
pub(crate) const ENV_BASE_URL: &str = "GITEA_BASE_URL";
/// Env var carrying the active repository (`owner/name`) to the run.
pub(crate) const ENV_REPO: &str = "GITEA_REPO";
/// Env var carrying the active issue number to the run.
pub(crate) const ENV_ISSUE_NUMBER: &str = "GITEA_ISSUE_NUMBER";

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
/// stderr, which afkd streams into the service log under `[@afkd/gitea:err]`.
pub(crate) trait Diag {
    /// Surface one problem.
    fn err(&self, err: &dyn Display);
}

/// The running plugin's [`Diag`]: one `afkd-gitea: …` line on stderr per problem.
pub(crate) struct StderrDiag;

impl Diag for StderrDiag {
    fn err(&self, err: &dyn Display) {
        eprintln!("afkd-gitea: {err}");
    }
}

/// Why a claim attempt did not produce a unit: the built-in's fatal/transient verdict.
///
/// A **definite** verdict from the forge — it answered, and the answer says this kind can
/// never claim anything — is [`Fatal`](Self::Fatal) and takes the service down with that
/// sentence. Anything **transient** — a timeout, a 5xx, a connection reset — is
/// [`Transient`](Self::Transient), logged and retried on the next beat, because a flaky
/// network must never take a service down. The blanket `From` makes `?` classify as
/// `Transient`, the right default.
#[derive(Debug)]
pub(crate) enum ClaimFault<E> {
    /// A retryable failure: diagnosed, retried next beat.
    Transient(E),
    /// An unrecoverable condition, as the operator-facing sentence.
    Fatal(String),
}

impl<E> From<E> for ClaimFault<E> {
    fn from(e: E) -> Self {
        Self::Transient(e)
    }
}

/// The credentials merged into every run's environment: the token and base URL (the
/// repo + number are per-unit).
pub(crate) fn creds_env(cfg: &GiteaConfig) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert(ENV_TOKEN.to_string(), cfg.token.clone());
    env.insert(ENV_BASE_URL.to_string(), cfg.base_url.clone());
    env
}

/// The unit's stable identity: `owner/name#number`, unique across an org's repos and
/// **the same across every claim of the same issue** — the agent-session thread
/// (ADR-0067). The claim-journal key carries the claim marker on top of it
/// ([`claim_key_for`]).
pub(crate) fn unit_key(repo: &Repo, number: u64) -> String {
    format!("{}#{number}", repo.full_name())
}

/// The claim-journal key for one claim of a unit (ADR-0059): [`unit_key`]'s two halves
/// plus the claim marker's comment id, so a reaper can delete a crashed run's marker
/// rather than leaving it to block the next claim for an hour.
pub(crate) fn claim_key_for(repo: &Repo, number: u64, marker_id: u64) -> String {
    claim_key(&repo.full_name(), number, marker_id)
}

/// The repository's definition of label `name`, if it has one — the whole [`Label`]:
/// the id a by-id removal must name, and the `exclusive` flag [`ensure_labels`] refuses
/// to build a gate on.
pub(crate) fn label_named(
    client: &dyn GiteaClient,
    repo: &Repo,
    name: &str,
) -> Result<Option<Label>, GiteaError> {
    Ok(client
        .list_labels(repo)?
        .into_iter()
        .find(|l| l.name == name))
}

/// Whether a failure is a **definite** verdict — the forge answered, and the answer will
/// not change on the next beat — as opposed to a blip worth retrying. A
/// [`Refused`](GiteaError::Refused) and a **4xx** are definite; a 5xx, a transport
/// failure and an undecodable body are transient.
pub(crate) fn definite(e: &GiteaError) -> bool {
    match e {
        GiteaError::Refused { .. } => true,
        GiteaError::Status { status, .. } => (400..500).contains(status),
        GiteaError::Transport { .. } | GiteaError::Decode { .. } => false,
    }
}

/// Make sure `repo` defines the afkd-managed labels `names`, creating any it lacks as
/// **plain, non-exclusive** labels.
///
/// Gitea *silently drops* a label name the repository does not define, answering success
/// either way: without the ensure, a repo that never created `afkd/claimed` never gets
/// the re-pick gate set, so every poll re-claims the same issue. Non-exclusive is the
/// rule: an **exclusive** label is removed the moment another exclusive label in its scope
/// is added, so an already-defined exclusive `afkd/claimed` is refused outright as
/// [`Fatal`](ClaimFault::Fatal) — as is a definite refusal of the create. Anything
/// transient stays a retried poll error.
fn ensure_labels(
    client: &dyn GiteaClient,
    repo: &Repo,
    names: &[&str],
) -> Result<(), ClaimFault<GiteaError>> {
    let defined = client.list_labels(repo)?;
    for name in names {
        match defined.iter().find(|l| l.name == *name) {
            Some(label) if label.exclusive => {
                return Err(ClaimFault::Fatal(format!(
                    "label “{name}” on {} is an exclusive scoped label, and afkd's own \
                     labels must not be: gitea strips an exclusive label as soon as another \
                     label in its scope is added, so the claim gate would vanish and the \
                     unit would be claimed again on every poll — recreate “{name}” without \
                     the exclusive flag",
                    repo.full_name()
                )));
            }
            Some(_) => {}
            None => {
                if let Err(e) = client.create_label(repo, name, AFKD_LABEL_COLOR, false) {
                    return Err(if definite(&e) {
                        ClaimFault::Fatal(format!(
                            "label “{name}” cannot be created on {}: {e}",
                            repo.full_name()
                        ))
                    } else {
                        ClaimFault::Transient(e)
                    });
                }
            }
        }
    }
    Ok(())
}

/// [`ensure_labels`] for the issue kind: both labels it manages — the claim gate and the
/// park's awaiting-reply marker.
pub(crate) fn ensure_issue_labels(
    client: &dyn GiteaClient,
    repo: &Repo,
) -> Result<(), ClaimFault<GiteaError>> {
    ensure_labels(client, repo, &[CLAIMED_LABEL, AWAITING_LABEL])
}

/// Classify a failed **claim status-label** write. That label is the re-pick gate, so a
/// *definite* refusal fails the service with a sentence naming the repo, the unit and the
/// label; a transient failure is the swallowed, retried poll error.
pub(crate) fn claim_label_fault(
    repo: &Repo,
    index: u64,
    name: &str,
    e: GiteaError,
) -> ClaimFault<GiteaError> {
    if definite(&e) {
        ClaimFault::Fatal(format!(
            "label “{name}” cannot be applied to {}#{index}: {e} — without it the claim has \
             no re-pick gate, so the unit would be claimed again on every poll",
            repo.full_name()
        ))
    } else {
        ClaimFault::Transient(e)
    }
}

/// Run a lifecycle moment's actions in order, stopping at the first failure, which is
/// returned for the caller to log. `facts` are what the finishing run did (the pre-run
/// `on_claim` passes [`Facts::none`]); a `comment` interpolates its `@{run:…}`
/// references against them.
pub(crate) fn apply_actions(
    client: &dyn GiteaClient,
    repo: &Repo,
    index: u64,
    actions: &[LifecycleAction],
    me: &str,
    facts: &Facts,
) -> Result<(), GiteaError> {
    for action in actions {
        do_action(client, repo, index, action, me, facts)?;
    }
    Ok(())
}

/// Carry out one lifecycle action against an issue.
///
/// `LabelRemove` resolves the name to its id and removes by id (**never** the
/// all-clearing bare `…/labels` path); a name that resolves to no id is a no-op.
/// `LabelAdd` is **not** forgiving of an undefined name: Gitea drops it under a success
/// status and the client turns that into an error.
pub(crate) fn do_action(
    client: &dyn GiteaClient,
    repo: &Repo,
    index: u64,
    action: &LifecycleAction,
    me: &str,
    facts: &Facts,
) -> Result<(), GiteaError> {
    match action {
        // Both assignee verbs are self-scoped: Gitea has only a replace-set `PATCH`, so
        // they read-modify-write rather than evict (or clear) a human's assignment.
        LifecycleAction::AssignMe => assign_me(client, repo, index, me),
        LifecycleAction::Unassign => unassign_me(client, repo, index, me),
        LifecycleAction::LabelAdd(name) => client.add_label(repo, index, name),
        LifecycleAction::LabelRemove(name) => match label_named(client, repo, name)? {
            Some(label) => client.remove_label(repo, index, &label.id.to_string()),
            None => Ok(()),
        },
        LifecycleAction::Close => client.set_state(repo, index, "closed"),
        LifecycleAction::Comment(text) => client
            .post_comment(repo, index, &run_ref::substitute(text, facts))
            .map(|_| ()),
    }
}

/// Add `me` to issue `index`'s assignees, leaving every other assignee alone.
fn assign_me(
    client: &dyn GiteaClient,
    repo: &Repo,
    index: u64,
    me: &str,
) -> Result<(), GiteaError> {
    let issue = client.get_issue(repo, index)?;
    if issue.assigned_to(me) {
        return Ok(());
    }
    let mut assignees: Vec<String> = issue.assignees.iter().map(|u| u.login.clone()).collect();
    assignees.push(me.to_string());
    client.patch_assignees(repo, index, &assignees).map(|_| ())
}

/// Remove `me` from issue `index`'s assignees, leaving every human assignee in place.
pub(crate) fn unassign_me(
    client: &dyn GiteaClient,
    repo: &Repo,
    index: u64,
    me: &str,
) -> Result<(), GiteaError> {
    let issue = client.get_issue(repo, index)?;
    if !issue.assigned_to(me) {
        return Ok(());
    }
    let assignees: Vec<String> = issue
        .assignees
        .iter()
        .filter(|u| u.login != me)
        .map(|u| u.login.clone())
        .collect();
    client.patch_assignees(repo, index, &assignees).map(|_| ())
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
/// `(created_at, id)`. Nothing concludes "I won" from its own write. Every non-winning
/// exit — a failed re-read, a lost race — deletes our marker on the way out,
/// best-effort: a leaked marker ages out after [`CLAIM_LIFETIME`].
pub(crate) fn claim_issue(
    client: &dyn GiteaClient,
    repo: &Repo,
    index: u64,
    me: &str,
    clock: &dyn Clock,
    diag: &dyn Diag,
) -> Result<Claimed, GiteaError> {
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

/// Delete one claim marker, best-effort: a failure is diagnosed, never propagated.
pub(crate) fn delete_marker(
    client: &dyn GiteaClient,
    repo: &Repo,
    marker_id: u64,
    diag: &dyn Diag,
) {
    if let Err(e) = client.delete_comment(repo, marker_id) {
        diag.err(&e);
    }
}

/// Renew one claim marker: rewrite it to [`claim_renewal_text`], which moves the
/// comment's `updated_at` and so keeps a rival reading it as live. Best-effort.
pub(crate) fn renew_marker(
    client: &dyn GiteaClient,
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

/// Release afkd's claim on an issue: remove the `afkd/claimed` status label and delete
/// the claim marker. Best-effort. **Assignees are not touched** — the claim never wrote
/// the whole set, so releasing must not clear a human's assignment.
pub(crate) fn release_claim(
    client: &dyn GiteaClient,
    repo: &Repo,
    index: u64,
    marker_id: u64,
    diag: &dyn Diag,
) {
    match label_named(client, repo, CLAIMED_LABEL) {
        Ok(Some(label)) => {
            if let Err(e) = client.remove_label(repo, index, &label.id.to_string()) {
                diag.err(&e);
            }
        }
        Ok(None) => {}
        Err(e) => diag.err(&e),
    }
    delete_marker(client, repo, marker_id, diag);
}

/// Release one stale claim named by a whole journal key (ADR-0059) — the `release` call.
///
/// `None` — the key is not a [`claim_key_for`] shape, so it names nothing releasable and
/// afkd forgets it. `Some(false)` — the location half has no `owner/name` shape, so the
/// entry is left to retry. `Some(true)` — released (best-effort).
pub(crate) fn release_stale(client: &dyn GiteaClient, key: &str, diag: &dyn Diag) -> Option<bool> {
    let (location, number, marker_id) = split_claim_key(key)?;
    let Some(repo) = Repo::parse(location) else {
        return Some(false);
    };
    release_claim(client, &repo, number, marker_id, diag);
    Some(true)
}

/// Park issue `index` awaiting human input: add [`AWAITING_LABEL`], remove
/// [`CLAIMED_LABEL`] (by id), and unassign **afkd only** — releasing the issue to a human
/// **without closing it**. Best-effort in order: the first error is returned.
pub(crate) fn park_issue(
    client: &dyn GiteaClient,
    repo: &Repo,
    index: u64,
    me: &str,
) -> Result<(), GiteaError> {
    client.add_label(repo, index, AWAITING_LABEL)?;
    if let Some(label) = label_named(client, repo, CLAIMED_LABEL)? {
        client.remove_label(repo, index, &label.id.to_string())?;
    }
    unassign_me(client, repo, index, me)?;
    Ok(())
}

/// The watermark: the newest `updated_at` among the bot's **own** comments, or `None`
/// when the bot has said nothing yet. A claim marker is not afkd speaking, so it is
/// skipped.
pub(crate) fn comment_watermark(comments: &[IssueComment], me: &str) -> Option<SystemTime> {
    comments
        .iter()
        .filter(|c| c.user.login == me && !is_claim(&c.body))
        .map(|c| c.updated_at)
        .max()
}

/// The comments newer than the bot's last word whose author `allows` accepts,
/// **oldest-first** and each carrying its author — the signal that a parked issue's
/// question has been answered, *and* the answer itself, so the regenerated brief shows
/// the agent what it parked on. A claim marker is dropped whatever its author.
pub(crate) fn new_reply_comments(
    comments: &[IssueComment],
    me: &str,
    allows: impl Fn(&str) -> bool,
) -> Vec<FeedbackItem> {
    let watermark = comment_watermark(comments, me);
    let mut items: Vec<&IssueComment> = comments
        .iter()
        .filter(|c| {
            c.user.login != me
                && !is_claim(&c.body)
                && watermark.is_none_or(|w| c.updated_at > w)
                && allows(&c.user.login)
        })
        .collect();
    items.sort_by_key(|c| c.updated_at);
    items
        .into_iter()
        .map(|c| FeedbackItem {
            author: c.user.login.clone(),
            body: c.body.clone(),
        })
        .collect()
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

/// A fresh directory under the system temp dir, removed on drop — the crate's own, since
/// it takes no dev-dependencies.
#[cfg(test)]
pub(crate) struct TempDir(std::path::PathBuf);

#[cfg(test)]
impl TempDir {
    pub(crate) fn new() -> Self {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("afkd-gitea-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create a temp dir");
        Self(dir)
    }

    pub(crate) fn path(&self) -> &std::path::Path {
        &self.0
    }
}

#[cfg(test)]
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
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
    use crate::client::{Action, MockClient, User};

    fn repo() -> Repo {
        Repo {
            owner: "acme".into(),
            name: "widgets".into(),
        }
    }

    /// The second every same-second race is staged in — a real wall-clock instant,
    /// decades from the epoch the seeded fixtures sit at.
    const T: u64 = 1_700_000_000;

    /// Claim `index` for `owner` against `c` on a fake clock — the shape every claim
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

    /// The `[afkd-claim]` markers left on issue `index`, as `(id, body)`. Read
    /// through the trait's own list call — the same thing the claim reads — so no
    /// test-only accessor can disagree with production about what is on the thread.
    fn markers_on(c: &MockClient, index: u64) -> Vec<(u64, String)> {
        c.list_issue_comments(&repo(), index)
            .expect("read the thread")
            .into_iter()
            .filter(|c| is_claim(&c.body))
            .map(|c| (c.id, c.body))
            .collect()
    }

    /// The comment ids this claim deleted, in order.
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

    /// AC1 — two contenders claiming the same issue **in the same second**: exactly
    /// one wins and the loser leaves no marker.
    ///
    /// The post clock is frozen, so both markers carry an identical `created_at` and
    /// the only thing that *can* decide is `won_claim`'s `(created_at, id)`
    /// tie-break on the ids the forge minted. Sequential calls against one mock are
    /// how the race is staged (a unit test has no true concurrency); each contender
    /// runs the same `claim_issue` the kind calls, and the kind contributes
    /// nothing to the decision.
    #[test]
    fn two_contenders_in_the_same_second_yield_exactly_one_winner() {
        let c = MockClient::new("bot-a");
        c.add_issue(7, "T", "B", &["afkd/ready"]);
        c.set_clock(T);

        let first = claim(&c, 7, "bot-a");
        let second = claim(&c, 7, "bot-b");

        // Exactly one win, and it is the earlier id — the tie-break, not the order
        // of the calls (both markers carry second `T`).
        assert_eq!(first, Claimed::Won(surviving_marker(&c, 7, "bot-a")));
        assert_eq!(second, Claimed::Lost);
        // One marker survives on the thread, and it is the *winner's*: the loser
        // deleted its own on the way out.
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

    /// The N-way shape of the same rule: five contenders in one second leave one
    /// winner, four losses, and one marker.
    #[test]
    fn n_contenders_in_the_same_second_yield_exactly_one_winner() {
        let c = MockClient::new("bot-1");
        c.add_issue(7, "T", "B", &["afkd/ready"]);
        c.set_clock(T);

        // A non-ASCII owner among them: the identity is handed through verbatim, and
        // the decision must not care what it says.
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

    /// AC2 — the decision is a pure function of the **re-read**: our own post
    /// succeeded and handed us an id, yet an earlier live rival that landed during
    /// the settle takes the unit. Nothing concludes "I won" from its own write, and
    /// the claim path records only a comment (no assignee write can be deciding it).
    #[test]
    fn the_claim_never_concludes_it_won_from_its_own_write() {
        let c = MockClient::new("me");
        c.add_issue(7, "T", "B", &["afkd/ready"]);
        c.set_clock(T);
        // A rival marker that landed between our post and our re-read, one second
        // earlier and so strictly ahead of us in the order.
        c.rival_claims_next("rival", 42, T - 1);

        assert_eq!(claim(&c, 7, "me"), Claimed::Lost);
        assert_eq!(
            markers_on(&c, 7),
            vec![(42, claim_text("rival"))],
            "our marker is gone; the rival's is untouched"
        );
        assert!(
            !c.actions()
                .iter()
                .any(|a| matches!(a, Action::Assign { .. })),
            "the claim writes no assignee at all: {:?}",
            c.actions()
        );
    }

    /// AC3 — the marker order is each comment's **creation** time: a rival created
    /// before ours but *edited* after it still wins, and the mirror image still
    /// loses. Under `updated_at` both assertions would flip.
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

    /// AC4 — a failed re-read deletes our marker before propagating: the error is
    /// still the caller's to log, but the issue is not left locked for an hour by a
    /// claim nobody is acting on.
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
    }

    /// AC4 — a marker older than the claim lifetime does not block a new claim, and
    /// one exactly *at* the boundary still does. Liveness is measured against our
    /// own post time, so the fixture is hermetic.
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

    /// AC5 — releasing never clears an assignee it did not set. The reaper's release
    /// removes the status label and the crashed run's marker; a human's assignment
    /// (and afkd's own, which only `on_fail`/park take back) is left byte-identical.
    #[test]
    fn release_claim_drops_the_label_and_marker_but_no_assignee() {
        let c = MockClient::new("me");
        c.add_issue_assigned(7, "T", "B", &["afkd/claimed"], &["alice", "me"]);
        c.add_comment_body(7, 42, "me", &claim_text("me"), 100);
        c.add_comment_body(7, 43, "alice", "any progress?", 200);

        release_claim(&c, &repo(), 7, 42, &CaptureDiag::default());

        assert!(!c.has_label(7, CLAIMED_LABEL));
        assert!(markers_on(&c, 7).is_empty());
        assert_eq!(
            c.assignees_of(7),
            vec!["alice".to_string(), "me".to_string()],
            "the assignee set is untouched"
        );
        assert_eq!(
            c.list_issue_comments(&repo(), 7).unwrap().len(),
            1,
            "only the marker was deleted"
        );
    }

    /// AC4 — the reaper's key handling over the three shapes it can be handed: a
    /// well-formed key releases, an unparseable location half is left to retry, and
    /// a key that is not a claim key at all names nothing releasable.
    #[test]
    fn release_stale_reads_the_three_key_shapes() {
        let c = MockClient::new("me");
        c.add_issue(7, "T", "B", &["afkd/claimed"]);
        c.add_comment_body(7, 4242, "me", &claim_text("me"), 100);

        assert_eq!(
            release_stale(&c, "acme/widgets#7#4242", &CaptureDiag::default()),
            Some(true)
        );
        assert!(markers_on(&c, 7).is_empty(), "the marker was reaped");
        assert!(!c.has_label(7, CLAIMED_LABEL));

        // A location half with no `owner/name` shape: retried, not dropped.
        assert_eq!(
            release_stale(&c, "not-a-repo#7#4242", &CaptureDiag::default()),
            Some(false)
        );
        // The pre-marker two-part key an older binary persisted: not read, not
        // migrated (ADR-0069) — it names nothing releasable, so it is dropped.
        assert_eq!(
            release_stale(&c, "acme/widgets#7", &CaptureDiag::default()),
            None
        );
        assert_eq!(release_stale(&c, "garbage", &CaptureDiag::default()), None);
    }

    /// AC5 — `assign_me` is a union, not a replace: claiming an issue a human is
    /// already on keeps them there. `unassign` is its mirror, removing afkd alone.
    #[test]
    fn the_assignee_verbs_are_self_scoped() {
        let c = MockClient::new("me");
        c.add_issue_assigned(7, "T", "B", &[], &["alice"]);

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
            c.assignees_of(7),
            vec!["alice".to_string(), "me".to_string()],
            "the human assignee survives the claim's status write"
        );

        // Assigning again is a no-op, not a duplicate row.
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
            c.assignees_of(7),
            vec!["alice".to_string(), "me".to_string()]
        );

        do_action(
            &c,
            &repo(),
            7,
            &LifecycleAction::Unassign,
            "me",
            &Facts::none(),
        )
        .unwrap();
        assert_eq!(
            c.assignees_of(7),
            vec!["alice".to_string()],
            "unassign releases afkd's own row only"
        );
    }

    #[test]
    fn do_action_label_remove_resolves_to_an_id() {
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
        // The recorded action names the label id, never the all-clearing bare path.
        assert!(c
            .actions()
            .iter()
            .any(|a| matches!(a, crate::client::Action::Unlabel { .. })));
        assert!(!c.has_label(7, "afkd/claimed"));
    }

    #[test]
    fn do_action_label_remove_is_a_no_op_when_the_name_has_no_id() {
        // A LabelRemove for a name that resolves to no id is a no-op (nothing to remove) —
        // never the all-clearing bare `…/labels` path (ADR-0031). Here the label was never
        // defined, so `label_id` returns `None` and no removal is recorded.
        let c = MockClient::new("me");
        c.add_issue(7, "T", "B", &[]);
        do_action(
            &c,
            &repo(),
            7,
            &LifecycleAction::LabelRemove("afkd/absent".into()),
            "me",
            &Facts::none(),
        )
        .unwrap();
        assert!(
            !c.actions()
                .iter()
                .any(|a| matches!(a, crate::client::Action::Unlabel { .. })),
            "an unresolved label name removes nothing"
        );
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
            .any(|a| matches!(a, crate::client::Action::State { state, .. } if state == "closed")));
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
        assert!(c.actions().iter().any(
            |a| matches!(a, crate::client::Action::Comment { body, .. } if body == "handled by afkd")
        ));
    }

    #[test]
    fn do_action_comment_substitutes_run_facts() {
        // The primary run-end proof (ADR-0064): a `@{run:…}` comment posts the rendered
        // facts — `format_duration` output, `$<2dp>`, the bare turn count, and the
        // finishing fire's run directory name.
        let c = MockClient::new("me");
        c.add_issue(7, "T", "B", &[]);
        let facts = Facts {
            duration_ms: 5_000,
            cost: 1.5,
            turns: Some(3),
            run_name: Some("260722-141802-issue-7-1".into()),
            ..Facts::none()
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
                crate::client::Action::Comment { body, .. }
                    if body == "done in 5.00s — $1.50, 3 turns — \
                                log: .afkd/runs/afkd::selfdev/260722-141802-issue-7-1/run.log"
            )),
            "{:?}",
            c.actions()
        );
    }

    /// AC5 — a `label_add` naming a label the repository does not define is an
    /// error, not a quiet no-op: Gitea drops the name and answers success, the client
    /// reads the resulting list and refuses, and `apply_actions` abandons the rest of
    /// the moment. The `close` behind it never runs, so a half-applied moment is
    /// surfaced rather than finished.
    #[test]
    fn a_label_add_the_forge_did_not_apply_aborts_the_moment() {
        let c = MockClient::new("me");
        c.add_issue(7, "T", "B", &["afkd/ready"]);
        // `afkd/working` is never defined in this repo — the shape the docs' own
        // example config walks a user straight into.
        let err = apply_actions(
            &c,
            &repo(),
            7,
            &[
                LifecycleAction::LabelAdd("afkd/working".into()),
                LifecycleAction::Close,
            ],
            "me",
            &Facts::none(),
        )
        .expect_err("a dropped label add is a failed moment");
        assert_eq!(err.stage(), "add label");
        assert!(
            err.to_string().contains("afkd/working"),
            "the error names the label: {err}"
        );
        assert!(!c.has_label(7, "afkd/working"));
        assert!(
            !c.actions()
                .iter()
                .any(|a| matches!(a, Action::State { .. })),
            "the close after the failed add never ran: {:?}",
            c.actions()
        );
    }

    /// The ensure defines what is missing and leaves what is there: both managed
    /// labels are created **non-exclusive** in a repo that has neither, and a second
    /// pass over the now-complete set creates nothing.
    #[test]
    fn the_ensure_creates_both_managed_labels_non_exclusive() {
        let c = MockClient::new("me");
        // A repo mid-way: the awaiting label exists (the docs named it), the claim
        // gate does not.
        c.register_label(AWAITING_LABEL, false);

        ensure_issue_labels(&c, &repo()).expect("the ensure ran");
        assert_eq!(
            c.list_labels(&repo())
                .unwrap()
                .into_iter()
                .map(|l| (l.name, l.exclusive))
                .collect::<Vec<_>>(),
            vec![
                (AWAITING_LABEL.to_string(), false),
                (CLAIMED_LABEL.to_string(), false),
            ],
            "the missing gate was created, plain"
        );

        // Idempotent: a repo that already defines both is untouched.
        c.fail("create label");
        ensure_issue_labels(&c, &repo()).expect("nothing left to create");
    }

    /// The ensure's refusals, by kind. An **exclusive** managed label is fatal with a
    /// sentence naming repo, label and the exclusivity (a human must recreate it); a
    /// **definite** create failure is fatal naming repo and label; a **transient** one
    /// is the retried poll error it has always been. Both managed names are covered —
    /// the park's label is as much a gate as the claim's.
    #[test]
    fn the_ensure_refuses_an_exclusive_label_and_classifies_a_failed_create() {
        for name in [CLAIMED_LABEL, AWAITING_LABEL] {
            let c = MockClient::new("me");
            c.register_label(name, true);
            let Err(ClaimFault::Fatal(reason)) = ensure_issue_labels(&c, &repo()) else {
                panic!("an exclusive {name} must be fatal");
            };
            for needle in ["acme/widgets", name, "exclusive"] {
                assert!(reason.contains(needle), "{needle:?} missing from: {reason}");
            }
        }

        // A 403 on the create: definite, so fatal, and it says which label on which
        // repo could not be created.
        let c = MockClient::new("me");
        c.refuse("create label", 403);
        let Err(ClaimFault::Fatal(reason)) = ensure_issue_labels(&c, &repo()) else {
            panic!("a refused create must be fatal");
        };
        for needle in ["acme/widgets", CLAIMED_LABEL, "403"] {
            assert!(reason.contains(needle), "{needle:?} missing from: {reason}");
        }

        // A transport blip on the same call, and a 500 on the listing: transient.
        let c = MockClient::new("me");
        c.fail("create label");
        assert!(matches!(
            ensure_issue_labels(&c, &repo()),
            Err(ClaimFault::Transient(_))
        ));
        let c = MockClient::new("me");
        c.fail("list labels");
        assert!(matches!(
            ensure_issue_labels(&c, &repo()),
            Err(ClaimFault::Transient(_))
        ));
    }

    /// The claim's status-label write is classified by the same rule, and its fatal
    /// sentence names the unit it could not gate. A `Refused` (the forge's success
    /// that did nothing) and a 4xx are definite; a 5xx, a transport failure and an
    /// undecodable body are not.
    #[test]
    fn a_definite_claim_label_failure_is_fatal_and_names_the_unit() {
        let refused = GiteaError::Refused {
            stage: "add label",
            reason: "label “afkd/claimed” was not applied".into(),
        };
        let ClaimFault::Fatal(reason) = claim_label_fault(&repo(), 12, CLAIMED_LABEL, refused)
        else {
            panic!("a refused claim label is definite");
        };
        for needle in ["acme/widgets#12", CLAIMED_LABEL] {
            assert!(reason.contains(needle), "{needle:?} missing from: {reason}");
        }

        for definite_error in [
            GiteaError::Status {
                stage: "add label",
                status: 403,
            },
            GiteaError::Status {
                stage: "add label",
                status: 422,
            },
        ] {
            assert!(
                matches!(
                    claim_label_fault(&repo(), 12, CLAIMED_LABEL, definite_error),
                    ClaimFault::Fatal(_)
                ),
                "a 4xx is a definite verdict"
            );
        }
        for transient in [
            GiteaError::Status {
                stage: "add label",
                status: 502,
            },
            GiteaError::Transport {
                stage: "add label",
                reason: "connection reset".into(),
            },
            GiteaError::Decode {
                stage: "add label",
                reason: "not json".into(),
            },
        ] {
            assert!(
                matches!(
                    claim_label_fault(&repo(), 12, CLAIMED_LABEL, transient),
                    ClaimFault::Transient(_)
                ),
                "a blip must never take the service down"
            );
        }
    }

    #[test]
    fn apply_actions_stops_at_the_first_failure() {
        // A lifecycle moment's actions run in order; the first failure aborts the
        // rest and is returned, so a half-applied moment is surfaced (not silently
        // finished). Here the label add fails, so the following Close never runs.
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
        // The Close after the failed action never ran (no state change recorded).
        assert!(
            !c.actions()
                .iter()
                .any(|a| matches!(a, crate::client::Action::State { .. })),
            "actions after the first failure are not applied"
        );
    }

    #[test]
    fn park_adds_awaiting_removes_claimed_and_unassigns_afkd_only() {
        let c = MockClient::new("me");
        // The repo defines the awaiting label (the claim path's ensure created it),
        // so the park's write really lands — see the AC6 test for the other case.
        c.register_label(AWAITING_LABEL, false);
        // A human is on the issue too — the park hands it back to *them*, so their
        // assignment must survive it.
        c.add_issue_assigned(
            7,
            "T",
            "B",
            &["afkd/ready", "afkd/claimed"],
            &["alice", "me"],
        );

        park_issue(&c, &repo(), 7, "me").unwrap();

        assert!(c.has_label(7, AWAITING_LABEL));
        assert!(!c.has_label(7, CLAIMED_LABEL));
        assert!(c.has_label(7, "afkd/ready"), "the source label is kept");
        assert_eq!(c.assignees_of(7), vec!["alice".to_string()]);
    }

    /// An unedited comment: written and last touched at the same instant.
    fn comment(id: u64, author: &str, secs: u64) -> IssueComment {
        edited_comment(id, author, secs, secs)
    }

    /// A comment written at `created` and last edited at `updated` — the shape that
    /// tells the gate's two times apart.
    fn edited_comment(id: u64, author: &str, created: u64, updated: u64) -> IssueComment {
        IssueComment {
            id,
            body: format!("comment {id}"),
            user: User {
                login: author.to_string(),
            },
            created_at: std::time::UNIX_EPOCH + std::time::Duration::from_secs(created),
            updated_at: std::time::UNIX_EPOCH + std::time::Duration::from_secs(updated),
        }
    }

    #[test]
    fn watermark_is_the_newest_of_the_bots_own_comments() {
        let comments = [comment(1, "human", 100), comment(2, "me", 200)];
        assert_eq!(
            comment_watermark(&comments, "me"),
            Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(200))
        );
        // With nothing said by the bot, there is no watermark.
        assert_eq!(comment_watermark(&[comment(1, "human", 100)], "me"), None);
    }

    /// The delta with no allow-list — the awaiting-reply re-arm path, which takes
    /// any author.
    fn new_human_comments(comments: &[IssueComment], me: &str) -> Vec<FeedbackItem> {
        new_reply_comments(comments, me, |_| true)
    }

    #[test]
    fn new_human_comment_only_when_one_postdates_the_bots_last_word() {
        // Human spoke after the bot's question → answered, re-claimable.
        assert!(
            !new_human_comments(&[comment(1, "me", 100), comment(2, "human", 200)], "me")
                .is_empty()
        );
        // The bot replied last; nothing newer arrived → still waiting.
        assert!(
            new_human_comments(&[comment(1, "human", 100), comment(2, "me", 200)], "me").is_empty()
        );
        // Bot never spoke but a human has → counts as new (first round).
        assert!(!new_human_comments(&[comment(1, "human", 100)], "me").is_empty());
        // Silence is not an answer.
        assert!(new_human_comments(&[comment(1, "me", 100)], "me").is_empty());
    }

    /// The allow-list filters the retained delta, not only the fire decision: a
    /// disallowed author's reply in the tail is neither counted nor briefed, and a
    /// disallowed comment landing *after* an allowed one does not mask it.
    #[test]
    fn new_reply_comments_drop_a_disallowed_author() {
        let comments = [
            comment(1, "me", 100),
            comment(2, "alice", 200),
            comment(3, "mallory", 300),
        ];
        let delta = new_reply_comments(&comments, "me", |l| l == "alice");
        assert_eq!(
            delta,
            vec![FeedbackItem {
                author: "alice".into(),
                body: "comment 2".into(),
            }],
            "only the allowed author's reply is retained"
        );
        assert!(
            new_reply_comments(&comments, "me", |l| l == "bob").is_empty(),
            "nobody allowed in the tail is an empty delta"
        );
    }

    /// The delta the bool never carried: who answered and what they said, in
    /// thread order, with the bot's own word excluded — several humans may have
    /// replied since the park, and the brief shows all of them.
    #[test]
    fn new_human_comments_carry_each_replier_oldest_first() {
        // The park shape: an old human comment, the bot's parking question, then
        // two humans answering it out of order.
        let comments = [
            comment(1, "alice", 50),
            comment(2, "me", 100),
            comment(4, "bob", 400),
            comment(3, "alice", 200),
        ];
        let delta = new_human_comments(&comments, "me");
        assert_eq!(
            delta,
            vec![
                FeedbackItem {
                    author: "alice".into(),
                    body: "comment 3".into(),
                },
                FeedbackItem {
                    author: "bob".into(),
                    body: "comment 4".into(),
                },
            ],
            "oldest-first, authored, never the bot's own, and never pre-watermark"
        );
    }

    /// The `discuss_with` gate's two halves keep reading `updated_at` now that a
    /// comment also carries `created_at`: the question is "has this been touched
    /// since the bot last spoke", and an edited answer is still an answer. The
    /// fixture flips under the other key — the bot wrote first but edited last,
    /// one human wrote before the bot's word and edited after it, and one wrote
    /// after the bot's word but has not touched it since.
    #[test]
    fn the_reply_gate_reads_updated_at_not_created_at() {
        let comments = [
            edited_comment(1, "me", 100, 500),
            edited_comment(2, "josefandersson", 200, 600),
            edited_comment(3, "björn-öst", 300, 400),
        ];

        // The watermark is the bot's *edit* (500), not when it wrote (100).
        assert_eq!(
            comment_watermark(&comments, "me"),
            Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(500))
        );
        // Only the edited-after reply answers the park. Under `created_at` both
        // humans would count, and `björn-öst` would sort last rather than vanish.
        assert_eq!(
            new_reply_comments(&comments, "me", |_| true),
            vec![FeedbackItem {
                author: "josefandersson".into(),
                body: "comment 2".into(),
            }]
        );
        // The allow-list half is unaffected by the new field: the same fixture with
        // only the untouched author allowed is still silence.
        assert!(new_reply_comments(&comments, "me", |l| l == "björn-öst").is_empty());
    }

    /// AC6 — a claim marker is never afkd speaking, and never a human's word either.
    ///
    /// Asserted as **parity**: the same reply-gate reads over one thread, computed
    /// with and without two markers interleaved into it, must agree — not two
    /// hand-written expectations that could drift. The markers sit exactly where
    /// they would do damage if counted: **ours** after the bot's last word (it would
    /// push the watermark past the reply and leave a parked issue waiting forever)
    /// and a **rival's** newest of all, authored by a different login (it would read
    /// as the human reply that unparks the issue, and be briefed back to the agent
    /// as feedback).
    ///
    /// The thread underneath is the adversarial one: an edited comment, multi-line
    /// prose with a code block, non-ASCII and wide handles.
    #[test]
    fn a_claim_marker_is_never_read_as_conversation() {
        let body = |id: u64, author: &str, text: &str, created: u64, updated: u64| IssueComment {
            id,
            body: text.to_string(),
            user: User {
                login: author.to_string(),
            },
            created_at: std::time::UNIX_EPOCH + std::time::Duration::from_secs(created),
            updated_at: std::time::UNIX_EPOCH + std::time::Duration::from_secs(updated),
        };
        let conversation = [
            body(1, "me", "Which backoff should I use?", 100, 100),
            body(
                2,
                "álvaro",
                "Exponential, please — see §4 of the RFC 🙏\n\n    max_backoff = 30\n",
                150,
                200,
            ),
            body(3, "陳大文", "看起来不对 🚨", 300, 300),
        ];
        let mut with_markers = conversation.to_vec();
        with_markers.push(body(4, "me", &claim_text("me"), 400, 400));
        with_markers.push(body(
            5,
            "björn-öst[bot]",
            &claim_text("björn-öst[bot]"),
            500,
            500,
        ));

        assert_eq!(
            comment_watermark(&with_markers, "me"),
            comment_watermark(&conversation, "me"),
            "a marker is not the bot's last word"
        );
        for allowed in ["álvaro", "陳大文", "björn-öst[bot]"] {
            assert_eq!(
                new_reply_comments(&with_markers, "me", |l| l == allowed),
                new_reply_comments(&conversation, "me", |l| l == allowed),
                "a marker is neither a reply nor a boundary (allowing {allowed})"
            );
        }
        assert_eq!(
            new_reply_comments(&with_markers, "me", |_| true),
            new_reply_comments(&conversation, "me", |_| true)
        );
        // Not vacuous: the marker-free read really does carry the two human replies,
        // and no marker text reaches the brief.
        assert_eq!(
            new_reply_comments(&with_markers, "me", |_| true)
                .iter()
                .map(|i| i.author.clone())
                .collect::<Vec<_>>(),
            vec!["álvaro", "陳大文"]
        );
        // …and the rival's marker allowed by name is still not a reply.
        assert!(new_reply_comments(&with_markers, "me", |l| l == "björn-öst[bot]").is_empty());
    }
}
