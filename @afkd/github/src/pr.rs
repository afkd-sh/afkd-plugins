//! The `github_pr_review` kind's vendor half (ADR-0041), ported from afkd's
//! `crates/github/src/trigger_pr.rs`.
//!
//! It polls a single repository's **open** pull requests through the mockable
//! [`GithubClient`] seam — only the bot's own with `author_me` — and treats a PR as
//! eligible when it carries **feedback newer than the bot's last word**
//! ([`feedback::has_new`]): the watermark is the newest of the bot's own comments and
//! reviews, which the agent's reply through the `github` skill is what advances. A claim
//! marker is bookkeeping, never a word, so it moves neither the watermark nor the delta.
//! It claims one with the same `[afkd-claim]` marker claim as the issue kind (a PR is an
//! issue to GitHub), writes the `afkd/claimed` label as **status** only — the watermark,
//! not the label, is the re-pick gate — and loops across polls until a human merges or
//! closes the PR, which drops it out of the `state=open` set. There is **no `on_done
//! close`**, no park and no clarification gate.
//!
//! afkd keeps the spine — the cadence, the queue lane, the claim journal, the attempt
//! bound, the mid-run watch's cursor, and the framing of `task.md` — so what is here is
//! exactly what the built-in's `ForgeUnits` impl does on the vendor side. There is no
//! org-wide polling: a GitHub trigger addresses a single `repo`, and a malformed `repo`
//! degrades to a target that claims nothing.

use std::collections::BTreeMap;

use crate::client::{GithubClient, GithubError, IssueComment, Repo};
use crate::common::{
    apply_actions, claim_issue, claim_key_for, creds_env, delete_marker, release_claim,
    release_stale, renew_marker, unit_key, Claimed, Clock, Diag, ScanBudget, CLAIMED_LABEL,
    ENV_PR_BRANCH, ENV_PR_NUMBER, ENV_REPO,
};
use crate::feedback::{self, FeedbackItem};
use crate::kind::{ClaimedUnit, Units};
use crate::lifecycle::LifecycleAction;
use crate::settings::GithubConfig;
use crate::wire::{Facts, UnitOutcome, WireFile, WireUnit};
use crate::{NUMBER_FILE, PR_DIR, TASK_FILE};

/// One PR taken on as a unit of work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Unit {
    pub(crate) repo: Repo,
    pub(crate) number: u64,
    /// The comment id of the `[afkd-claim]` marker this claim holds the PR with —
    /// released at run end, and the tail of the claim-journal key so a crashed run's
    /// marker is reaped.
    pub(crate) claim_id: u64,
    /// The PR's head branch, threaded into the run's env.
    pub(crate) head_branch: String,
    /// The human-feedback delta delivered in this run's brief (oldest-first), each item
    /// carrying its author so the brief reads as a thread with speakers.
    pub(crate) feedback: Vec<FeedbackItem>,
    /// The ids of every comment on the PR at claim time — the `seen` afkd's watch starts
    /// from, so the thread the brief was built from is never re-delivered as new.
    pub(crate) before_comments: Vec<u64>,
    /// The login this PR was claimed as: the unit's `self`, and the identity the terminal
    /// lifecycle assigns and unassigns.
    pub(crate) claimed_as: String,
}

impl Unit {
    /// The claim-journal key — the wire unit's `key`, and what every later call names it
    /// by. It carries the claim marker, so it changes on every claim of the PR.
    pub(crate) fn key(&self) -> String {
        claim_key_for(&self.repo, self.number, self.claim_id)
    }
}

impl ClaimedUnit for Unit {
    /// The PR's stable coordinate, `<owner>/<name>#<number>` (ADR-0067): the same across
    /// every claim, so an agent session resumes per PR across the review rounds.
    fn thread(&self) -> String {
        unit_key(&self.repo, self.number)
    }

    fn claimed_as(&self) -> &str {
        &self.claimed_as
    }
}

/// The `github_pr_review` kind's vendor half: the single-repo target, the `author_me`
/// filter, the claim, and the three lifecycle action lists.
pub(crate) struct PrUnits {
    client: Box<dyn GithubClient>,
    /// The single repository polled, or `None` when the `repo` setting was not
    /// `owner/name` (the kind then claims nothing rather than failing).
    repo: Option<Repo>,
    /// Restrict to the bot's own PRs (the `author_me` flag).
    author_me: bool,
    on_claim: Vec<LifecycleAction>,
    on_done: Vec<LifecycleAction>,
    on_fail: Vec<LifecycleAction>,
    /// The token and host every unit's `env` carries.
    creds: BTreeMap<String, String>,
}

