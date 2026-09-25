//! The `gitea` issue kind's vendor half (ADR-0031 Capability 1), ported from afkd's
//! `crates/gitea/src/trigger_issue.rs`.
//!
//! It polls a repository (or an org's repositories) through the mockable
//! [`GiteaClient`] seam for issues that are **open** and either **assigned to the bot**
//! (assigning the bot's own user *is* the queue), carrying the optional **source label**,
//! or carrying `afkd/awaiting-reply` (parked work due for re-arm); claims one with a
//! `[afkd-claim]` marker comment (post → settle → re-read → decide, then the
//! `afkd/claimed` status label and `on_claim`); and reflects the run's end back through
//! Gitea's native assignee/label/state primitives. Its logic is unit-tested with **no
//! network** against [`MockClient`](crate::client::MockClient) and a fake clock.
//!
//! afkd keeps the spine — the cadence, the queue lane, the claim journal, the attempt
//! bound, the mid-run watch's cursor, and the framing of `task.md` — so what is here is
//! exactly what the built-in's `ForgeUnits` impl does on the vendor side.
//!
//! The optional `discuss_with` key turns intake into a **conversation**: a candidate must
//! additionally pass the comment-tail gate ([`tail_decision`]) — it fires on first sight,
//! goes quiet once afkd has spoken, and re-fires only on an allowed author's reply — and
//! every turn ends with a backstop comment so afkd is always the last speaker. Unset, the
//! **candidate scan** reads no comments at all.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use crate::claim::is_claim;
use crate::client::{GiteaClient, GiteaError, Issue, IssueComment, Repo};
use crate::common::{
    apply_actions, claim_issue, claim_key_for, claim_label_fault, comment_watermark, creds_env,
    delete_marker, ensure_issue_labels, new_reply_comments, park_issue, release_claim,
    release_stale, renew_marker, unit_key, ClaimFault, Claimed, Clock, Diag, ScanBudget, Target,
    AWAITING_LABEL, CLAIMED_LABEL, ENV_ISSUE_NUMBER, ENV_REPO,
};
use crate::feedback::{render_feedback_section, FeedbackItem};
use crate::kind::{ClaimedUnit, Units};
use crate::lifecycle::LifecycleAction;
use crate::settings::{DiscussWith, GiteaConfig};
use crate::wire::{Facts, UnitOutcome, WireFile, WireUnit};
use crate::{ISSUE_DIR, NUMBER_FILE, PARK_FILE, TASK_FILE};

/// One issue taken on as a unit of work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Unit {
    pub(crate) repo: Repo,
    pub(crate) number: u64,
    /// The id of the `[afkd-claim]` marker comment this claim won on — released at run
    /// end, and the tail of the claim-journal key so a crashed run's marker is reaped.
    pub(crate) claim_id: u64,
    pub(crate) title: String,
    pub(crate) body: String,
    /// The comments that unparked this issue (oldest-first, each authored), so the
    /// regenerated brief shows the answer to the question the run parked on.
    pub(crate) feedback: Vec<FeedbackItem>,
    /// The ids of every comment on the issue at claim time — the `seen` afkd's watch
    /// starts from, and the "before" half of the `discuss_with` backstop's diff. Empty
    /// when the claim read no comments.
    pub(crate) before_comments: Vec<u64>,
    /// The login this issue was claimed as — the unit's `self`.
    pub(crate) claimed_as: String,
}

impl Unit {
    /// The claim-journal key — the wire unit's `key`, and what every later call names it
    /// by. It carries the claim marker, so it changes on every claim of the issue.
    pub(crate) fn key(&self) -> String {
        claim_key_for(&self.repo, self.number, self.claim_id)
    }

    /// The issue's stable coordinate, `<full-name>#<number>` (ADR-0067): the wire unit's
    /// `thread`, the same across every claim, so an agent session resumes per issue.
    pub(crate) fn thread(&self) -> String {
        unit_key(&self.repo, self.number)
    }
}

impl ClaimedUnit for Unit {
    fn thread(&self) -> String {
        Unit::thread(self)
    }

    fn claimed_as(&self) -> &str {
        &self.claimed_as
    }
}

/// The `discuss_with` gate resolved for one poll: afkd's own login (the tail boundary and
/// the never-allowed author), whether any author is allowed, and the allow-list.
struct DiscussGate {
    self_login: String,
    anyone: bool,
    allowed: HashSet<String>,
}

impl DiscussGate {
    /// Resolve `dw` against the claim identity `me`. Self is never an allowed author,
    /// however it was named — afkd must not answer itself.
    fn resolve(dw: &DiscussWith, me: &str) -> Self {
        let (anyone, mut allowed) = match dw {
            DiscussWith::Anyone => (true, HashSet::new()),
            DiscussWith::Logins(logins) => (false, logins.iter().cloned().collect()),
        };
        allowed.remove(me);
        Self {
            self_login: me.to_string(),
            anyone,
            allowed,
        }
    }

    /// Whether a comment by `login` counts as a reply.
    fn allows(&self, login: &str) -> bool {
        login != self.self_login && (self.anyone || self.allowed.contains(login))
    }
}

/// The `gitea` kind's vendor half: the target, the intake gates, the claim, and the four
/// lifecycle action lists.
pub(crate) struct IssueUnits {
    client: Box<dyn GiteaClient>,
    target: Target,
    source_label: String,
    on_claim: Vec<LifecycleAction>,
    on_done: Vec<LifecycleAction>,
    on_fail: Vec<LifecycleAction>,
    on_park: Vec<LifecycleAction>,
    discuss_with: Option<DiscussWith>,
    /// The token and base URL every unit's `env` carries.
    creds: BTreeMap<String, String>,
}

impl IssueUnits {
    /// Exactly one of `cfg.repo`/`cfg.org` is set (the settings layer enforces it); a
    /// malformed `repo` degrades to an org-less, repo-less target that claims nothing.
    pub(crate) fn new(client: Box<dyn GiteaClient>, cfg: &GiteaConfig) -> Self {
        Self {
            client,
            target: Target::new(cfg),
            source_label: cfg.source_label.clone(),
            on_claim: cfg.on_claim.clone(),
            on_done: cfg.on_done.clone(),
            on_fail: cfg.on_fail.clone(),
            on_park: cfg.on_park.clone(),
            discuss_with: cfg.discuss_with.clone(),
            creds: creds_env(cfg),
        }
    }

    /// Resolve the authenticated user (the claim identity).
    pub(crate) fn resolve_me(&self) -> Result<String, GiteaError> {
        Ok(self.client.current_user()?.login)
    }

    /// The candidate issues for one repo: the open issues **assigned to the bot**, those
    /// carrying the optional **source label**, or those carrying `afkd/awaiting-reply`.
    /// One `list_issues(.., "open", "")` plus a client-side predicate: Gitea's issues
    /// endpoint has no dependable per-assignee filter, and each issue carries its
    /// assignees already.
    fn candidates(&self, repo: &Repo, me: &str) -> Result<Vec<Issue>, GiteaError> {
        Ok(self
            .client
            .list_issues(repo, "open", "")?
            .into_iter()
            .filter(|issue| {
                issue.assigned_to(me)
                    || (!self.source_label.is_empty() && issue.has_label(&self.source_label))
                    || issue.has_label(AWAITING_LABEL)
            })
            .collect())
    }

    /// Whether `issue` is claimable: open and not already `afkd/claimed`.
    fn eligible(&self, issue: &Issue) -> bool {
        issue.state == "open" && !issue.has_label(CLAIMED_LABEL)
    }

    /// Apply a lifecycle moment's actions, diagnosing the first failure. Returns whether
    /// the moment fully applied — the terminal moments' half of the delivery verdict.
    fn apply(
        &self,
        repo: &Repo,
        index: u64,
        actions: &[LifecycleAction],
        me: &str,
        facts: &Facts,
        diag: &dyn Diag,
    ) -> bool {
        if let Err(e) = apply_actions(&*self.client, repo, index, actions, me, facts) {
            diag.err(&e);
            return false;
        }
        true
    }

    /// Find the first eligible issue across the polled repos and claim it — the claim
    /// race runs here, inside afkd's `poll`.
    ///
    /// The scan stops once it has run [`POLL_BUDGET`](crate::common::POLL_BUDGET),
    /// checked before each repo and before each claim attempt, and answers "nothing this
    /// beat".
    pub(crate) fn try_claim_next(
        &self,
        me: &str,
        diag: &dyn Diag,
        clock: &dyn Clock,
    ) -> Result<Option<Unit>, ClaimFault<GiteaError>> {
        let budget = ScanBudget::start(clock, diag);
        // Resolve the `discuss_with` gate once per poll, never per issue.
        let gate = self
            .discuss_with
            .as_ref()
            .map(|dw| DiscussGate::resolve(dw, me));
        for repo in self.target.repos(&*self.client)? {
            if budget.spent() {
                return Ok(None);
            }
            // The managed labels are ensured lazily, once per repo per **claiming** poll,
            // and before the first claim, so a repo whose `afkd/claimed` is exclusive
            // posts no marker and starts no run.
            let mut ensured = false;
            for issue in self.candidates(&repo, me)? {
                if !self.eligible(&issue) {
                    continue;
                }
                // The tail check, last among the predicates because it costs a per-issue
                // read. A parked issue re-arms only once a human replied newer than the
                // bot's last word; `discuss_with` applies that rule to every candidate.
                // The unparking replies are retained for the regenerated brief.
                let parked = issue.has_label(AWAITING_LABEL);
                let mut feedback = Vec::new();
                let mut before_comments = Vec::new();
                if gate.is_some() || parked {
                    let comments = self.client.list_issue_comments(&repo, issue.number)?;
                    let (fires, replies) = tail_decision(&comments, me, gate.as_ref());
                    if !fires {
                        continue;
                    }
                    feedback = replies;
                    before_comments = comments.iter().map(|c| c.id).collect();
                }
                if budget.spent() {
                    return Ok(None);
                }
                if !ensured {
                    ensure_issue_labels(&*self.client, &repo)?;
                    ensured = true;
                }
                match claim_issue(&*self.client, &repo, issue.number, me, clock, diag)? {
                    Claimed::Won(claim_id) => {
                        // The claim is the marker; the label is the **status** it shows —
                        // and the re-pick gate, so the plugin adds it itself. A definite
                        // refusal is fatal, and the marker goes with us, so the crash
                        // leaves nothing blocking the claim a human's fix makes possible.
                        if let Err(e) = self.client.add_label(&repo, issue.number, CLAIMED_LABEL) {
                            match claim_label_fault(&repo, issue.number, CLAIMED_LABEL, e) {
                                fatal @ ClaimFault::Fatal(_) => {
                                    delete_marker(&*self.client, &repo, claim_id, diag);
                                    return Err(fatal);
                                }
                                ClaimFault::Transient(e) => diag.err(&e),
                            }
                        }
                        // Re-claim of a parked issue: drop the awaiting-reply marker so
                        // the gate is clean for the next round.
                        if parked {
                            self.apply(
                                &repo,
                                issue.number,
                                &[LifecycleAction::LabelRemove(AWAITING_LABEL.to_string())],
                                me,
                                &Facts::none(),
                                diag,
                            );
                        }
                        self.apply(
                            &repo,
                            issue.number,
                            &self.on_claim,
                            me,
                            &Facts::none(),
                            diag,
                        );
                        return Ok(Some(Unit {
                            repo,
                            number: issue.number,
                            claim_id,
                            title: issue.title,
                            body: issue.body,
                            feedback,
                            before_comments,
                            claimed_as: me.to_string(),
                        }));
                    }
                    // Lost the race for this issue; try the next one.
                    Claimed::Lost => continue,
                }
            }
        }
        Ok(None)
    }

    /// Release one claim named by a whole journal key — a crashed run's leftover, or a
    /// unit afkd handed back.
    pub(crate) fn release_stale(&self, key: &str, diag: &dyn Diag) -> Option<bool> {
        release_stale(&*self.client, key, diag)
    }

    /// Release this unit's claim now — the recovery the built-in's reaper performs for a
    /// terminal lifecycle that did not land.
    pub(crate) fn release(&self, unit: &Unit, diag: &dyn Diag) {
        release_claim(&*self.client, &unit.repo, unit.number, unit.claim_id, diag);
    }

    /// Keep this unit's claim marker alive for as long as afkd's fire holds it.
    pub(crate) fn renew(&self, unit: &Unit, renewal: u64, diag: &dyn Diag) {
        renew_marker(
            &*self.client,
            &unit.repo,
            unit.claim_id,
            &unit.claimed_as,
            renewal,
            diag,
        );
    }

    /// Every comment on the unit's issue, for afkd's mid-run watch.
    pub(crate) fn comments(&self, unit: &Unit) -> Result<Vec<IssueComment>, GiteaError> {
        self.client.list_issue_comments(&unit.repo, unit.number)
    }

    /// The clarification gate's read: a [`PARK_FILE`] marker in the attempt's scratch
    /// directory means the agent asked for human input (the `gitea` skill's ask action
    /// wrote it), so the attempt **parks**. The marker is authoritative; otherwise afkd's
    /// own verdict stands.
    pub(crate) fn classify(scratch: &Path, verdict: UnitOutcome) -> UnitOutcome {
        if scratch.join(PARK_FILE).is_file() {
            return UnitOutcome::Park;
        }
        verdict
    }

    /// The terminal lifecycle: `on_done` (clean), `on_fail` (exhausted), or the **park**
    /// — then the claim marker's release and, on the `discuss_with` path, the backstop
    /// that keeps afkd the last speaker.
    ///
    /// Returns whether the moment reached the remote. The park needs **both** halves: a
    /// park whose label swap failed leaves `afkd/claimed` standing, so the issue is
    /// neither parked nor re-pickable.
    pub(crate) fn finish(
        &self,
        unit: &Unit,
        outcome: UnitOutcome,
        facts: &Facts,
        diag: &dyn Diag,
    ) -> bool {
        let me = unit.claimed_as.as_str();
        let delivered = match outcome {
            UnitOutcome::Clean => {
                self.apply(&unit.repo, unit.number, &self.on_done, me, facts, diag)
            }
            UnitOutcome::Failed => {
                self.apply(&unit.repo, unit.number, &self.on_fail, me, facts, diag)
            }
            UnitOutcome::Park => {
                // Both evaluated into locals first: the extras run whether or not the park
                // landed, so the verdict's `&&` never short-circuits the second half.
                let parked = match park_issue(&*self.client, &unit.repo, unit.number, me) {
                    Ok(()) => true,
                    Err(e) => {
                        diag.err(&e);
                        false
                    }
                };
                let extras = self.apply(&unit.repo, unit.number, &self.on_park, me, facts, diag);
                parked && extras
            }
        };
        // The claim is over however it ended, so its marker goes — unconditionally, and
        // outside the delivery verdict: the lock is the label.
        if let Err(e) = self.client.delete_comment(&unit.repo, unit.claim_id) {
            diag.err(&e);
        }
        // Last: a lifecycle `comment` counts as afkd speaking, so it suppresses the
        // backstop.
        if self.discuss_with.is_some() {
            self.post_backstop_if_silent(unit, outcome, facts, diag);
        }
        delivered
    }

    /// On the `discuss_with` path, guarantee afkd is the last speaker after a turn: if no
    /// comment authored by afkd is new since the claim (its own claim marker aside), post
    /// one backstop. The diff keys on comment **id + author**, never a timestamp. If the
    /// after-read fails, post anyway: a rare redundant comment beats the re-fire loop.
    fn post_backstop_if_silent(
        &self,
        unit: &Unit,
        outcome: UnitOutcome,
        facts: &Facts,
        diag: &dyn Diag,
    ) {
        let me = unit.claimed_as.as_str();
        let spoke = match self.client.list_issue_comments(&unit.repo, unit.number) {
            Ok(after) => {
                let before: HashSet<u64> = unit.before_comments.iter().copied().collect();
                after
                    .iter()
                    .any(|c| c.user.login == me && !is_claim(&c.body) && !before.contains(&c.id))
            }
            Err(e) => {
                diag.err(&e);
                false
            }
        };
        if !spoke {
            if let Err(e) =
                self.client
                    .post_comment(&unit.repo, unit.number, &backstop_text(outcome, facts))
            {
                diag.err(&e);
            }
        }
    }

    /// The unit as it crosses the wire: the built-in's `unit_key` / `unit_thread` /
    /// `unit_env` ∪ `creds_env` / `scratch_layout`, with the claim-time comment ids as
    /// `seen` and the claim identity as `self`. The brief is unframed — afkd frames
    /// `task.md` itself (ADR-0081).
    pub(crate) fn wire_unit(&self, unit: &Unit) -> WireUnit {
        let mut env = self.creds.clone();
        env.insert(ENV_REPO.to_string(), unit.repo.full_name());
        env.insert(ENV_ISSUE_NUMBER.to_string(), unit.number.to_string());
        WireUnit {
            id: unit.number.to_string(),
            key: unit.key(),
            thread: unit.thread(),
            seen: unit.before_comments.iter().map(u64::to_string).collect(),
            me: unit.claimed_as.clone(),
            env,
            files: vec![
                WireFile {
                    path: TASK_FILE.to_string(),
                    text: brief_text(&unit.title, &unit.body, &unit.feedback),
                },
                WireFile {
                    path: format!("{ISSUE_DIR}/{NUMBER_FILE}"),
                    text: unit.number.to_string(),
                },
            ],
        }
    }
}

/// The `gitea` kind behind the plugin's seam: the inherent methods above, as they are.
impl Units for IssueUnits {
    type Unit = Unit;

    /// It marks no failed attempt — the built-in keeps the spine's no-op there — so
    /// `attempt_failed` is not among them.
    const CALLS: &'static [&'static str] = &["release", "renew", "comments", "classify"];

    fn resolve_me(&self) -> Result<String, GiteaError> {
        IssueUnits::resolve_me(self)
    }

    fn try_claim_next(
        &self,
        me: &str,
        diag: &dyn Diag,
        clock: &dyn Clock,
    ) -> Result<Option<Unit>, ClaimFault<GiteaError>> {
        IssueUnits::try_claim_next(self, me, diag, clock)
    }

    fn wire_unit(&self, unit: &Unit) -> WireUnit {
        IssueUnits::wire_unit(self, unit)
    }

    fn release(&self, unit: &Unit, diag: &dyn Diag) {
        IssueUnits::release(self, unit, diag);
    }

    fn release_stale(&self, key: &str, diag: &dyn Diag) -> Option<bool> {
        IssueUnits::release_stale(self, key, diag)
    }

    fn renew(&self, unit: &Unit, renewal: u64, diag: &dyn Diag) {
        IssueUnits::renew(self, unit, renewal, diag);
    }

    fn comments(&self, unit: &Unit) -> Result<Vec<IssueComment>, GiteaError> {
        IssueUnits::comments(self, unit)
    }

    fn classify(scratch: &Path, verdict: UnitOutcome) -> UnitOutcome {
        IssueUnits::classify(scratch, verdict)
    }

    fn finish(&self, unit: &Unit, outcome: UnitOutcome, facts: &Facts, diag: &dyn Diag) -> bool {
        IssueUnits::finish(self, unit, outcome, facts, diag)
    }
}

/// The tail decision for one candidate issue: whether it fires, and the replies to retain
/// for the brief — computed together so the fire rule and the delivered delta cannot
/// drift apart.
///
/// `gate` `None` is the awaiting-reply re-arm path: fire iff a human replied newer than
/// the bot's last word. `Some` is `discuss_with`: on **first sight** — afkd never
/// commented — the issue fires unconditionally; once afkd has spoken it fires iff some
/// comment after that boundary is from an allowed, non-self author. The **whole tail** is
/// scanned, so a disallowed author commenting after an allowed one cannot mask it.
fn tail_decision(
    comments: &[IssueComment],
    me: &str,
    gate: Option<&DiscussGate>,
) -> (bool, Vec<FeedbackItem>) {
    let replies = new_reply_comments(comments, me, |login| gate.is_none_or(|g| g.allows(login)));
    let fires = match gate {
        Some(_) => comment_watermark(comments, me).is_none() || !replies.is_empty(),
        None => !replies.is_empty(),
    };
    (fires, replies)
}

/// The backstop comment posted at the end of a silent `discuss_with` turn. It carries no
/// marker: afkd posts it, so it is author=self, which is what the tail boundary keys on.
fn backstop_text(outcome: UnitOutcome, facts: &Facts) -> String {
    match outcome {
        UnitOutcome::Clean => "reviewed, nothing to add".to_string(),
        UnitOutcome::Park => "awaiting a human reply".to_string(),
        UnitOutcome::Failed => match facts.fault() {
            Some(reason) => format!("run did not complete: {reason}"),
            None => "run did not complete".to_string(),
        },
    }
}

/// The brief written to `task.md`: the title, or the title, a blank line, then the body —
/// then, for a re-claimed (unparked) issue, the human replies that unparked it, each
/// attributed.
fn brief_text(title: &str, body: &str, feedback: &[FeedbackItem]) -> String {
    let mut s = if body.trim().is_empty() {
        title.to_string()
    } else {
        format!("{title}\n\n{body}")
    };
    s.push_str(&render_feedback_section("New comments", feedback));
    s
}