impl PrUnits {
    pub(crate) fn new(client: Box<dyn GithubClient>, cfg: &GithubConfig) -> Self {
        Self {
            client,
            repo: Repo::parse(&cfg.repo),
            author_me: cfg.author_me,
            on_claim: cfg.on_claim.clone(),
            on_done: cfg.on_done.clone(),
            on_fail: cfg.on_fail.clone(),
            creds: creds_env(cfg),
        }
    }
}

impl Units for PrUnits {
    type Unit = Unit;

    /// No `classify` — the built-in keeps the spine's default, so a PR never parks — and
    /// no `attempt_failed`.
    const CALLS: &'static [&'static str] = &["release", "renew", "comments"];

    /// Resolve the authenticated user (the claim identity and the `author_me` filter).
    fn resolve_me(&self) -> Result<String, GithubError> {
        Ok(self.client.current_user()?.login)
    }

    /// Claim the next PR with feedback newer than the bot's last word — the claim race
    /// runs here, inside afkd's `poll`.
    ///
    /// The scan stops once it has run [`POLL_BUDGET`](crate::common::POLL_BUDGET),
    /// checked before each claim attempt, and answers "nothing this beat".
    fn try_claim_next(
        &self,
        me: &str,
        diag: &dyn Diag,
        clock: &dyn Clock,
    ) -> Result<Option<Unit>, GithubError> {
        let Some(repo) = &self.repo else {
            return Ok(None);
        };
        let budget = ScanBudget::start(clock, diag);
        for pr in self.client.list_open_pulls(repo)? {
            if self.author_me && pr.user.login != me {
                continue;
            }
            let comments = self.client.list_issue_comments(repo, pr.number)?;
            let reviews = self.client.list_pull_reviews(repo, pr.number)?;
            if !feedback::has_new(&comments, &reviews, me) {
                continue;
            }
            if budget.spent() {
                return Ok(None);
            }
            match claim_issue(&*self.client, repo, pr.number, me, clock, diag)? {
                Claimed::Won(claim_id) => {
                    // The claim is the marker; the label is the status it shows. It is
                    // not this kind's re-pick gate (the watermark is), so a refused write
                    // is diagnosed and the claim kept.
                    if let Err(e) = self.client.add_label(repo, pr.number, CLAIMED_LABEL) {
                        diag.err(&e);
                    }
                    // `on_claim` runs before any fire, so neutral facts.
                    if let Err(e) = apply_actions(
                        &*self.client,
                        repo,
                        pr.number,
                        &self.on_claim,
                        me,
                        &Facts::none(),
                    ) {
                        diag.err(&e);
                    }
                    return Ok(Some(Unit {
                        repo: repo.clone(),
                        number: pr.number,
                        claim_id,
                        head_branch: pr.head_branch,
                        feedback: feedback::delta(&comments, &reviews, me),
                        before_comments: comments.iter().map(|c| c.id).collect(),
                        claimed_as: me.to_string(),
                    }));
                }
                // Lost the race for this PR; try the next one.
                Claimed::Lost => continue,
            }
        }
        Ok(None)
    }

    /// The unit as it crosses the wire: the built-in's `unit_key` / `unit_thread` /
    /// `unit_env` ∪ `creds_env` / `scratch_layout`, with the claim-time comment ids as
    /// `seen` and the claim identity as `self`. The PR number and head branch ride the
    /// env so a prompted `git fetch`/checkout reconstructs the branch. The brief is
    /// unframed — afkd frames `task.md` itself.
    fn wire_unit(&self, unit: &Unit) -> WireUnit {
        let mut env = self.creds.clone();
        env.insert(ENV_REPO.to_string(), unit.repo.full_name());
        env.insert(ENV_PR_NUMBER.to_string(), unit.number.to_string());
        env.insert(ENV_PR_BRANCH.to_string(), unit.head_branch.clone());
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
                    text: feedback::pr_brief(unit.number, &unit.feedback),
                },
                WireFile {
                    path: format!("{PR_DIR}/{NUMBER_FILE}"),
                    text: unit.number.to_string(),
                },
            ],
        }
    }

    fn release(&self, unit: &Unit, diag: &dyn Diag) {
        release_claim(
            &*self.client,
            &unit.repo,
            unit.number,
            &unit.claimed_as,
            unit.claim_id,
            diag,
        );
    }

    /// Release one claim named by a whole journal key — a crashed run's leftover, or a
    /// unit afkd handed back. A PR's key is an issue's key, so the release is the issue
    /// kind's.
    fn release_stale(&self, key: &str, me: &str, diag: &dyn Diag) -> Option<bool> {
        release_stale(&*self.client, key, me, diag)
    }

    /// Keep this unit's claim marker alive for as long as afkd's fire holds it, so a run
    /// outliving `CLAIM_LIFETIME` is not double-claimed by a sibling instance.
    fn renew(&self, unit: &Unit, renewal: u64, diag: &dyn Diag) {
        renew_marker(
            &*self.client,
            &unit.repo,
            unit.claim_id,
            &unit.claimed_as,
            renewal,
            diag,
        );
    }

    /// Every comment on the PR, for afkd's mid-run watch — GitHub models a PR as an
    /// issue, so its conversation is read through the issue comments endpoint.
    fn comments(&self, unit: &Unit) -> Result<Vec<IssueComment>, GithubError> {
        self.client.list_issue_comments(&unit.repo, unit.number)
    }

    /// The terminal lifecycle: `on_done` for a clean run, `on_fail` otherwise. There is
    /// no `on_done close` here — a human's merge ends the loop by dropping the PR from
    /// the `state=open` set — and no park, so a `park` verdict (which afkd never sends
    /// this kind, since it answers no `classify`) runs `on_fail` as the built-in does.
    ///
    /// Returns whether the moment reached the remote.
    fn finish(&self, unit: &Unit, outcome: UnitOutcome, facts: &Facts, diag: &dyn Diag) -> bool {
        let actions = match outcome {
            UnitOutcome::Clean => &self.on_done,
            UnitOutcome::Park | UnitOutcome::Failed => &self.on_fail,
        };
        let delivered = match apply_actions(
            &*self.client,
            &unit.repo,
            unit.number,
            actions,
            &unit.claimed_as,
            facts,
        ) {
            Ok(()) => true,
            Err(e) => {
                diag.err(&e);
                false
            }
        };
        // The claim is over however it ended, so its marker goes — a finished round must
        // leave no marker to out-order the next claim. Best-effort: a leaked marker ages
        // out after `CLAIM_LIFETIME`.
        delete_marker(&*self.client, &unit.repo, unit.claim_id, diag);
        delivered
    }
}