#[cfg(test)]
mod tests {
    //! No network: every test drives the in-memory `MockClient` and a fake clock.
    //!
    //! What is proven here is the **vendor half** — eligibility, the claim, the
    //! `discuss_with` tail gate, the clarification gate's park/backstop, the brief and
    //! the env. The drive around it (attempt counting, the claim journal, the run-name
    //! mint, the cadence, the framing of `task.md`) is afkd's, on the far side of the
    //! wire; the wire itself is `tests/wire.rs`.

    use super::*;
    use crate::claim::CLAIM_SETTLE;
    use crate::client::{Action, MockClient};
    use crate::common::{CaptureDiag, FakeClock, TempDir};
    use std::sync::Arc;

    fn cfg(
        repo: &str,
        on_claim: Vec<LifecycleAction>,
        on_done: Vec<LifecycleAction>,
        on_fail: Vec<LifecycleAction>,
    ) -> GiteaConfig {
        GiteaConfig {
            base_url: "https://gitea.example.com".into(),
            repo: repo.into(),
            org: String::new(),
            token: "PAT".into(),
            source_label: "afkd/ready".into(),
            author_me: false,
            on_claim,
            on_done,
            on_fail,
            on_park: Vec::new(),
            discuss_with: None,
        }
    }

    /// The kind over a shared [`MockClient`], with a capturing diagnostic sink and a fake
    /// clock — the spine's side of each call, played by hand: [`poll`](Self::poll) is the
    /// `poll` call, [`finish`](Self::finish) the `finish` call.
    struct Harness {
        client: Arc<MockClient>,
        units: IssueUnits,
        diag: CaptureDiag,
        clock: FakeClock,
    }

    impl Harness {
        fn new(cfg: GiteaConfig, me: &str) -> Self {
            let client = Arc::new(MockClient::new(me));
            let units = IssueUnits::new(Box::new(Arc::clone(&client)), &cfg);
            Self {
                client,
                units,
                diag: CaptureDiag::default(),
                clock: FakeClock::new(),
            }
        }

        /// One beat as afkd's spine drives it: a transient failure is diagnosed and the
        /// beat is idle; a fatal one is the service's crash sentence.
        fn poll(&self) -> Result<Option<Unit>, String> {
            match self.units.try_claim_next(&me(), &self.diag, &self.clock) {
                Ok(unit) => Ok(unit),
                Err(ClaimFault::Transient(e)) => {
                    self.diag.err(&e);
                    Ok(None)
                }
                Err(ClaimFault::Fatal(reason)) => Err(reason),
            }
        }

        /// The `finish` call for `unit`, ended `outcome` with `facts`.
        fn finish(&self, unit: &Unit, outcome: UnitOutcome, facts: &Facts) -> bool {
            self.units.finish(unit, outcome, facts, &self.diag)
        }

        /// Define the two afkd-managed labels in the repo, as a repository afkd has
        /// already claimed in once has them — for the tests that finish a unit
        /// **without** polling.
        fn defines_the_afkd_labels(&self) {
            self.client.register_label(CLAIMED_LABEL, false);
            self.client.register_label(AWAITING_LABEL, false);
        }
    }

    /// The `task.md` text a `poll` would hand over for `unit`.
    fn task_md(h: &Harness, unit: &Unit) -> String {
        let wire = h.units.wire_unit(unit);
        assert_eq!(wire.files[0].path, TASK_FILE);
        wire.files[0].text.clone()
    }

    /// The facts of a run that faulted with `reason`.
    fn fault(reason: &str) -> Facts {
        Facts {
            signal: "fault".into(),
            reason: Some(reason.into()),
            ..Facts::none()
        }
    }

    fn repo() -> Repo {
        Repo {
            owner: "acme".into(),
            name: "widgets".into(),
        }
    }

    /// The claim identity every test drives with.
    fn me() -> String {
        "me".to_string()
    }

    /// The second a claim race is staged in, frozen on the mock's post clock so a
    /// rival marker can be placed a known distance either side of our own.
    const T: u64 = 1_700_000_000;

    /// The `[afkd-claim]` marker bodies on issue `index`, read through the trait's
    /// own list call — the same thing the claim reads, so no test-only accessor can
    /// disagree with production about what is on the thread.
    fn claim_markers_on(h: &Harness, index: u64) -> Vec<String> {
        h.client
            .list_issue_comments(&repo(), index)
            .expect("read the thread")
            .into_iter()
            .filter(|c| is_claim(&c.body))
            .map(|c| c.body)
            .collect()
    }

    fn unit(number: u64) -> Unit {
        Unit {
            repo: repo(),
            number,
            claim_id: 1,
            title: "T".into(),
            body: "B".into(),
            feedback: Vec::new(),
            before_comments: Vec::new(),
            claimed_as: me(),
        }
    }

    // --- Eligibility + the claim ---

    #[test]
    fn only_open_source_labelled_unclaimed_issues_are_eligible() {
        let h = Harness::new(cfg("acme/widgets", vec![], vec![], vec![]), "me");
        // Eligible.
        h.client.add_issue(1, "Fix", "do it", &["afkd/ready"]);
        // Already claimed → never re-picked.
        h.client
            .add_issue(2, "Other", "", &["afkd/ready", "afkd/claimed"]);

        let unit = h.poll().expect("no fatal claim verdict").expect("claimed");
        assert_eq!(unit.number, 1);
        // The claimed issue (2) is never picked even on a second poll.
        assert!(h.poll().expect("no fatal claim verdict").is_none());
    }

    #[test]
    fn an_issue_assigned_to_the_bot_is_eligible_without_the_source_label() {
        // The seamless signal: a human assigns the bot's own user (no label), and
        // the issue is up for grabs. Here #1 carries no `afkd/ready` but is
        // assigned to `me`.
        let h = Harness::new(cfg("acme/widgets", vec![], vec![], vec![]), "me");
        h.client.add_issue_assigned(1, "Fix", "do it", &[], &["me"]);

        let unit = h
            .poll()
            .expect("no fatal claim verdict")
            .expect("claimed via assignment");
        assert_eq!(unit.number, 1);
    }

    #[test]
    fn without_a_source_label_an_unassigned_unlabelled_issue_is_not_picked() {
        // Guard against the empty-label trap: `list_issues(.., "")` returns every
        // open issue, so `candidates` must still filter. With no `source_label`,
        // only assignment (or the awaiting marker) makes an issue eligible — a bare
        // open issue is left alone.
        let mut c = cfg("acme/widgets", vec![], vec![], vec![]);
        c.source_label = String::new();
        let h = Harness::new(c, "me");
        // Neither assigned nor labelled → ignored.
        h.client.add_issue(1, "Bare", "not mine", &[]);
        // Assigned to the bot → the one that gets claimed.
        h.client
            .add_issue_assigned(2, "Mine", "do it", &[], &["me"]);

        let unit = h
            .poll()
            .expect("no fatal claim verdict")
            .expect("claimed the assigned one");
        assert_eq!(unit.number, 2);
    }

    #[test]
    fn claim_runs_on_claim_assign_and_label() {
        let h = Harness::new(
            cfg(
                "acme/widgets",
                vec![
                    LifecycleAction::AssignMe,
                    LifecycleAction::LabelAdd("afkd/claimed".into()),
                ],
                vec![],
                vec![],
            ),
            "me",
        );
        h.client.add_issue(1, "Fix", "do it", &["afkd/ready"]);

        h.poll().expect("no fatal claim verdict").expect("claimed");
        // `on_claim` assigned us and added the claimed label (recorded actions).
        assert!(h
            .client
            .actions()
            .iter()
            .any(|a| matches!(a, Action::Assign { assignees, .. } if assignees == &vec!["me".to_string()])));
        assert!(h
            .client
            .actions()
            .iter()
            .any(|a| matches!(a, Action::Label { name, .. } if name == "afkd/claimed")));
    }

    /// AC5 — the claim never removes a human's assignee. An issue a human already
    /// assigned to themselves, claimed with `on_claim { assign_me }`, ends up
    /// carrying **both** of them: the old replace-set write evicted `alice`.
    #[test]
    fn claiming_an_issue_a_human_is_assigned_to_keeps_them_assigned() {
        let h = Harness::new(
            cfg(
                "acme/widgets",
                vec![LifecycleAction::AssignMe],
                vec![],
                vec![],
            ),
            "me",
        );
        h.client
            .add_issue_assigned(1, "Fix", "do it", &["afkd/ready"], &["alice"]);

        h.poll().expect("no fatal claim verdict").expect("claimed");

        assert_eq!(
            h.client.assignees_of(1),
            vec!["alice".to_string(), "me".to_string()]
        );
    }

    /// The claim's status half is the kind's own: a won claim adds
    /// `afkd/claimed` (the re-pick gate) even when no `on_claim` block spells it —
    /// the gate cannot be disarmed by a config that names a different label.
    #[test]
    fn a_won_claim_labels_the_issue_even_with_an_empty_on_claim() {
        let h = Harness::new(cfg("acme/widgets", vec![], vec![], vec![]), "me");
        h.client.add_issue(1, "Fix", "do it", &["afkd/ready"]);

        h.poll().expect("no fatal claim verdict").expect("claimed");
        assert!(h.client.has_label(1, "afkd/claimed"));
    }

    // --- The claim label the re-pick gate rests on ---

    /// The claim markers this run posted, in order — the count that tells "claimed
    /// once" from "claimed once per poll".
    fn claims_posted(h: &Harness) -> Vec<String> {
        h.client
            .actions()
            .into_iter()
            .filter_map(|a| match a {
                Action::Comment { body, .. } if is_claim(&body) => Some(body),
                _ => None,
            })
            .collect()
    }