#[cfg(test)]
mod tests {
    //! No network: every test drives the in-memory `MockClient` and a fake clock.
    //!
    //! What is proven here is the **vendor half** — eligibility, the claim, the brief,
    //! the env, the terminal lifecycle. The drive around it (attempt counting, the claim
    //! journal, the run-name mint, the cadence, the framing of `task.md`) is afkd's, on
    //! the far side of the wire; the wire itself is `tests/wire.rs`.

    use super::*;
    use crate::claim::{claim_renewal_text, claim_text, is_claim, split_claim_key, CLAIM_MARKER};
    use crate::client::{Action, MockClient};
    use crate::common::{CaptureDiag, FakeClock};
    use crate::feedback::review_thread;
    use std::sync::Arc;

    fn cfg(repo: &str) -> GithubConfig {
        GithubConfig {
            host: "github.com".into(),
            repo: repo.into(),
            token: "PAT".into(),
            author_me: true,
            on_claim: vec![
                LifecycleAction::AssignMe,
                LifecycleAction::LabelAdd("afkd/reviewing".into()),
            ],
            on_fail: vec![LifecycleAction::Unassign],
            ..GithubConfig::default()
        }
    }

    /// The kind over a shared [`MockClient`], with a capturing diagnostic sink and a fake
    /// clock — the spine's side of each call, played by hand: [`poll`](Self::poll) is the
    /// `poll` call, [`finish`](Self::finish) the `finish` call.
    struct Harness {
        client: Arc<MockClient>,
        units: PrUnits,
        diag: CaptureDiag,
        clock: FakeClock,
    }

    impl Harness {
        fn new(cfg: GithubConfig) -> Self {
            let client = Arc::new(MockClient::new("me"));
            let units = PrUnits::new(Box::new(Arc::clone(&client)), &cfg);
            Self {
                client,
                units,
                diag: CaptureDiag::default(),
                clock: FakeClock::new(),
            }
        }

        /// One beat as the plugin drives it: a forge error is diagnosed and the beat idle.
        fn poll(&self) -> Option<Unit> {
            match self.units.try_claim_next("me", &self.diag, &self.clock) {
                Ok(unit) => unit,
                Err(e) => {
                    self.diag.err(&e);
                    None
                }
            }
        }

        /// The `finish` call for `unit`, ended `outcome` with `facts`.
        fn finish(&self, unit: &Unit, outcome: UnitOutcome, facts: &Facts) -> bool {
            self.units.finish(unit, outcome, facts, &self.diag)
        }
    }

    fn repo() -> Repo {
        Repo {
            owner: "acme".into(),
            name: "widgets".into(),
        }
    }