    /// AC1 — the wild bug: a repository that never defined `afkd/claimed`. Gitea
    /// drops a label name it does not know and answers success anyway, so the gate
    /// never landed; the finished run left the issue open and still eligible, and the
    /// very next poll claimed it again — four pull requests for one issue.
    ///
    /// The repo here holds exactly the label set the docs led that user to: their
    /// own `afkd/inprogress` and the documented `afkd/awaiting-reply` — no
    /// `afkd/claimed`. The plugin creates it (non-exclusive), the issue is claimed
    /// **once**, and the second poll finds nothing.
    #[test]
    fn an_undefined_claim_label_is_created_and_the_issue_claimed_once() {
        let h = Harness::new(cfg("acme/widgets", vec![], vec![], vec![]), "me");
        h.client.register_label("afkd/inprogress", false);
        h.client.register_label(AWAITING_LABEL, false);
        h.client.add_issue(1, "Fix", "do it", &["afkd/ready"]);

        let unit = h.poll().expect("no fatal claim verdict").expect("claimed");
        assert_eq!(unit.number, 1);
        h.finish(&unit, UnitOutcome::Clean, &Facts::none());

        // The gate is on the issue, and the label the plugin minted for it is a
        // plain one — an exclusive one would be stripped by the next label add.
        assert!(h.client.has_label(1, CLAIMED_LABEL), "the gate landed");
        assert_eq!(
            h.client
                .list_labels(&repo())
                .unwrap()
                .into_iter()
                .find(|l| l.name == CLAIMED_LABEL)
                .map(|l| l.exclusive),
            Some(false),
            "the created gate label is non-exclusive"
        );
        // The second poll is the whole point: the issue is still open and still
        // assigned, and it is not claimed again.
        assert!(
            h.poll().expect("no fatal claim verdict").is_none(),
            "the labelled issue is not re-picked"
        );
        assert_eq!(
            claims_posted(&h),
            vec![crate::claim::claim_text("me")],
            "one claim, not one per poll"
        );
    }

    /// AC2 — the exclusive trap sprung by the user's own config: their
    /// `on_claim { label_add "afkd/inprogress" }` names an **exclusive** label in
    /// the `afkd` scope, which strips every other exclusive label in that scope off
    /// the issue. The gate afkd created is not exclusive, so it survives — while the
    /// exclusive `afkd/triage` sitting beside it does not, which is what makes this
    /// assertion mean something.
    #[test]
    fn the_claim_label_survives_an_exclusive_on_claim_label() {
        let h = Harness::new(
            cfg(
                "acme/widgets",
                vec![LifecycleAction::LabelAdd("afkd/inprogress".into())],
                vec![],
                vec![],
            ),
            "me",
        );
        h.client.register_label("afkd/inprogress", true);
        h.client.register_label("afkd/triage", true);
        h.client.add_issue(1, "Fix", "do it", &["afkd/ready"]);
        h.client.add_label(&repo(), 1, "afkd/triage").unwrap();

        h.poll().expect("no fatal claim verdict").expect("claimed");

        assert!(h.client.has_label(1, "afkd/inprogress"), "on_claim ran");
        assert!(
            !h.client.has_label(1, "afkd/triage"),
            "the exclusive add really does strip its scope siblings"
        );
        assert!(
            h.client.has_label(1, CLAIMED_LABEL),
            "…and the non-exclusive gate is not one of them"
        );
    }

    /// AC3 — an `afkd/claimed` that already exists as an **exclusive** label cannot
    /// hold the gate (the next add in its scope would take it off again), and it is
    /// not afkd's to redefine. The poll refuses with a sentence naming the repo, the
    /// label and the exclusivity — and refuses *before* the claim, so nothing is
    /// posted, nothing is labelled, and no run starts.
    #[test]
    fn an_exclusive_claim_label_is_refused_and_starts_no_run() {
        let h = Harness::new(cfg("acme/widgets", vec![], vec![], vec![]), "me");
        h.client.register_label(CLAIMED_LABEL, true);
        h.client.add_issue(1, "Fix", "do it", &["afkd/ready"]);

        let Err(reason) = h.poll() else {
            panic!("an exclusive claim label must refuse the poll");
        };
        for needle in ["acme/widgets", CLAIMED_LABEL, "exclusive"] {
            assert!(reason.contains(needle), "{needle:?} missing from: {reason}");
        }
        assert!(
            h.client.actions().is_empty(),
            "the refusal precedes the claim: {:?}",
            h.client.actions()
        );
        assert!(claim_markers_on(&h, 1).is_empty());
    }

    /// AC4 — the two halves of the split, at the `poll` boundary. The same stage fails
    /// twice: a **definite** 403 on the label create is `Fatal`, naming repo and label
    /// (the plugin exits with it, and afkd faults the service), while a **transient**
    /// transport blip on the very same call is `Transient` — an idle beat, diagnosed.
    #[test]
    fn a_definite_label_failure_is_fatal_but_a_transient_one_is_not() {
        // (a) definite: the forge answered, and the answer will not change on the
        // next beat.
        let h = Harness::new(cfg("acme/widgets", vec![], vec![], vec![]), "me");
        h.client.add_issue(1, "Fix", "do it", &["afkd/ready"]);
        h.client.refuse("create label", 403);

        let outcome = h.units.try_claim_next(&me(), &h.diag, &h.clock);
        let Err(ClaimFault::Fatal(reason)) = outcome else {
            panic!("a refused label create must fail the service, got {outcome:?}");
        };
        for needle in ["acme/widgets", CLAIMED_LABEL, "403"] {
            assert!(reason.contains(needle), "{needle:?} missing from: {reason}");
        }
        assert!(
            h.client.actions().is_empty(),
            "the crash claimed nothing: {:?}",
            h.client.actions()
        );

        // (b) transient: a blip on the same call keeps the service alive, reported.
        let h = Harness::new(cfg("acme/widgets", vec![], vec![], vec![]), "me");
        h.client.add_issue(1, "Fix", "do it", &["afkd/ready"]);
        h.client.fail("create label");

        let outcome = h.units.try_claim_next(&me(), &h.diag, &h.clock);
        assert!(
            matches!(
                outcome,
                Err(ClaimFault::Transient(GiteaError::Transport { .. }))
            ),
            "a transport failure is not a crash: {outcome:?}"
        );
    }

    /// AC4 — the definite case that strikes **after** the claim is won: the repo
    /// defines the label, the marker is posted and the race won, and only then does
    /// the forge refuse the write. The service still fails (an unlabelled issue would
    /// be claimed again on every poll), and the claim marker is released on the way
    /// out — a crash must not leave one blocking the retry a human's fix enables for
    /// a whole claim lifetime.
    #[test]
    fn a_definite_claim_label_refusal_crashes_and_releases_the_marker() {
        let h = Harness::new(cfg("acme/widgets", vec![], vec![], vec![]), "me");
        h.defines_the_afkd_labels();
        h.client.add_issue(1, "Fix", "do it", &["afkd/ready"]);
        h.client.refuse("add label", 403);

        let Err(reason) = h.poll() else {
            panic!("a refused claim label must fail the service");
        };
        for needle in ["acme/widgets#1", CLAIMED_LABEL, "403"] {
            assert!(reason.contains(needle), "{needle:?} missing from: {reason}");
        }
        assert!(
            claim_markers_on(&h, 1).is_empty(),
            "the fatal verdict released its marker: {:?}",
            claim_markers_on(&h, 1)
        );
    }

    /// AC5 — a user's `label_add` for a name Gitea would drop is an **error**, not a
    /// success, and per `apply_actions` it abandons the rest of that moment: the
    /// `assign_me` behind it never runs, and the failure is reported. The claim
    /// itself stands (a lifecycle failure never rolls it back), gate and all.
    #[test]
    fn a_label_add_gitea_would_drop_is_an_error_and_abandons_the_moment() {
        let h = Harness::new(
            cfg(
                "acme/widgets",
                vec![
                    LifecycleAction::LabelAdd("afkd/working".into()),
                    LifecycleAction::AssignMe,
                ],
                vec![],
                vec![],
            ),
            "me",
        );
        // `afkd/working` is what the docs' own example spells — and this repository
        // never created it.
        h.client.add_issue(1, "Fix", "do it", &["afkd/ready"]);

        let unit = h
            .poll()
            .expect("no fatal claim verdict")
            .expect("a user's lifecycle failure does not undo the claim");
        assert_eq!(unit.number, 1);
        assert!(!h.client.has_label(1, "afkd/working"), "nothing landed");
        assert!(
            h.client.assignees_of(1).is_empty(),
            "the action after the failing one was abandoned"
        );
        assert!(
            h.diag.lines().iter().any(|e| e.contains("afkd/working")),
            "the drop was surfaced by name: {:?}",
            h.diag.lines()
        );
        assert!(
            h.client.has_label(1, CLAIMED_LABEL),
            "the gate still landed"
        );
    }

    /// AC6 — a park that cannot place `afkd/awaiting-reply` is **surfaced**, not
    /// silently parked. The re-arm gate reads that label, so an issue that quietly
    /// lost it would sit unclaimed forever with nobody the wiser; here the label is
    /// absent, the claim is not dropped, and the failure names the label.
    #[test]
    fn a_park_that_cannot_label_is_surfaced() {
        let h = Harness::new(cfg("acme/widgets", vec![], vec![], vec![]), "me");
        // The claim gate is defined; the awaiting label deliberately is not (a human
        // deleted it after the claim, say).
        h.client.register_label(CLAIMED_LABEL, false);
        h.client
            .add_issue(1, "Fix", "do it", &["afkd/ready", "afkd/claimed"]);

        h.finish(&unit(1), UnitOutcome::Park, &Facts::none());

        assert!(
            !h.client.has_label(1, AWAITING_LABEL),
            "the label really did not land"
        );
        assert!(
            h.client.has_label(1, CLAIMED_LABEL),
            "and the park stopped at the first failure rather than half-applying"
        );
        assert!(
            h.diag.lines().iter().any(|e| e.contains(AWAITING_LABEL)),
            "the failed park was reported by name: {:?}",
            h.diag.lines()
        );
    }

    #[test]
    fn lost_claim_race_releases_the_marker_and_skips() {
        let h = Harness::new(cfg("acme/widgets", vec![], vec![], vec![]), "me");
        h.client.add_issue(1, "Fix", "do it", &["afkd/ready"]);
        // A rival's marker lands between our post and our re-read, one second ahead
        // of ours in the order and well inside the claim lifetime.
        h.client.set_clock(T);
        h.client.rival_claims_next("rival", 42, T - 1);

        assert!(h.poll().expect("no fatal claim verdict").is_none());
        // The status label was never applied, and our marker went with the loss.
        assert!(!h.client.has_label(1, "afkd/claimed"));
        assert_eq!(
            claim_markers_on(&h, 1),
            vec![crate::claim::claim_text("rival")],
            "only the winner's marker is left"
        );
    }

    // Regression: two fires worked one issue and each opened a pull request. A human
    // took `afkd/claimed` off mid-run, and the holder's marker was 68 minutes old —
    // past `CLAIM_LIFETIME` — so the second fire read it as stale and won a unit
    // somebody was actively working. The marker is now renewed for the life of the
    // run, so the second fire loses the race instead.