    /// A claimed PR #7 on `feature/x` carrying `feedback`, as a finish or renewal test
    /// holds it without polling.
    fn unit(claim_id: u64, feedback: Vec<FeedbackItem>) -> Unit {
        Unit {
            repo: repo(),
            number: 7,
            claim_id,
            head_branch: "feature/x".into(),
            feedback,
            before_comments: Vec::new(),
            claimed_as: "me".into(),
        }
    }

    /// The second a claim race is staged in, frozen on the mock's post clock so a rival
    /// marker can be placed a known distance either side of our own.
    const T: u64 = 1_700_000_000;

    /// The `[afkd-claim]` marker bodies on PR `index`, read through the trait's own
    /// list call — the same thing the claim reads.
    fn claim_markers_on(h: &Harness, index: u64) -> Vec<String> {
        h.client
            .list_issue_comments(&repo(), index)
            .expect("read the thread")
            .into_iter()
            .filter(|c| is_claim(&c.body))
            .map(|c| c.body)
            .collect()
    }

    /// The brief this kind hands over is the **shared** renderer's text (parity, not a
    /// second hand-written string): `wire_unit` is driven for real over the adversarial
    /// `review_thread` fixture, and what lands in `task.md` must equal
    /// `feedback::pr_brief` over the same thread, which is where that text is pinned.
    #[test]
    fn the_brief_in_the_wire_unit_is_the_shared_renderers_text() {
        let h = Harness::new(cfg("acme/widgets"));
        let wire = h.units.wire_unit(&unit(1, review_thread()));
        assert_eq!(wire.files[0].path, TASK_FILE);
        assert_eq!(wire.files[0].text, feedback::pr_brief(7, &review_thread()));
        // Not vacuous: the thread really did reach the brief, attributed by speaker.
        assert!(
            wire.files[0].text.contains("**bob-döner:**"),
            "{}",
            wire.files[0].text
        );
    }

    // --- Eligibility + claim ---

    #[test]
    fn polls_only_author_me_open_prs_and_claims_one_with_new_feedback() {
        let h = Harness::new(cfg("acme/widgets"));
        // Someone else's PR, listed first → filtered out by author_me.
        h.client.add_pull(8, "other", "feature/y");
        h.client.add_comment(8, 2, "human", 100);
        // Mine, with new human feedback → eligible.
        h.client.add_pull(7, "me", "feature/x");
        h.client.add_comment(7, 1, "human", 100);

        let unit = h.poll().expect("claimed");
        assert_eq!(unit.number, 7);
        assert_eq!(unit.head_branch, "feature/x");
        assert_eq!(unit.before_comments, [1]);
        assert_eq!(unit.claimed_as, "me");
        // The claim labelled (the kind's status) and assigned us (on_claim).
        assert!(h.client.has_label(7, CLAIMED_LABEL));
        assert!(h.client.has_label(7, "afkd/reviewing"));
        assert_eq!(h.client.assignees_of(7), ["me"]);
        assert!(!h.client.has_label(8, CLAIMED_LABEL));
        assert!(h.diag.lines().is_empty(), "{:?}", h.diag.lines());
    }

    #[test]
    fn a_pr_with_no_new_feedback_is_idle() {
        let h = Harness::new(cfg("acme/widgets"));
        h.client.add_pull(7, "me", "feature/x");
        // The bot already replied last (t=200) and nothing newer arrived — a review
        // older than its reply included.
        h.client.add_comment(7, 1, "human", 100);
        h.client.add_review(7, 3, "carol", 150);
        h.client.add_comment(7, 2, "me", 200);
        assert_eq!(h.poll(), None);
        assert!(h.client.actions().is_empty(), "{:?}", h.client.actions());
    }

    /// A review alone is feedback: a PR whose only word newer than the bot's is a review
    /// is claimed, and the brief names the reviewer.
    #[test]
    fn a_review_newer_than_the_bots_last_word_is_new_feedback() {
        let h = Harness::new(cfg("acme/widgets"));
        h.client.add_pull(7, "me", "feature/x");
        h.client.add_comment(7, 1, "human", 100);
        h.client.add_comment(7, 2, "me", 200);
        h.client.add_review(7, 3, "björn-öst", 300);
        let unit = h.poll().expect("claimed");
        assert_eq!(
            unit.feedback,
            [FeedbackItem {
                author: "björn-öst".into(),
                body: "(review 3)".into()
            }]
        );
        assert!(h.units.wire_unit(&unit).files[0]
            .text
            .ends_with("**björn-öst:** (review 3)\n"));
    }