    #[test]
    fn a_renewed_rival_marker_loses_us_the_claim_with_no_label() {
        let h = Harness::new(cfg("acme/widgets", vec![], vec![], vec![]), "me");
        // The incident's issue: open, source-labelled, and with **no**
        // `afkd/claimed` — a human took it off — so it is fully eligible.
        h.client
            .add_issue(1048, "Fix", "the retry storm", &["afkd/ready"]);
        // The holder's marker: posted eleven hours ago, renewed a minute ago.
        h.client.add_comment_edited(
            1048,
            42,
            "autocoder",
            &crate::claim::claim_renewal_text("autocoder", 132),
            T - 40_000,
            T - 60,
        );
        h.client.set_clock(T);

        let issue = h
            .client
            .list_issues(&repo(), "open", "")
            .expect("read the issues")
            .into_iter()
            .find(|i| i.number == 1048)
            .expect("the seeded issue");
        assert!(
            h.units.eligible(&issue),
            "the label gate is open — that is the whole premise"
        );

        assert!(
            h.poll().expect("no fatal claim verdict").is_none(),
            "we claimed an issue a live run is holding"
        );

        // The holder's marker is the only one left: ours was posted and taken back.
        assert_eq!(
            claim_markers_on(&h, 1048),
            vec![crate::claim::claim_renewal_text("autocoder", 132)],
            "only the holder's marker is left"
        );
        assert!(
            !h.client.has_label(1048, CLAIMED_LABEL),
            "nothing was claimed"
        );
        let posted: Vec<u64> = h
            .client
            .actions()
            .into_iter()
            .filter_map(|a| match a {
                Action::Comment { .. } => Some(0),
                Action::DeleteComment { id } => Some(id),
                _ => None,
            })
            .collect();
        assert_eq!(
            posted.len(),
            2,
            "exactly one marker posted and one deleted: {:?}",
            h.client.actions()
        );
        assert_eq!(posted[0], 0, "the post came first");
        assert!(posted[1] >= 1_000_000, "the delete named our own marker");
    }

    /// The renewal itself, over the real `renew_marker`: the claim comment is edited
    /// **in place** — same id, a body that still reads as a claim — and the forge's
    /// `updated_at` moves with it, which is the whole liveness signal.
    #[test]
    fn renewing_a_claim_edits_the_marker_in_place() {
        let h = Harness::new(cfg("acme/widgets", vec![], vec![], vec![]), "me");
        h.client.add_issue(1, "Fix", "do it", &["afkd/ready"]);
        h.client
            .add_comment_body(1, 7, "me", &crate::claim::claim_text("me"), 100);
        let mut unit = unit(1);
        unit.claim_id = 7;

        h.units.renew(&unit, 3, &h.diag);

        let renewed = crate::claim::claim_renewal_text("me", 3);
        assert_eq!(
            h.client.actions(),
            vec![Action::EditComment {
                id: 7,
                body: renewed.clone(),
            }],
            "the marker was not edited by its own id"
        );
        let thread = h
            .client
            .list_issue_comments(&repo(), 1)
            .expect("read the thread");
        assert_eq!(thread.len(), 1, "a renewal minted a second comment");
        assert_eq!(thread[0].id, 7, "the comment id moved");
        assert_eq!(thread[0].body, renewed);
        assert!(is_claim(&thread[0].body), "a renewal stopped being a claim");
        assert!(
            thread[0].updated_at > thread[0].created_at,
            "the liveness half did not move"
        );
    }

    /// A renewal the forge refuses is one diagnostic line and nothing else: the hook
    /// returns normally, the marker is untouched, and afkd's run carries on.
    #[test]
    fn a_failing_renewal_raises_one_diagnostic_and_nothing_else() {
        let h = Harness::new(cfg("acme/widgets", vec![], vec![], vec![]), "me");
        h.client.add_issue(1, "Fix", "do it", &["afkd/ready"]);
        let claim = crate::claim::claim_text("me");
        h.client.add_comment_body(1, 7, "me", &claim, 100);
        h.client.fail("edit comment");
        let mut unit = unit(1);
        unit.claim_id = 7;

        h.units.renew(&unit, 1, &h.diag);

        let lines = h.diag.lines();
        assert_eq!(lines.len(), 1, "not exactly one diagnostic: {lines:?}");
        assert!(
            lines[0].contains("edit comment"),
            "the diagnostic does not name the stage: {lines:?}"
        );
        let thread = h
            .client
            .list_issue_comments(&repo(), 1)
            .expect("read the thread");
        assert_eq!(thread[0].body, claim, "a failed renewal changed the marker");
    }

    /// The `Lost` arm continues the candidate loop rather than ending the poll: the
    /// rival takes the first issue, so the *second* eligible one is claimed — and
    /// the first is left with neither our marker nor the claimed label.
    #[test]
    fn a_lost_claim_moves_on_to_the_next_candidate() {
        let h = Harness::new(cfg("acme/widgets", vec![], vec![], vec![]), "me");
        h.client.add_issue(1, "Fix", "do it", &["afkd/ready"]);
        h.client.add_issue(2, "Other", "do that", &["afkd/ready"]);
        // Armed for the next comment read, which is issue #1's claim re-read.
        h.client.set_clock(T);
        h.client.rival_claims_next("rival", 42, T - 1);

        let unit = h
            .poll()
            .expect("no fatal claim verdict")
            .expect("the second issue is claimed");
        assert_eq!(unit.number, 2);
        assert_eq!(
            h.clock.sleeps(),
            [CLAIM_SETTLE; 2],
            "each claim attempt settled once"
        );
        assert_eq!(
            claim_markers_on(&h, 1),
            vec![crate::claim::claim_text("rival")],
            "issue #1 keeps only the rival's marker"
        );
        assert!(!h.client.has_label(1, "afkd/claimed"), "#1 was left alone");
        assert!(h.client.has_label(2, "afkd/claimed"));
    }

    /// AC7 — the journal key carries the claim, the session thread does not: two
    /// successive claims of the same issue give two different `unit_key`s (each
    /// naming its own marker) and one identical `unit_thread`, so an ADR-0067 agent
    /// session resumes per issue rather than restarting on every claim.
    #[test]
    fn the_journal_key_carries_the_claim_but_the_thread_does_not() {
        let h = Harness::new(cfg("acme/widgets", vec![], vec![], vec![]), "me");
        h.client.add_issue(7, "Fix", "do it", &["afkd/ready"]);

        let first = h.poll().expect("no fatal claim verdict").expect("claimed");
        // Wind the claim down as a finished run does, so the issue is claimable
        // again (the marker released, the status label dropped).
        h.finish(&first, UnitOutcome::Clean, &Facts::none());
        let claimed = h
            .client
            .list_labels(&repo())
            .unwrap()
            .into_iter()
            .find(|l| l.name == CLAIMED_LABEL)
            .expect("the claim created its status label");
        h.client
            .remove_label(&repo(), 7, &claimed.id.to_string())
            .unwrap();
        let second = h
            .poll()
            .expect("no fatal claim verdict")
            .expect("re-claimed");

        let (key_a, key_b) = (first.key(), second.key());
        assert_ne!(key_a, key_b, "a fresh claim is a fresh journal key");
        assert_eq!(
            first.thread(),
            second.thread(),
            "the session thread is the issue, not the claim"
        );
        assert_eq!(first.thread(), "acme/widgets#7");
        // Each key round-trips to the coordinate + the marker the reaper deletes.
        for (key, unit) in [(&key_a, &first), (&key_b, &second)] {
            assert_eq!(
                crate::claim::split_claim_key(key),
                Some(("acme/widgets", 7, unit.claim_id))
            );
        }
    }

    /// AC4 — a finished unit leaves no marker: the run-end release runs whatever the
    /// terminal moment did, so the next claim of this issue is not out-ordered by a
    /// marker nobody is acting on.
    #[test]
    fn a_finished_unit_leaves_no_claim_marker() {
        let h = Harness::new(
            cfg("acme/widgets", vec![], vec![LifecycleAction::Close], vec![]),
            "me",
        );
        h.client.add_issue(1, "Fix", "do it", &["afkd/ready"]);

        let unit = h.poll().expect("no fatal claim verdict").expect("claimed");
        assert_eq!(claim_markers_on(&h, 1).len(), 1, "the claim is held");

        h.finish(&unit, UnitOutcome::Clean, &Facts::none());
        assert!(
            claim_markers_on(&h, 1).is_empty(),
            "the finished run released its marker: {:?}",
            claim_markers_on(&h, 1)
        );
    }

    #[test]
    fn org_wide_lists_repos_and_claims_the_eligible_issue() {
        let mut config = cfg("", vec![], vec![], vec![]);
        config.org = "acme".into();
        let h = Harness::new(config, "me");
        h.client.set_org_repos(
            "acme",
            &[
                Repo {
                    owner: "acme".into(),
                    name: "widgets".into(),
                },
                Repo {
                    owner: "acme".into(),
                    name: "gadgets".into(),
                },
            ],
        );
        h.client.add_issue(5, "Fix", "do it", &["afkd/ready"]);

        let unit = h.poll().expect("no fatal claim verdict").expect("claimed");
        assert_eq!(unit.number, 5);
        // The unit is keyed under the first org repo it was found in.
        assert_eq!(unit.repo.name, "widgets");
    }

    // --- Lifecycle on terminal outcomes (the `finish` call) ---

    #[test]
    fn clean_run_applies_on_done_close() {
        let h = Harness::new(
            cfg("acme/widgets", vec![], vec![LifecycleAction::Close], vec![]),
            "me",
        );
        h.client.add_issue(1, "Fix", "do it", &["afkd/ready"]);
        h.finish(&unit(1), UnitOutcome::Clean, &Facts::none());

        assert!(h
            .client
            .actions()
            .iter()
            .any(|a| matches!(a, Action::State { state, .. } if state == "closed")));
    }

    #[test]
    fn failed_run_applies_on_fail_label_remove_by_id_and_unassign() {
        let h = Harness::new(
            cfg(
                "acme/widgets",
                vec![],
                vec![],
                vec![
                    LifecycleAction::LabelRemove("afkd/claimed".into()),
                    LifecycleAction::Unassign,
                ],
            ),
            "me",
        );
        h.client
            .add_issue_assigned(1, "Fix", "do it", &["afkd/ready", "afkd/claimed"], &["me"]);
        h.finish(&unit(1), UnitOutcome::Failed, &fault("boom"));

        // on_fail removed the claimed label BY ID (not the all-clearing path) and
        // released afkd's own assignment (here the only one, so the set empties).
        assert!(h
            .client
            .actions()
            .iter()
            .any(|a| matches!(a, Action::Unlabel { .. })));
        assert!(h
            .client
            .actions()
            .iter()
            .any(|a| matches!(a, Action::Assign { assignees, .. } if assignees.is_empty())));
        assert!(!h.client.has_label(1, "afkd/claimed"));
    }