    /// A merge or a close takes the PR out of the `state=open` set, so the poll after it
    /// claims nothing — no special "until closed" case.
    #[test]
    fn a_merged_or_closed_pr_drops_out_of_the_open_set() {
        let h = Harness::new(cfg("acme/widgets"));
        h.client.add_pull(7, "me", "feature/x");
        h.client.add_comment(7, 1, "human", 100);
        h.client.close(7);
        assert_eq!(h.poll(), None);
        assert!(h.client.actions().is_empty(), "{:?}", h.client.actions());
    }

    #[test]
    fn a_not_me_pr_is_skipped_without_claiming() {
        // Only a foreign-authored PR is open: the `author_me` filter skips it, so the
        // poll walks the whole list and claims nothing (no marker, no assign/label).
        let h = Harness::new(cfg("acme/widgets"));
        h.client.add_pull(8, "other", "feature/y");
        h.client.add_comment(8, 2, "human", 100);
        assert_eq!(h.poll(), None);
        assert!(
            h.client.actions().is_empty(),
            "a filtered-out PR is never claimed: {:?}",
            h.client.actions()
        );
        // Without `author_me`, the same PR is anyone's to review.
        let mut all = cfg("acme/widgets");
        all.author_me = false;
        let h = Harness::new(all);
        h.client.add_pull(8, "other", "feature/y");
        h.client.add_comment(8, 2, "human", 100);
        assert_eq!(h.poll().map(|u| u.number), Some(8));
    }

    #[test]
    fn a_lost_claim_releases_the_marker_and_claims_no_unit() {
        // A rival's marker lands between our post and our re-read, ordering ahead of
        // ours: the claim loses, so the poll returns no unit, our marker is deleted, and
        // the `afkd/claimed` status label is never applied.
        let h = Harness::new(cfg("acme/widgets"));
        h.client.add_pull(7, "me", "feature/x");
        h.client.add_comment(7, 1, "human", 100);
        h.client.set_clock(T);
        h.client.rival_claims_next("rival", 42, T - 1);

        assert_eq!(h.poll(), None);
        assert!(
            h.client
                .actions()
                .iter()
                .any(|a| matches!(a, Action::DeleteComment { .. })),
            "a lost claim deletes its own marker: {:?}",
            h.client.actions()
        );
        assert_eq!(
            claim_markers_on(&h, 7),
            [claim_text("rival")],
            "only the rival's marker is left on the thread"
        );
        assert!(!h.client.has_label(7, CLAIMED_LABEL), "nothing was claimed");
        assert!(h.client.assignees_of(7).is_empty(), "on_claim never ran");
    }

    /// The `Lost` arm continues the candidate loop: a PR whose claim is lost does not
    /// end the poll — the next eligible PR is claimed instead.
    #[test]
    fn a_lost_claim_moves_on_to_the_next_pr() {
        let h = Harness::new(cfg("acme/widgets"));
        h.client.add_pull(7, "me", "feature/x");
        h.client.add_comment(7, 1, "human", 100);
        h.client.add_pull(8, "me", "feature/y");
        h.client.add_comment(8, 2, "human", 100);
        // The rival lands in #7's thread on the next comment read, and stays.
        h.client.set_clock(T);
        h.client.rival_claims_next("rival", 42, T - 1);

        let unit = h.poll().expect("the second PR is claimed");
        assert_eq!(unit.number, 8);
        assert!(!h.client.has_label(7, CLAIMED_LABEL), "#7 was left alone");
        assert_eq!(h.clock.sleeps().len(), 2, "two claim attempts settled");
    }

    /// A PR whose only new comment is a **rival's claim marker** is not eligible: a
    /// marker is bookkeeping, not feedback, so it must not re-fire a review round.
    #[test]
    fn a_rivals_marker_is_not_new_feedback() {
        let h = Harness::new(cfg("acme/widgets"));
        h.client.add_pull(7, "me", "feature/x");
        h.client.add_comment(7, 1, "human", 100);
        h.client.add_comment(7, 2, "me", 200);
        h.client
            .add_comment_body(7, 3, "björn-öst[bot]", &claim_text("björn-öst[bot]"), 300);

        assert_eq!(
            h.poll(),
            None,
            "the marker is not an answer to the bot's last word"
        );
        assert!(
            h.client.actions().is_empty(),
            "an idle PR is never claimed: {:?}",
            h.client.actions()
        );
    }