    // --- The clarification gate (the `park` marker → park → re-arm on reply) ---

    #[test]
    fn on_park_extras_run_after_the_park() {
        let mut config = cfg("acme/widgets", vec![], vec![], vec![]);
        config.on_park = vec![LifecycleAction::LabelAdd("needs-triage".into())];
        let h = Harness::new(config, "me");
        h.defines_the_afkd_labels();
        // The user's own extra is a label the repository defines — an undefined one
        // is an error now, which is `a_label_add_gitea_would_drop_is_an_error`.
        h.client.register_label("needs-triage", false);
        h.client
            .add_issue(1, "Fix", "do it", &["afkd/ready", "afkd/claimed"]);
        h.finish(&unit(1), UnitOutcome::Park, &Facts::none());

        // The plugin-managed awaiting label AND the user's extra both landed.
        assert!(h.client.has_label(1, "afkd/awaiting-reply"));
        assert!(h.client.has_label(1, "needs-triage"));
    }

    #[test]
    fn a_park_delivers_only_when_both_halves_land() {
        // The delivery verdict (ADR-0059) for the one terminal arm with real
        // composition. A park is two writes — the plugin-managed label swap that the
        // re-arm gate reads, and the user's `on_park` extras — and the claim is
        // finished only if BOTH reached the forge: a park whose label swap failed
        // leaves `afkd/claimed` standing, so the issue is neither parked nor re-picked.
        //
        // The two halves also have to be *evaluated* independently. A naive
        // `park_issue(…).is_ok() && self.apply(…)` short-circuits, silently dropping
        // the user's extras whenever the park failed; the third leg pins that they run
        // anyway.
        let park_harness = || {
            let mut config = cfg("acme/widgets", vec![], vec![], vec![]);
            config.on_park = vec![LifecycleAction::Comment(
                "parked — 看起来 the token expiry needs a human call 🙏\n\n\
                 which of the two backoffs should it take?"
                    .into(),
            )];
            let h = Harness::new(config, "me");
            h.defines_the_afkd_labels();
            h.client
                .add_issue(1, "Fix", "do it", &["afkd/ready", "afkd/claimed"]);
            h
        };
        let park = |h: &Harness| h.finish(&unit(1), UnitOutcome::Park, &Facts::none());
        let commented = |h: &Harness| {
            h.client
                .list_issue_comments(&repo(), 1)
                .expect("read the thread")
                .iter()
                .any(|c| c.body.contains("which of the two backoffs"))
        };

        // Both halves land ⇒ delivered.
        let clean = park_harness();
        assert!(park(&clean), "a whole park is delivered");
        assert!(clean.client.has_label(1, AWAITING_LABEL));
        assert!(commented(&clean));

        // The extras cannot post ⇒ undelivered, even though the park itself landed.
        let no_extras = park_harness();
        no_extras.client.fail("post comment");
        assert!(!park(&no_extras), "a half-applied park is not delivered");
        assert!(no_extras.client.has_label(1, AWAITING_LABEL));

        // The park's own first call fails ⇒ undelivered, `afkd/claimed` still
        // standing — AND the extras still ran, which is the short-circuit trap.
        let no_park = park_harness();
        no_park.client.fail("add label");
        assert!(!park(&no_park), "an unparked issue is not delivered");
        assert!(!no_park.client.has_label(1, AWAITING_LABEL));
        assert!(
            no_park.client.has_label(1, CLAIMED_LABEL),
            "the failed swap left the re-pick guard on the issue",
        );
        assert!(
            commented(&no_park),
            "the extras must not be short-circuited away by the failed park",
        );
    }

    #[test]
    fn a_parked_issue_is_reclaimed_only_after_a_human_replies() {
        let h = Harness::new(
            cfg(
                "acme/widgets",
                vec![
                    LifecycleAction::AssignMe,
                    LifecycleAction::LabelAdd("afkd/claimed".into()),
                ],
                vec![],
                vec![],
            ),
            "me",
        );
        // A parked issue: source + awaiting labels; the bot's question is the last word.
        h.client
            .add_issue(1, "Fix", "do it", &["afkd/ready", "afkd/awaiting-reply"]);
        h.client.add_comment(1, 10, "me", 100);

        // No human reply yet → skipped.
        assert!(h.poll().expect("no fatal claim verdict").is_none());

        // A human replies after the question → re-claimable.
        h.client.add_comment(1, 11, "human", 200);
        let unit = h
            .poll()
            .expect("no fatal claim verdict")
            .expect("re-claimed");
        assert_eq!(unit.number, 1);
        // The awaiting marker is cleared on re-claim and the claim re-applied.
        assert!(!h.client.has_label(1, "afkd/awaiting-reply"));
        assert!(h.client.has_label(1, "afkd/claimed"));
        // The reply that unparked it survives the claim: who answered, and what
        // they said — the bool this gate used to be delivered neither.
        assert_eq!(
            unit.feedback,
            vec![FeedbackItem {
                author: "human".into(),
                body: "comment 11".into(),
            }]
        );
    }

    /// AC3's round-trip end-to-end: a parked issue, two humans answering with real
    /// prose (multi-line, non-ASCII, a code block), re-claimed and re-briefed. The
    /// regenerated `task.md` carries the standing title/body *and* a `## New
    /// comments` section naming each replier — the answer to the parked question,
    /// not a re-derivation of it.
    #[test]
    fn an_unparked_issues_brief_carries_the_replies_and_their_authors() {
        let h = Harness::new(
            cfg(
                "acme/widgets",
                vec![LifecycleAction::LabelAdd("afkd/claimed".into())],
                vec![],
                vec![],
            ),
            "me",
        );
        h.client.add_issue(
            1,
            "Retry storm on token expiry",
            "The client retries forever once the token expires.",
            &["afkd/ready", "afkd/awaiting-reply"],
        );
        // The bot's parking question is the last word until the humans answer.
        h.client
            .add_comment_body(1, 10, "me", "Which backoff should I use?", 100);
        h.client.add_comment_body(
            1,
            12,
            "bob",
            "…and cap it at 30s.\n\n    max_backoff = 30\n",
            300,
        );
        h.client.add_comment_body(
            1,
            11,
            "álvaro",
            "Exponential, please — see §4 of the RFC 🙏",
            200,
        );

        let unit = h
            .poll()
            .expect("no fatal claim verdict")
            .expect("re-claimed");
        let brief = task_md(&h, &unit);
        assert_eq!(
            brief,
            "Retry storm on token expiry\n\n\
             The client retries forever once the token expires.\n\n## New comments\n\
             \n**álvaro:** Exponential, please — see §4 of the RFC 🙏\n\
             \n**bob:** …and cap it at 30s.\n\n    max_backoff = 30\n"
        );
        // The bot's own parking question is not fed back to it as new input.
        assert!(!brief.contains("Which backoff should I use?"), "{brief}");
    }

    /// The other half: a **first** claim reads no comments at all, so its brief is
    /// byte-identical to the pre-attribution title/body shape — no empty heading.
    #[test]
    fn a_first_claims_brief_gains_no_comments_section() {
        let h = Harness::new(
            cfg(
                "acme/widgets",
                vec![LifecycleAction::LabelAdd("afkd/claimed".into())],
                vec![],
                vec![],
            ),
            "me",
        );
        h.client
            .add_issue(1, "Fix the flag", "It is spelled wrong.", &["afkd/ready"]);
        // A human comment exists, but a non-parked issue never reads them.
        h.client.add_comment_body(1, 10, "alice", "bump", 100);

        let unit = h.poll().expect("no fatal claim verdict").expect("claimed");
        assert!(unit.feedback.is_empty());
        let brief = task_md(&h, &unit);
        assert_eq!(brief, "Fix the flag\n\nIt is spelled wrong.");
    }

    #[test]
    fn a_parked_issue_is_found_for_rearm_without_the_source_label() {
        // Regression: a parked issue that lost the source label (dropped on claim, or
        // by a human) is still discovered via `afkd/awaiting-reply` and re-claimed
        // once a human replies. Before the fix it was invisible to the poll (the
        // trigger only listed source-labelled issues) and re-arm never fired.
        let h = Harness::new(
            cfg(
                "acme/widgets",
                vec![LifecycleAction::LabelAdd("afkd/claimed".into())],
                vec![],
                vec![],
            ),
            "me",
        );
        // ONLY the awaiting label — no `afkd/ready`.
        h.client
            .add_issue(1, "Fix", "do it", &["afkd/awaiting-reply"]);
        h.client.add_comment(1, 10, "me", 100); // the bot's question

        // Bot's question is the last word → still skipped.
        assert!(h.poll().expect("no fatal claim verdict").is_none());

        // Human replies → found via the awaiting-reply query and re-claimed.
        h.client.add_comment(1, 11, "human", 200);
        let unit = h
            .poll()
            .expect("no fatal claim verdict")
            .expect("re-claimed without source label");
        assert_eq!(unit.number, 1);
        assert!(!h.client.has_label(1, "afkd/awaiting-reply"));
    }

    // --- The `discuss_with` tail gate (the grooming loop) ---

    /// The base config with the tail gate set, and the claim's own label add as
    /// `on_claim` so a claim is visible in the recorded actions.
    fn discuss_cfg(dw: DiscussWith) -> GiteaConfig {
        GiteaConfig {
            discuss_with: Some(dw),
            ..cfg("acme/widgets", vec![], vec![], vec![])
        }
    }

    /// A real reply: multi-line, non-ASCII, and carrying an indented code block —
    /// what the retained delta actually renders into the brief.
    const REPLY: &str = "Exponential, please — see §4 of the RFC 🙏\n\n    max_backoff = 30\n";

    /// The bot's own last word, the tail boundary every re-fire is measured from.
    const ASKED: &str = "Which backoff should I use?";

    #[test]
    fn discuss_with_fires_on_first_sight() {
        // afkd has never commented, so assignment IS the opt-in: the issue fires
        // unconditionally — with no comments at all…
        let h = Harness::new(discuss_cfg(DiscussWith::Anyone), "me");
        h.client.add_issue_assigned(
            1,
            "Retry storm on token expiry",
            "It retries forever.",
            &[],
            &["me"],
        );
        let unit = h
            .poll()
            .expect("no fatal claim verdict")
            .expect("first sight fires");
        assert_eq!(unit.number, 1);
        assert!(unit.feedback.is_empty(), "no comments, no delta");

        // …and even when the only comment so far is from an author the allow-list
        // does not accept (the allow-list gates re-firing, not first sight).
        let h = Harness::new(discuss_cfg(DiscussWith::Logins(vec!["alice".into()])), "me");
        h.client
            .add_issue_assigned(2, "Flag is misspelled", "", &[], &["me"]);
        h.client.add_comment_body(2, 20, "mallory", "bump", 100);
        let unit = h
            .poll()
            .expect("no fatal claim verdict")
            .expect("first sight fires regardless");
        assert_eq!(unit.number, 2);
        assert!(
            unit.feedback.is_empty(),
            "the disallowed author is not briefed either: {:?}",
            unit.feedback
        );
    }

    #[test]
    fn discuss_with_does_not_refire_when_the_bot_spoke_last() {
        // The wild bug: an assigned, NOT parked issue whose last word is the bot's.
        // Today's ungated path re-claims it every poll and the bot answers itself.
        let seed = |h: &Harness| {
            h.client.add_issue_assigned(
                1,
                "Retry storm on token expiry",
                "It retries forever.",
                &[],
                &["me"],
            );
            h.client.add_comment_body(1, 10, "álvaro", "Any idea?", 100);
            h.client.add_comment_body(1, 11, "me", ASKED, 200);
        };

        let h = Harness::new(discuss_cfg(DiscussWith::Anyone), "me");
        seed(&h);
        assert!(
            h.poll().expect("no fatal claim verdict").is_none(),
            "afkd had the last word — it must not answer itself"
        );

        // The contrast that names the bug: the same fixture with the gate unset is
        // claimed on the spot, no last-speaker test at all.
        let h = Harness::new(cfg("acme/widgets", vec![], vec![], vec![]), "me");
        seed(&h);
        assert!(
            h.poll().expect("no fatal claim verdict").is_some(),
            "unset = today's behaviour"
        );
    }

    #[test]
    fn discuss_with_refires_on_an_allowed_reply() {
        // afkd asked; an allowed author answered after it. The issue re-fires, and
        // the answer — not a re-derivation of the question — is what the brief gets.
        let h = Harness::new(discuss_cfg(DiscussWith::Logins(vec!["alice".into()])), "me");
        h.client
            .add_issue_assigned(1, "Retry storm", "It retries forever.", &[], &["me"]);
        h.client.add_comment_body(1, 10, "me", ASKED, 100);
        h.client.add_comment_body(1, 11, "alice", REPLY, 200);

        let unit = h
            .poll()
            .expect("no fatal claim verdict")
            .expect("the allowed reply re-fires it");
        assert_eq!(
            unit.feedback,
            vec![FeedbackItem {
                author: "alice".into(),
                body: REPLY.into(),
            }]
        );
        // The claim landed, and afkd's own question is never fed back as new input.
        assert!(h.client.has_label(1, "afkd/claimed"));
    }

    #[test]
    fn discuss_with_ignores_a_disallowed_reply() {
        // A reply from outside the allow-list is not an answer: the issue stays quiet.
        let h = Harness::new(discuss_cfg(DiscussWith::Logins(vec!["alice".into()])), "me");
        h.client
            .add_issue_assigned(1, "Retry storm", "It retries forever.", &[], &["me"]);
        h.client.add_comment_body(1, 10, "me", ASKED, 100);
        h.client.add_comment_body(1, 11, "mallory", REPLY, 200);

        assert!(h.poll().expect("no fatal claim verdict").is_none());
        assert!(
            !h.client.has_label(1, "afkd/claimed"),
            "nothing was claimed"
        );
    }

    #[test]
    fn discuss_with_scans_the_whole_tail() {
        // The WHOLE tail is scanned, not just the newest comment: a disallowed
        // author commenting after an allowed one cannot mask the still-unanswered
        // allowed one — and only the allowed reply reaches the brief.
        let h = Harness::new(discuss_cfg(DiscussWith::Logins(vec!["alice".into()])), "me");
        h.client
            .add_issue_assigned(1, "Retry storm", "It retries forever.", &[], &["me"]);
        h.client.add_comment_body(1, 10, "me", ASKED, 100);
        h.client.add_comment_body(1, 11, "alice", REPLY, 200);
        h.client
            .add_comment_body(1, 12, "mallory", "+1 (drive-by)", 300);

        let unit = h
            .poll()
            .expect("no fatal claim verdict")
            .expect("the buried allowed reply fires");
        assert_eq!(
            unit.feedback,
            vec![FeedbackItem {
                author: "alice".into(),
                body: REPLY.into(),
            }],
            "the allow-list filters the delta, not only the fire decision"
        );
    }

    #[test]
    fn discuss_with_never_allows_the_bot_itself() {
        // Self is struck from the allow-list however it is named, so naming the bot
        // allows nobody — afkd must not answer itself. Its own comment is the tail
        // boundary, never a reply on it.
        let h = Harness::new(
            discuss_cfg(DiscussWith::Logins(vec!["me".into(), "alice".into()])),
            "me",
        );
        h.client
            .add_issue_assigned(1, "Retry storm", "It retries forever.", &[], &["me"]);
        h.client.add_comment_body(1, 10, "me", ASKED, 100);
        h.client
            .add_comment_body(1, 11, "me", "…still thinking about it.", 200);

        assert!(
            h.poll().expect("no fatal claim verdict").is_none(),
            "`me` allows nobody"
        );

        // The other named login still counts.
        h.client.add_comment_body(1, 12, "alice", REPLY, 300);
        assert!(h.poll().expect("no fatal claim verdict").is_some());
    }

    #[test]
    fn discuss_with_still_clears_the_awaiting_label_on_reclaim() {
        // The park mechanism is untouched by the gate: a parked issue whose question
        // an allowed author answered is re-claimed AND un-parked, so the awaiting
        // marker never survives into the next round.
        let h = Harness::new(discuss_cfg(DiscussWith::Logins(vec!["alice".into()])), "me");
        h.client.add_issue(
            1,
            "Retry storm",
            "It retries forever.",
            &["afkd/ready", "afkd/awaiting-reply"],
        );
        h.client.add_comment_body(1, 10, "me", ASKED, 100);
        h.client.add_comment_body(1, 11, "alice", REPLY, 200);

        h.poll()
            .expect("no fatal claim verdict")
            .expect("re-claimed");
        assert!(!h.client.has_label(1, "afkd/awaiting-reply"));
        assert!(h.client.has_label(1, "afkd/claimed"));
    }

    #[test]
    fn an_unset_discuss_with_scans_candidates_without_reading_comments() {
        // The default path's forge traffic is pinned: with the gate unset, the
        // **candidate scan** reads no comments at all — the one read a claimed
        // candidate costs is the claim's own re-read, which every claim pays.
        let h = Harness::new(cfg("acme/widgets", vec![], vec![], vec![]), "me");
        h.client
            .add_issue_assigned(1, "Fix the flag", "It is spelled wrong.", &[], &["me"]);
        h.client.add_comment_body(1, 10, "alice", REPLY, 100);

        assert!(h.poll().expect("no fatal claim verdict").is_some());
        assert_eq!(
            h.client.comment_reads(),
            1,
            "the claim's re-read, and nothing else"
        );

        // Not a vacuous counter: the awaiting-reply re-arm path reads the tail
        // *before* claiming, so it costs a second one.
        let h = Harness::new(cfg("acme/widgets", vec![], vec![], vec![]), "me");
        h.client.add_issue(
            2,
            "Fix the flag",
            "It is spelled wrong.",
            &["afkd/awaiting-reply"],
        );
        h.client.add_comment_body(2, 20, "alice", REPLY, 100);

        assert!(h.poll().expect("no fatal claim verdict").is_some());
        assert_eq!(h.client.comment_reads(), 2);

        // …and a candidate the scan *rejects* is never claimed, so it costs the tail
        // read alone (no marker is posted on an issue we then skip).
        let h = Harness::new(discuss_cfg(DiscussWith::Anyone), "me");
        h.client
            .add_issue_assigned(3, "Fix the flag", "", &[], &["me"]);
        h.client.add_comment_body(3, 30, "me", ASKED, 100);

        assert!(h.poll().expect("no fatal claim verdict").is_none());
        assert_eq!(h.client.comment_reads(), 1, "the tail read, no claim");
    }

    // --- The backstop: every gated turn ends with afkd as the last speaker ---