    /// A claim marker is never read as conversation, asserted as **parity**: the same PR
    /// polled with and without a pair of markers interleaved into its thread must agree
    /// on eligibility *and* on the brief handed over — not two hand-written expectations
    /// that could drift. The markers sit exactly where they would do damage if counted:
    /// **ours** newest after the bot's last word (it would push the watermark past the
    /// human's reply and leave the PR idle forever) and a **rival's** newest of all,
    /// authored by a different login (it would be delivered to the agent as feedback).
    #[test]
    fn a_claim_marker_is_never_read_as_conversation() {
        /// Seed one PR #7 with the adversarial thread, optionally interleaving the two
        /// claim markers, and poll for real. Returns the claimed unit's brief.
        fn brief_of(with_markers: bool) -> Option<String> {
            let h = Harness::new(cfg("acme/widgets"));
            h.client.add_pull(7, "me", "feature/x");
            h.client
                .add_comment_body(7, 1, "me", "Which backoff should I use?", 100);
            h.client.add_comment_edited(
                7,
                2,
                "álvaro",
                "Exponential, please — see §4 of the RFC 🙏\n\n    max_backoff = 30\n",
                150,
                200,
            );
            h.client
                .add_comment_body(7, 3, "陳大文", "看起来不对 🚨", 300);
            if with_markers {
                // Ours, newest after the bot's last word…
                h.client
                    .add_comment_body(7, 4, "me", &claim_text("me"), 400);
                // …and a rival's, newest of all.
                h.client.add_comment_body(
                    7,
                    5,
                    "björn-öst[bot]",
                    &claim_text("björn-öst[bot]"),
                    500,
                );
            }
            let unit = h.poll()?;
            Some(h.units.wire_unit(&unit).files[0].text.clone())
        }

        let clean = brief_of(false).expect("the human replies make the PR eligible");
        let with_markers = brief_of(true).expect("a marker changes no eligibility");
        assert_eq!(
            with_markers, clean,
            "a claim marker is neither the bot's word nor a human's"
        );
        // Not vacuous: the brief really does carry both human replies, verbatim and
        // attributed — and no marker text reached it.
        assert!(clean.contains("**álvaro:**"), "{clean}");
        assert!(clean.contains("**陳大文:** 看起来不对 🚨"), "{clean}");
        assert!(!with_markers.contains(CLAIM_MARKER), "{with_markers}");
    }

    /// The journal key carries the claim, the session thread does not: two successive
    /// claims of the same PR give two different keys (each naming its own marker) and one
    /// identical thread, so an agent session resumes per PR across review rounds.
    #[test]
    fn the_journal_key_carries_the_claim_but_the_thread_does_not() {
        let h = Harness::new(cfg("acme/widgets"));
        h.client.add_pull(7, "me", "feature/x");
        h.client.add_comment(7, 1, "human", 100);

        let first = h.poll().expect("claimed");
        // Wind the round down as a finished run does (releasing the marker), leaving the
        // human's word still newer than the bot's — so the PR is claimable again, the
        // status label notwithstanding.
        assert!(h.finish(&first, UnitOutcome::Clean, &Facts::none()));
        let second = h.poll().expect("re-claimed");

        let (key_a, key_b) = (first.key(), second.key());
        assert_ne!(key_a, key_b, "a fresh claim is a fresh journal key");
        assert_eq!(
            first.thread(),
            second.thread(),
            "the session thread is the PR, not the claim"
        );
        assert_eq!(first.thread(), "acme/widgets#7");
        for (key, unit) in [(&key_a, &first), (&key_b, &second)] {
            assert_eq!(
                split_claim_key(key),
                Some(("acme/widgets", 7, unit.claim_id))
            );
        }
    }

    #[test]
    fn an_on_claim_failure_is_logged_but_the_claim_proceeds() {
        // `on_claim` runs after a won claim; a failure there is logged and swallowed —
        // the unit is still taken on. Use an `on_claim` the claim itself does not
        // perform (`close`) and fail that stage.
        let mut closing = cfg("acme/widgets");
        closing.on_claim = vec![LifecycleAction::Close];
        let h = Harness::new(closing);
        h.client.add_pull(7, "me", "feature/x");
        h.client.add_comment(7, 1, "human", 100);
        h.client.fail("set state");

        let unit = h.poll().expect("claimed anyway");
        assert_eq!(unit.number, 7);
        assert_eq!(
            h.diag.lines(),
            ["github set state: no response (mock failure)"]
        );

        // The status label is diagnosed the same way, and the claim kept.
        let mut plain = cfg("acme/widgets");
        plain.on_claim.clear();
        let h = Harness::new(plain);
        h.client.add_pull(7, "me", "feature/x");
        h.client.add_comment(7, 1, "human", 100);
        h.client.fail("add label");
        assert_eq!(h.poll().expect("claimed anyway").number, 7);
        assert_eq!(
            h.diag.lines(),
            ["github add label: no response (mock failure)"]
        );
    }

    /// A forge error mid-poll is the kind's `Err`, which the plugin answers as an idle
    /// beat after diagnosing it with its stage; the next poll starts afresh.
    #[test]
    fn a_poll_forge_error_is_returned_and_the_next_poll_claims() {
        let h = Harness::new(cfg("acme/widgets"));
        h.client.add_pull(7, "me", "feature/x");
        h.client.add_comment(7, 1, "human", 100);
        h.client.fail("list pulls");
        assert_eq!(h.poll(), None);
        assert_eq!(
            h.diag.lines(),
            ["github list pulls: no response (mock failure)"]
        );
        h.client.clear_failure();
        assert_eq!(h.poll().map(|u| u.number), Some(7));

        // And the per-PR review read, the one the issue kind never makes.
        let h = Harness::new(cfg("acme/widgets"));
        h.client.add_pull(7, "me", "feature/x");
        h.client.fail("list reviews");
        assert_eq!(h.poll(), None);
        assert_eq!(
            h.diag.lines(),
            ["github list reviews: no response (mock failure)"]
        );
        assert!(h.client.actions().is_empty(), "{:?}", h.client.actions());
    }

    #[test]
    fn an_unresolvable_repo_claims_nothing() {
        // A `repo` with no `owner/name` shape resolves to no target, so a poll claims
        // nothing rather than failing — and asks the forge nothing.
        let h = Harness::new(cfg("not-a-repo"));
        h.client.add_pull(7, "me", "feature/x");
        h.client.add_comment(7, 1, "human", 100);
        h.client.fail("list pulls");
        assert_eq!(h.poll(), None);
        assert!(h.diag.lines().is_empty(), "{:?}", h.diag.lines());
    }

    /// A scan that runs past [`POLL_BUDGET`](crate::common::POLL_BUDGET) stops claiming
    /// and says so — the budget the issue kind keeps, shared. Every PR here carries a
    /// human's feedback and loses its race to a live rival, and each attempt settles one
    /// second, so the twenty-first is the first the budget turns away.
    #[test]
    fn the_poll_budget_ends_a_pr_scan_and_says_so() {
        let h = Harness::new(cfg("acme/widgets"));
        h.client.set_clock(T);
        for n in 1..=25 {
            h.client.add_pull(n, "me", &format!("fix/重试-{n}"));
            h.client.add_comment_body(
                n,
                2_000 + n,
                "陳大文",
                "看起来不对 🚨\n\n    retry(1);\n",
                T - 120,
            );
            let rival = claim_text("björn-öst[bot]");
            h.client
                .add_comment_body(n, 1_000 + n, "björn-öst[bot]", &rival, T - 60);
        }

        assert_eq!(h.poll(), None);

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
        assert_eq!(
            h.diag.lines(),
            ["github poll: the scan ran past its 20s budget; the rest of it waits for the next poll"]
        );
    }

    // --- The wire unit + terminal lifecycle ---