    /// The comment bodies afkd **said** on `index`, in order — the claim marker is
    /// excluded, since it is the lock's bookkeeping and not a word in the thread
    /// (the same exclusion the last-speaker diff makes).
    fn comments_posted(h: &Harness, index: u64) -> Vec<String> {
        h.client
            .actions()
            .iter()
            .filter_map(|a| match a {
                Action::Comment { index: i, body } if *i == index && !is_claim(body) => {
                    Some(body.clone())
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_silent_turn_posts_exactly_one_backstop() {
        // A turn where the agent posts nothing would leave the human's reply newest
        // and re-fire every poll. One terse backstop reclaims last-speaker.
        let h = Harness::new(discuss_cfg(DiscussWith::Anyone), "me");
        h.client
            .add_issue_assigned(1, "Retry storm", "It retries forever.", &[], &["me"]);
        h.client.add_comment_body(1, 10, "álvaro", REPLY, 100);

        let unit = h.poll().expect("no fatal claim verdict").expect("claimed");
        h.finish(&unit, UnitOutcome::Clean, &Facts::none());

        assert_eq!(comments_posted(&h, 1), vec!["reviewed, nothing to add"]);
        // And the issue is quiet on the next poll: afkd now has the last word.
        assert!(h.poll().expect("no fatal claim verdict").is_none());
    }

    /// AC6 — a claim marker is not afkd speaking, so a silent turn still backstops.
    ///
    /// The run-end release normally takes the marker away before the last-speaker
    /// diff reads the thread; here that delete **fails** (the realistic case the
    /// guard exists for), so the marker is still sitting there, self-authored and
    /// new since the claim-time snapshot. Without the marker exclusion it would read
    /// as afkd having spoken, the backstop would stand down, and the human's reply
    /// would stay newest — the re-fire loop the gate exists to stop.
    #[test]
    fn a_silent_turn_backstops_even_when_its_claim_marker_is_still_on_the_thread() {
        let h = Harness::new(discuss_cfg(DiscussWith::Anyone), "me");
        h.client
            .add_issue_assigned(1, "Retry storm", "It retries forever.", &[], &["me"]);
        h.client.add_comment_body(1, 10, "álvaro", REPLY, 100);

        let unit = h.poll().expect("no fatal claim verdict").expect("claimed");
        h.client.fail("delete comment");
        h.finish(&unit, UnitOutcome::Clean, &Facts::none());

        assert_eq!(
            claim_markers_on(&h, 1).len(),
            1,
            "the release failed, so the marker is still there"
        );
        assert_eq!(comments_posted(&h, 1), vec!["reviewed, nothing to add"]);
    }

    /// AC6 — the tail gate reads exactly as it does today with markers present. Both
    /// halves over one fixture: an answered issue whose only word after the bot's is
    /// a **rival's** marker stays quiet (the marker is not a reply), and first sight
    /// with a marker already on the thread still fires (the marker is not afkd's
    /// word either).
    #[test]
    fn a_claim_marker_changes_neither_half_of_the_tail_gate() {
        let h = Harness::new(discuss_cfg(DiscussWith::Anyone), "me");
        h.client
            .add_issue_assigned(1, "Retry storm", "It retries forever.", &[], &["me"]);
        h.client.add_comment_body(1, 10, "álvaro", REPLY, 100);
        h.client.add_comment_body(1, 11, "me", ASKED, 200);
        h.client.add_comment_body(
            1,
            12,
            "björn-öst[bot]",
            &crate::claim::claim_text("björn-öst[bot]"),
            300,
        );

        assert!(
            h.poll().expect("no fatal claim verdict").is_none(),
            "a rival's marker is not the reply that re-fires an answered issue"
        );

        // First sight, with a rival's marker already sitting on the thread: the
        // issue still fires, and the marker is not briefed back as feedback.
        let h = Harness::new(discuss_cfg(DiscussWith::Anyone), "me");
        h.client
            .add_issue_assigned(2, "Flag is misspelled", "", &[], &["me"]);
        h.client.add_comment_body(
            2,
            20,
            "björn-öst[bot]",
            &crate::claim::claim_text("björn-öst[bot]"),
            100,
        );
        h.client.add_comment_body(2, 21, "álvaro", REPLY, 200);

        let unit = h
            .poll()
            .expect("no fatal claim verdict")
            .expect("first sight still fires");
        assert_eq!(
            unit.feedback,
            vec![FeedbackItem {
                author: "álvaro".into(),
                body: REPLY.into(),
            }],
            "the marker is not conversation: {:?}",
            unit.feedback
        );
    }

    #[test]
    fn a_speaking_turn_posts_no_backstop() {
        // The diff really keys on a NEW self-authored comment: a lifecycle `comment`
        // is afkd speaking, so the backstop stands down and the turn posts once.
        let config = GiteaConfig {
            on_done: vec![LifecycleAction::Comment("Fixed in @{run:duration}.".into())],
            ..discuss_cfg(DiscussWith::Anyone)
        };
        let h = Harness::new(config, "me");
        h.client
            .add_issue_assigned(1, "Retry storm", "It retries forever.", &[], &["me"]);
        h.client.add_comment_body(1, 10, "álvaro", REPLY, 100);

        let unit = h.poll().expect("no fatal claim verdict").expect("claimed");
        h.finish(&unit, UnitOutcome::Clean, &Facts::none());

        assert_eq!(comments_posted(&h, 1), vec!["Fixed in 0ms."]);
    }

    #[test]
    fn a_silent_park_posts_a_backstop() {
        // A park that asked nothing must still leave afkd as the last speaker, or the
        // awaiting issue re-fires every poll. The park itself is unchanged.
        let h = Harness::new(discuss_cfg(DiscussWith::Anyone), "me");
        h.client
            .add_issue_assigned(1, "Retry storm", "It retries forever.", &[], &["me"]);
        h.client.add_comment_body(1, 10, "álvaro", REPLY, 100);

        let unit = h.poll().expect("no fatal claim verdict").expect("claimed");
        h.finish(&unit, UnitOutcome::Park, &Facts::none());

        assert!(h.client.has_label(1, "afkd/awaiting-reply"), "still parked");
        assert_eq!(comments_posted(&h, 1), vec!["awaiting a human reply"]);
    }

    #[test]
    fn an_exhausted_turn_backstops_with_the_fault_reason() {
        // A failing turn is the loop the gate exists to stop: without a last word the
        // issue re-fires forever. The backstop states why the run did not finish.
        let h = Harness::new(discuss_cfg(DiscussWith::Anyone), "me");
        h.client
            .add_issue_assigned(1, "Retry storm", "It retries forever.", &[], &["me"]);
        h.client.add_comment_body(1, 10, "álvaro", REPLY, 100);

        let unit = h.poll().expect("no fatal claim verdict").expect("claimed");
        h.finish(&unit, UnitOutcome::Failed, &fault("cargo test: 3 failed"));

        assert_eq!(
            comments_posted(&h, 1),
            vec!["run did not complete: cargo test: 3 failed"]
        );
    }

    // --- Env + scratch threading ---

    #[test]
    fn the_wire_unit_carries_the_built_ins_key_thread_env_and_layout() {
        let h = Harness::new(cfg("acme/widgets", vec![], vec![], vec![]), "me");
        let mut unit = unit(4);
        unit.claim_id = 1_000_001;
        unit.before_comments = vec![41, 42];
        let wire = h.units.wire_unit(&unit);

        // The claim-journal key the built-in wrote, and the session thread it resumes
        // under — both unchanged across the switch.
        assert_eq!(wire.id, "4");
        assert_eq!(wire.key, "acme/widgets#4#1000001");
        assert_eq!(wire.thread, "acme/widgets#4");
        assert_eq!(wire.seen, ["41", "42"]);
        assert_eq!(wire.me, "me");
        // `unit_env` ∪ `creds_env`: exactly the four names the skill's scripts read.
        assert_eq!(
            wire.env,
            BTreeMap::from([
                (
                    "GITEA_BASE_URL".to_string(),
                    "https://gitea.example.com".to_string()
                ),
                ("GITEA_ISSUE_NUMBER".to_string(), "4".to_string()),
                ("GITEA_REPO".to_string(), "acme/widgets".to_string()),
                ("GITEA_TOKEN".to_string(), "PAT".to_string()),
            ])
        );
        // `scratch_layout`, the brief unframed: afkd frames `task.md` itself.
        assert_eq!(
            wire.files,
            [
                WireFile {
                    path: "task.md".into(),
                    text: "T\n\nB".into(),
                },
                WireFile {
                    path: "issue/number".into(),
                    text: "4".into(),
                },
            ]
        );
    }

    // --- Degenerate targets and swallowed lifecycle errors ---

    #[test]
    fn an_unparseable_repo_claims_nothing() {
        // A `repo` with no `owner/name` shape and no `org` degrades to an empty org
        // target: a poll resolves no repos and claims nothing (rather than panicking).
        let h = Harness::new(cfg("not-a-repo", vec![], vec![], vec![]), "me");
        h.client.add_issue(1, "Fix", "do it", &["afkd/ready"]);
        assert!(h.poll().expect("no fatal claim verdict").is_none());
    }

    #[test]
    fn an_on_claim_failure_is_logged_but_the_issue_is_still_taken_on() {
        // `on_claim` runs after a won claim; a failure there is logged and swallowed,
        // and the unit is still returned (the claim is not rolled back). Use a
        // `close` on_claim the claim itself does not perform and fail that stage.
        let h = Harness::new(
            cfg("acme/widgets", vec![LifecycleAction::Close], vec![], vec![]),
            "me",
        );
        h.client.add_issue(1, "Fix", "do it", &["afkd/ready"]);
        h.client.fail("set state");

        let unit = h
            .poll()
            .expect("no fatal claim verdict")
            .expect("claimed anyway");
        assert_eq!(unit.number, 1);
        assert!(!h.diag.lines().is_empty());
    }

    #[test]
    fn a_bodyless_issue_briefs_with_just_the_title() {
        // An issue whose body is blank writes the title alone as the brief (no
        // trailing blank line + empty body).
        let h = Harness::new(cfg("acme/widgets", vec![], vec![], vec![]), "me");
        let bare = Unit {
            body: "   ".into(),
            title: "Just a title".into(),
            ..unit(9)
        };
        assert_eq!(task_md(&h, &bare), "Just a title");
    }

    // --- What the wire adds: the classify read, and the poll's time budget ---

    /// The clarification gate's read: the skill's `park` marker in the attempt's scratch
    /// directory parks the attempt whatever afkd concluded (the workflow's own gate
    /// faults the run on it, so afkd's verdict is usually `failed`); without it, afkd's
    /// verdict stands. A *directory* named `park` is not the marker.
    #[test]
    fn classify_parks_on_the_marker_and_otherwise_keeps_afkds_verdict() {
        let scratch = TempDir::new();
        for verdict in [UnitOutcome::Clean, UnitOutcome::Failed, UnitOutcome::Park] {
            assert_eq!(IssueUnits::classify(scratch.path(), verdict), verdict);
        }
        std::fs::create_dir(scratch.path().join(PARK_FILE)).unwrap();
        assert_eq!(
            IssueUnits::classify(scratch.path(), UnitOutcome::Clean),
            UnitOutcome::Clean
        );
        std::fs::remove_dir(scratch.path().join(PARK_FILE)).unwrap();
        std::fs::write(scratch.path().join(PARK_FILE), b"").unwrap();
        for verdict in [UnitOutcome::Clean, UnitOutcome::Failed] {
            assert_eq!(
                IssueUnits::classify(scratch.path(), verdict),
                UnitOutcome::Park
            );
        }
    }

    /// A scan that runs past [`POLL_BUDGET`](crate::common::POLL_BUDGET) stops claiming and says so, well inside
    /// afkd's 60-second call deadline. Every candidate here loses its race to a live
    /// rival, and each attempt settles one second, so the twenty-first candidate is the
    /// first the budget turns away — and nothing after it is posted to either.
    #[test]
    fn the_poll_budget_ends_the_scan_and_says_so() {
        let h = Harness::new(cfg("acme/widgets", vec![], vec![], vec![]), "me");
        h.client.set_clock(T);
        for n in 1..=25 {
            h.client.add_issue(
                n,
                "修复 the retry storm 🚨",
                "It retries forever.",
                &["afkd/ready"],
            );
            let rival = crate::claim::claim_text("björn-öst[bot]");
            h.client
                .add_comment_body(n, 1_000 + n, "björn-öst[bot]", &rival, T - 60);
        }

        assert_eq!(h.poll().expect("no fatal claim verdict"), None);

        let posted: Vec<u64> = h
            .client
            .actions()
            .into_iter()
            .filter_map(|a| match a {
                Action::Comment { index, body } if is_claim(&body) => Some(index),
                _ => None,
            })
            .collect();
        assert_eq!(
            posted,
            (1..=20).collect::<Vec<_>>(),
            "twenty attempts, in order"
        );
        assert_eq!(h.clock.sleeps().len(), 20);
        let lines = h.diag.lines();
        assert_eq!(
            lines,
            ["gitea poll: the scan ran past its 20s budget; the rest of it waits for the next poll"]
        );
    }
}