    /// The whole wire unit: the number as `id`, the claim-journal key, the stable
    /// thread, the claim-time comments as `seen`, the claim identity as `self`, the PR
    /// number and head branch merged over the credentials, and the brief beside the
    /// bare number under `pr/number`.
    #[test]
    fn the_wire_unit_carries_pr_number_branch_thread_and_layout() {
        let h = Harness::new(cfg("acme/widgets"));
        let unit = Unit {
            head_branch: "feature/重试-backoff".into(),
            before_comments: vec![41, 42],
            ..unit(
                1_000_001,
                vec![FeedbackItem {
                    author: "alice".into(),
                    body: "please rename the flag".into(),
                }],
            )
        };
        let env = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<BTreeMap<_, _>>()
        };
        assert_eq!(
            h.units.wire_unit(&unit),
            WireUnit {
                id: "7".into(),
                key: "acme/widgets#7#1000001".into(),
                thread: "acme/widgets#7".into(),
                seen: vec!["41".into(), "42".into()],
                me: "me".into(),
                env: env(&[
                    ("GITHUB_HOST", "github.com"),
                    ("GITHUB_PR_BRANCH", "feature/重试-backoff"),
                    ("GITHUB_PR_NUMBER", "7"),
                    ("GITHUB_REPO", "acme/widgets"),
                    ("GITHUB_TOKEN", "PAT"),
                ]),
                files: vec![
                    WireFile {
                        path: "task.md".into(),
                        text: "Address review feedback on PR #7.\n\n## New feedback\n\n\
                               **alice:** please rename the flag\n"
                            .into(),
                    },
                    WireFile {
                        path: "pr/number".into(),
                        text: "7".into(),
                    },
                ],
            }
        );
    }

    #[test]
    fn a_clean_finish_emits_no_close() {
        // A human's merge ends the loop, so a clean round records no state change and
        // takes its marker off the thread. `on_done` is empty here, and `on_fail`'s
        // `unassign` does not run: the bot stays on the PR it is iterating.
        let h = Harness::new(cfg("acme/widgets"));
        h.client.add_pull(7, "me", "feature/x");
        h.client.add_assignees(&repo(), 7, &["me".into()]).unwrap();
        h.client
            .add_comment_body(7, 9, "me", &claim_text("me"), 100);
        let before = h.client.actions().len();
        assert!(h.finish(&unit(9, Vec::new()), UnitOutcome::Clean, &Facts::none()));
        assert_eq!(
            h.client.actions()[before..],
            [Action::DeleteComment { id: 9 }]
        );
        assert_eq!(h.client.assignees_of(7), ["me"]);
        assert!(claim_markers_on(&h, 7).is_empty());
    }

    /// `on_fail` runs for a failed round — and for a `park`, which the built-in folds
    /// into the same moment since the kind has no park of its own. Only the bot's own
    /// assignee row goes.
    #[test]
    fn a_failed_or_parked_finish_runs_on_fail() {
        for outcome in [UnitOutcome::Failed, UnitOutcome::Park] {
            let h = Harness::new(cfg("acme/widgets"));
            h.client.add_pull(7, "me", "feature/x");
            h.client
                .add_assignees(&repo(), 7, &["alice".into(), "me".into()])
                .unwrap();
            assert!(h.finish(&unit(9, Vec::new()), outcome, &Facts::none()));
            assert_eq!(h.client.assignees_of(7), ["alice"], "{outcome:?}");
        }
    }

    /// A finished round leaves no marker, whichever way it ended: the release runs after
    /// the terminal `on_done`/`on_fail` dispatch alike.
    #[test]
    fn a_finished_pr_leaves_no_claim_marker() {
        for outcome in [UnitOutcome::Clean, UnitOutcome::Failed, UnitOutcome::Park] {
            let h = Harness::new(cfg("acme/widgets"));
            h.client.add_pull(7, "me", "feature/x");
            h.client.add_comment(7, 1, "human", 100);

            let unit = h.poll().expect("claimed");
            assert_eq!(claim_markers_on(&h, 7).len(), 1, "the claim is held");

            assert!(h.finish(&unit, outcome, &Facts::none()));
            assert!(
                claim_markers_on(&h, 7).is_empty(),
                "a round ending {outcome:?} released its marker: {:?}",
                claim_markers_on(&h, 7)
            );
        }
    }

    #[test]
    fn a_terminal_lifecycle_failure_is_logged() {
        // A faulting run drives the `on_fail` lifecycle; a transport error there is
        // diagnosed and the moment reports it did not land, so the plugin can hold it —
        // and the marker still goes.
        let h = Harness::new(cfg("acme/widgets"));
        h.client.add_pull(7, "me", "feature/x");
        h.client
            .add_comment_body(7, 9, "me", &claim_text("me"), 100);
        // GitHub's unassign is the dedicated `remove assignees` endpoint.
        h.client.fail("remove assignees");
        assert!(!h.finish(&unit(9, Vec::new()), UnitOutcome::Failed, &Facts::none()));
        assert_eq!(
            h.diag.lines(),
            ["github remove assignees: no response (mock failure)"]
        );
        assert!(claim_markers_on(&h, 7).is_empty());
    }

    /// The renewal over the real `renew_marker`: the claim marker is edited **in place**
    /// — same id, a body that still reads as a claim — and the forge's last-touched stamp
    /// moves with it, which is the whole liveness signal.
    #[test]
    fn renewing_a_claim_edits_the_marker_in_place() {
        let h = Harness::new(cfg("acme/widgets"));
        h.client.add_pull(7, "me", "feature/x");
        h.client
            .add_comment_body(7, 9, "me", &claim_text("me"), 100);

        h.units.renew(&unit(9, Vec::new()), 3, &h.diag);

        let renewed = claim_renewal_text("me", 3);
        assert_eq!(
            h.client.actions(),
            vec![Action::EditComment {
                id: 9,
                body: renewed.clone(),
            }],
            "the marker was not edited by its own id"
        );
        let thread = h
            .client
            .list_issue_comments(&repo(), 7)
            .expect("read the thread");
        assert_eq!(thread.len(), 1, "a renewal minted a second comment");
        assert_eq!(thread[0].id, 9, "the comment id moved");
        assert_eq!(thread[0].body, renewed);
        assert!(
            thread[0].updated_at > thread[0].created_at,
            "the liveness half did not move"
        );
    }
}
