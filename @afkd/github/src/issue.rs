//! The `github` issue kind's vendor half (ADR-0041), ported from afkd's
//! `crates/github/src/trigger_issue.rs`.
//!
//! It polls a single repository through the mockable [`GithubClient`] seam for issues that
//! are **open** and carry the configured **source label**; claims one with a
//! `[afkd-claim]` marker comment (post → settle → re-read → decide, then the
//! `afkd/claimed` status label and `on_claim`); and reflects the run's end back through
//! GitHub's native assignee/label/state primitives. Its logic is unit-tested with **no
//! network** against [`MockClient`](crate::client::MockClient) and a fake clock.
//!
//! afkd keeps the spine — the cadence, the queue lane, the claim journal, the attempt
//! bound, the mid-run watch's cursor, and the framing of `task.md` — so what is here is
//! exactly what the built-in's `ForgeUnits` impl does on the vendor side.
//!
//! Work is only ever claimed from the source-labelled, unclaimed set; an issue already
//! carrying `afkd/claimed` is never re-picked. With `source_label` unset the label filter
//! is empty, so that set is *every* open issue — the key is what narrows intake. There is
//! no org-wide polling: a GitHub trigger addresses a single `repo`, and a malformed `repo`
//! degrades to a target that claims nothing.

use std::collections::BTreeMap;

use crate::client::{GithubClient, GithubError, Issue, IssueComment, Repo};
use crate::common::{
    apply_actions, claim_issue, claim_key_for, creds_env, delete_marker, release_claim,
    release_stale, renew_marker, unit_key, Claimed, Clock, Diag, ScanBudget, CLAIMED_LABEL,
    ENV_ISSUE_NUMBER, ENV_REPO,
};
use crate::kind::{ClaimedUnit, Units};
use crate::lifecycle::LifecycleAction;
use crate::settings::GithubConfig;
use crate::wire::{Facts, UnitOutcome, WireFile, WireUnit};
use crate::{ISSUE_DIR, NUMBER_FILE, TASK_FILE};

/// One issue taken on as a unit of work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Unit {
    pub(crate) repo: Repo,
    pub(crate) number: u64,
    /// The comment id of the `[afkd-claim]` marker this claim holds the issue with —
    /// released at run end, and the tail of the claim-journal key so a crashed run's
    /// marker is reaped.
    pub(crate) claim_id: u64,
    pub(crate) title: String,
    pub(crate) body: String,
    /// The login this issue was claimed as: the unit's `self`, and the identity the
    /// terminal lifecycle assigns and unassigns.
    pub(crate) claimed_as: String,
}

impl Unit {
    /// The claim-journal key — the wire unit's `key`, and what every later call names it
    /// by. It carries the claim marker, so it changes on every claim of the issue.
    pub(crate) fn key(&self) -> String {
        claim_key_for(&self.repo, self.number, self.claim_id)
    }

    /// The issue's stable coordinate, `<owner>/<name>#<number>` (ADR-0067): the wire
    /// unit's `thread`, the same across every claim, so an agent session resumes per
    /// issue.
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

/// The `github` kind's vendor half: the single-repo target, the source-label intake gate,
/// the claim, and the three lifecycle action lists.
pub(crate) struct IssueUnits {
    client: Box<dyn GithubClient>,
    /// The single repository polled, or `None` when the `repo` setting was not
    /// `owner/name` (the kind then claims nothing rather than failing).
    repo: Option<Repo>,
    source_label: String,
    on_claim: Vec<LifecycleAction>,
    on_done: Vec<LifecycleAction>,
    on_fail: Vec<LifecycleAction>,
    /// The token and host every unit's `env` carries.
    creds: BTreeMap<String, String>,
}

impl IssueUnits {
    pub(crate) fn new(client: Box<dyn GithubClient>, cfg: &GithubConfig) -> Self {
        Self {
            client,
            repo: Repo::parse(&cfg.repo),
            source_label: cfg.source_label.clone(),
            on_claim: cfg.on_claim.clone(),
            on_done: cfg.on_done.clone(),
            on_fail: cfg.on_fail.clone(),
            creds: creds_env(cfg),
        }
    }

    /// Whether `issue` is claimable: open and not already `afkd/claimed`. (The
    /// `state`/`labels` filter is also applied server-side, but the guard is enforced
    /// here too.)
    fn eligible(&self, issue: &Issue) -> bool {
        issue.state == "open" && !issue.has_label(CLAIMED_LABEL)
    }

    /// Find the first eligible issue in the polled repo and claim it — the claim race
    /// runs here, inside afkd's `poll`.
    ///
    /// The scan stops once it has run [`POLL_BUDGET`](crate::common::POLL_BUDGET), checked
    /// before each claim attempt, and answers "nothing this beat".
    pub(crate) fn try_claim_next(
        &self,
        me: &str,
        diag: &dyn Diag,
        clock: &dyn Clock,
    ) -> Result<Option<Unit>, GithubError> {
        let Some(repo) = &self.repo else {
            return Ok(None);
        };
        let budget = ScanBudget::start(clock, diag);
        let issues = self.client.list_issues(repo, "open", &self.source_label)?;
        for issue in issues {
            if !self.eligible(&issue) {
                continue;
            }
            if budget.spent() {
                return Ok(None);
            }
            match claim_issue(&*self.client, repo, issue.number, me, clock, diag)? {
                Claimed::Won(claim_id) => {
                    // The claim is the marker; the label is the status it shows — and the
                    // re-pick gate `eligible` reads, so the kind adds it itself rather
                    // than trusting an `on_claim` block to.
                    if let Err(e) = self.client.add_label(repo, issue.number, CLAIMED_LABEL) {
                        diag.err(&e);
                    }
                    // First claim of this issue: `on_claim` runs now (no run yet, so
                    // neutral facts).
                    if let Err(e) = apply_actions(
                        &*self.client,
                        repo,
                        issue.number,
                        &self.on_claim,
                        me,
                        &Facts::none(),
                    ) {
                        diag.err(&e);
                    }
                    return Ok(Some(Unit {
                        repo: repo.clone(),
                        number: issue.number,
                        claim_id,
                        title: issue.title,
                        body: issue.body,
                        claimed_as: me.to_string(),
                    }));
                }
                // Lost the race for this issue; try the next one.
                Claimed::Lost => continue,
            }
        }
        Ok(None)
    }

    /// The unit as it crosses the wire: the built-in's `unit_key` / `unit_thread` /
    /// `unit_env` ∪ `creds_env` / `scratch_layout`, with the claim identity's login as
    /// `self`. `seen` is empty: the issue claim reads no comments, so afkd's watch takes
    /// its own baseline on its first read. The brief is unframed — afkd frames `task.md`
    /// itself.
    pub(crate) fn wire_unit(&self, unit: &Unit) -> WireUnit {
        let mut env = self.creds.clone();
        env.insert(ENV_REPO.to_string(), unit.repo.full_name());
        env.insert(ENV_ISSUE_NUMBER.to_string(), unit.number.to_string());
        WireUnit {
            id: unit.number.to_string(),
            key: unit.key(),
            thread: unit.thread(),
            seen: Vec::new(),
            me: unit.claimed_as.clone(),
            env,
            files: vec![
                WireFile {
                    path: TASK_FILE.to_string(),
                    text: brief_text(&unit.title, &unit.body),
                },
                WireFile {
                    path: format!("{ISSUE_DIR}/{NUMBER_FILE}"),
                    text: unit.number.to_string(),
                },
            ],
        }
    }

    /// The terminal lifecycle: `on_done` (clean) or `on_fail` (exhausted). This kind has
    /// no clarification gate or `on_park`, so a park runs `on_fail`, as the built-in does.
    /// Returns whether the moment reached the remote.
    pub(crate) fn finish(
        &self,
        unit: &Unit,
        outcome: UnitOutcome,
        facts: &Facts,
        diag: &dyn Diag,
    ) -> bool {
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
        // The claim is over however it ended, so its marker goes — a finished unit must
        // leave no marker to out-order the next claim. Best-effort: a leaked marker ages
        // out after `CLAIM_LIFETIME`.
        delete_marker(&*self.client, &unit.repo, unit.claim_id, diag);
        delivered
    }
}

/// The `github` kind behind the plugin's seam.
impl Units for IssueUnits {
    type Unit = Unit;

    /// It neither classifies an attempt nor marks a failed one — the built-in keeps the
    /// spine's defaults for both — so `classify` and `attempt_failed` are not among them.
    const CALLS: &'static [&'static str] = &["release", "renew", "comments"];

    fn resolve_me(&self) -> Result<String, GithubError> {
        Ok(self.client.current_user()?.login)
    }

    fn try_claim_next(
        &self,
        me: &str,
        diag: &dyn Diag,
        clock: &dyn Clock,
    ) -> Result<Option<Unit>, GithubError> {
        IssueUnits::try_claim_next(self, me, diag, clock)
    }

    fn wire_unit(&self, unit: &Unit) -> WireUnit {
        IssueUnits::wire_unit(self, unit)
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

    /// Release one stale claim named by the whole journal key: remove the `afkd/claimed`
    /// status label, take back only the bot's own assignee row, and delete the crashed
    /// run's marker, so the issue is re-claimable and its marker cannot out-order the next
    /// claim.
    fn release_stale(&self, key: &str, me: &str, diag: &dyn Diag) -> Option<bool> {
        release_stale(&*self.client, key, me, diag)
    }

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

    fn comments(&self, unit: &Unit) -> Result<Vec<IssueComment>, GithubError> {
        self.client.list_issue_comments(&unit.repo, unit.number)
    }

    fn finish(&self, unit: &Unit, outcome: UnitOutcome, facts: &Facts, diag: &dyn Diag) -> bool {
        IssueUnits::finish(self, unit, outcome, facts, diag)
    }
}

/// The brief written to `task.md`: the title, or the title, a blank line, then the body.
fn brief_text(title: &str, body: &str) -> String {
    if body.trim().is_empty() {
        title.to_string()
    } else {
        format!("{title}\n\n{body}")
    }
}

#[cfg(test)]
mod tests {
    //! No network: every test drives the in-memory `MockClient` and a fake clock.
    //!
    //! What is proven here is the **vendor half** — eligibility, the claim, the brief, the
    //! env, the terminal lifecycle. The drive around it (attempt counting, the claim
    //! journal, the run-name mint, the cadence, the framing of `task.md`) is afkd's, on
    //! the far side of the wire; the wire itself is `tests/wire.rs`.

    use super::*;
    use crate::claim::{claim_renewal_text, claim_text, is_claim, split_claim_key, CLAIM_SETTLE};
    use crate::client::{Action, MockClient};
    use crate::common::{CaptureDiag, FakeClock};
    use std::sync::Arc;

    fn cfg(
        on_claim: Vec<LifecycleAction>,
        on_done: Vec<LifecycleAction>,
        on_fail: Vec<LifecycleAction>,
    ) -> GithubConfig {
        GithubConfig {
            host: "github.com".into(),
            repo: "acme/widgets".into(),
            token: "PAT".into(),
            source_label: "afkd/ready".into(),
            on_claim,
            on_done,
            on_fail,
            ..GithubConfig::default()
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
        fn new(cfg: GithubConfig) -> Self {
            let client = Arc::new(MockClient::new("me"));
            let units = IssueUnits::new(Box::new(Arc::clone(&client)), &cfg);
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

    /// The second a claim race is staged in, frozen on the mock's post clock so a rival
    /// marker can be placed a known distance either side of our own.
    const T: u64 = 1_700_000_000;

    /// The `[afkd-claim]` marker bodies on issue `number`, read through the trait's own
    /// list call — the same thing the claim reads.
    fn claim_markers_on(h: &Harness, number: u64) -> Vec<String> {
        h.client
            .list_issue_comments(&repo(), number)
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
            claimed_as: "me".into(),
        }
    }

    // --- Eligibility + the claim ---

    #[test]
    fn only_open_source_labelled_unclaimed_issues_are_eligible() {
        let h = Harness::new(cfg(vec![], vec![], vec![]));
        // Eligible.
        h.client.add_issue(1, "Fix", "do it", &["afkd/ready"]);
        // Already claimed → never re-picked.
        h.client
            .add_issue(2, "Other", "", &["afkd/ready", "afkd/claimed"]);
        // Closed → passed over.
        h.client.add_issue(3, "Done", "", &["afkd/ready"]);
        h.client.close(3);
        // Not source-labelled → not listed.
        h.client.add_issue(4, "Someone else's", "", &[]);

        let unit = h.poll().expect("claimed");
        assert_eq!(unit.number, 1);
        // The claimed issue (2), the closed one and the unlabelled one are never picked,
        // even on a second poll.
        assert!(h.poll().is_none());
        for n in [2, 3, 4] {
            assert_eq!(claim_markers_on(&h, n), Vec::<String>::new(), "#{n}");
        }
    }

    /// With `source_label` unset the label filter is empty, so every open issue is a
    /// candidate — the key is what narrows intake.
    #[test]
    fn without_a_source_label_every_open_issue_is_a_candidate() {
        let mut c = cfg(vec![], vec![], vec![]);
        c.source_label = String::new();
        let h = Harness::new(c);
        h.client.add_issue(4, "Unlabelled", "do it", &[]);
        assert_eq!(h.poll().expect("claimed").number, 4);
    }

    #[test]
    fn claim_runs_on_claim_assign_and_label() {
        let h = Harness::new(cfg(
            vec![
                LifecycleAction::AssignMe,
                LifecycleAction::LabelAdd("afkd/working".into()),
            ],
            vec![],
            vec![],
        ));
        h.client.add_issue(1, "Fix", "do it", &["afkd/ready"]);

        h.poll().expect("claimed");
        // `on_claim` assigned us (by login) and added the working label, after the kind's
        // own status label.
        let writes: Vec<Action> = h
            .client
            .actions()
            .into_iter()
            .filter(|a| !matches!(a, Action::Comment { .. }))
            .collect();
        assert_eq!(
            writes,
            [
                Action::Label {
                    index: 1,
                    name: "afkd/claimed".into()
                },
                Action::AddAssignees {
                    index: 1,
                    assignees: vec!["me".into()]
                },
                Action::Label {
                    index: 1,
                    name: "afkd/working".into()
                },
            ]
        );
        assert_eq!(h.clock.sleeps(), [CLAIM_SETTLE]);
    }

    /// The claim's status half is the kind's own: a won claim adds `afkd/claimed` (the
    /// re-pick gate) even when no `on_claim` block spells it.
    #[test]
    fn a_won_claim_labels_the_issue_even_with_an_empty_on_claim() {
        let h = Harness::new(cfg(vec![], vec![], vec![]));
        h.client.add_issue(1, "Fix", "do it", &["afkd/ready"]);

        h.poll().expect("claimed");
        assert!(h.client.has_label(1, "afkd/claimed"));
    }

    /// A human assignee is **not** a rival claimant: an eligible issue a person already
    /// assigned themselves to is claimed and run — nothing in the claim path reads the
    /// assignee set — and the human keeps their assignment (GitHub's assign is additive).
    #[test]
    fn an_issue_with_a_human_assignee_is_still_claimable() {
        let h = Harness::new(cfg(vec![LifecycleAction::AssignMe], vec![], vec![]));
        h.client
            .add_issue_assigned(1, "Fix", "do it", &["afkd/ready"], &["陳大文"]);

        let unit = h.poll().expect("an issue a human is on is still claimable");
        assert_eq!(unit.number, 1);
        assert_eq!(
            h.client.assignees_of(1),
            vec!["陳大文".to_string(), "me".to_string()]
        );
        assert!(
            !h.client.actions().iter().any(|a| matches!(
                a,
                Action::Unlabel { .. }
                    | Action::RemoveAssignees { .. }
                    | Action::DeleteComment { .. }
            )),
            "a won claim releases nothing: {:?}",
            h.client.actions()
        );
        assert!(h.client.has_label(1, "afkd/claimed"), "and shows status");
    }

    #[test]
    fn lost_claim_race_releases_the_marker_and_skips() {
        let h = Harness::new(cfg(vec![LifecycleAction::AssignMe], vec![], vec![]));
        h.client.add_issue(1, "Fix", "do it", &["afkd/ready"]);
        // A rival's marker lands between our post and our re-read, one second ahead of
        // ours in the order and well inside the claim lifetime.
        h.client.set_clock(T);
        h.client.rival_claims_next("rival", 42, T - 1);

        assert!(h.poll().is_none());
        // The status label was never applied, `on_claim` never ran, and our marker went
        // with the loss.
        assert!(!h.client.has_label(1, "afkd/claimed"));
        assert!(h.client.assignees_of(1).is_empty());
        assert_eq!(
            claim_markers_on(&h, 1),
            vec![claim_text("rival")],
            "only the winner's marker is left"
        );
    }

    /// The `Lost` arm continues the candidate loop rather than ending the poll: the rival
    /// takes the first issue, so the *second* eligible one is claimed — and the first is
    /// left with neither our marker nor the claimed label.
    #[test]
    fn a_lost_claim_moves_on_to_the_next_candidate() {
        let h = Harness::new(cfg(vec![], vec![], vec![]));
        h.client.add_issue(1, "Fix", "do it", &["afkd/ready"]);
        h.client.add_issue(2, "Other", "do that", &["afkd/ready"]);
        // Armed for the next comment read, which is issue #1's claim re-read.
        h.client.set_clock(T);
        h.client.rival_claims_next("rival", 42, T - 1);

        let unit = h.poll().expect("the second issue is claimed");
        assert_eq!(unit.number, 2);
        assert_eq!(
            claim_markers_on(&h, 1),
            vec![claim_text("rival")],
            "issue #1 keeps only the rival's marker"
        );
        assert!(!h.client.has_label(1, "afkd/claimed"), "#1 was left alone");
        assert!(h.client.has_label(2, "afkd/claimed"));
        assert_eq!(h.clock.sleeps(), [CLAIM_SETTLE, CLAIM_SETTLE]);
    }

    /// A finished unit leaves no marker: the release runs whatever the terminal moment
    /// did, so the next claim of this issue is not out-ordered by a marker nobody is
    /// acting on. All three dispositions are driven.
    #[test]
    fn a_finished_unit_leaves_no_claim_marker() {
        for outcome in [UnitOutcome::Clean, UnitOutcome::Failed, UnitOutcome::Park] {
            let h = Harness::new(cfg(
                vec![],
                vec![LifecycleAction::Close],
                vec![LifecycleAction::Unassign],
            ));
            h.client.add_issue(1, "Fix", "do it", &["afkd/ready"]);

            let unit = h.poll().expect("claimed");
            assert_eq!(claim_markers_on(&h, 1).len(), 1, "the claim is held");

            assert!(h.finish(&unit, outcome, &Facts::none()));
            assert!(
                claim_markers_on(&h, 1).is_empty(),
                "a run ending {outcome:?} released its marker: {:?}",
                claim_markers_on(&h, 1)
            );
        }
    }

    /// The journal key carries the claim, the session thread does not: two successive
    /// claims of the same issue give two different keys (each naming its own marker) and
    /// one identical thread, so an agent session resumes per issue.
    #[test]
    fn the_journal_key_carries_the_claim_but_the_thread_does_not() {
        let h = Harness::new(cfg(vec![], vec![], vec![]));
        h.client.add_issue(7, "Fix", "do it", &["afkd/ready"]);

        let first = h.poll().expect("claimed");
        // Wind the claim down as a finished run does, so the issue is claimable again.
        h.finish(&first, UnitOutcome::Clean, &Facts::none());
        h.client
            .remove_label(&repo(), 7, CLAIMED_LABEL)
            .expect("drop the re-pick gate");
        let second = h.poll().expect("re-claimed");

        let (key_a, key_b) = (first.key(), second.key());
        assert_ne!(key_a, key_b, "a fresh claim is a fresh journal key");
        assert_eq!(first.thread(), second.thread());
        assert_eq!(first.thread(), "acme/widgets#7");
        // Each key round-trips to the coordinate + the marker the reaper deletes.
        for (key, unit) in [(&key_a, &first), (&key_b, &second)] {
            assert_eq!(
                split_claim_key(key),
                Some(("acme/widgets", 7, unit.claim_id))
            );
        }
    }

    /// A `repo` with no `owner/name` shape resolves to no target: a poll claims nothing,
    /// and asks the forge nothing.
    #[test]
    fn an_unparseable_repo_claims_nothing() {
        let mut c = cfg(vec![], vec![], vec![]);
        c.repo = "not-a-repo".into();
        let h = Harness::new(c);
        h.client.add_issue(1, "Fix", "do it", &["afkd/ready"]);
        h.client.fail("list issues");
        assert!(h.poll().is_none());
        assert!(h.diag.lines().is_empty(), "{:?}", h.diag.lines());
        assert!(h.client.actions().is_empty());
    }

    // --- Lifecycle on terminal outcomes ---

    #[test]
    fn clean_run_applies_on_done_close() {
        let h = Harness::new(cfg(vec![], vec![LifecycleAction::Close], vec![]));
        h.client.add_issue(1, "Fix", "do it", &["afkd/ready"]);

        assert!(h.finish(&unit(1), UnitOutcome::Clean, &Facts::none()));

        assert!(h
            .client
            .actions()
            .iter()
            .any(|a| matches!(a, Action::State { state, .. } if state == "closed")));
    }

    #[test]
    fn failed_run_applies_on_fail_label_remove_by_name_and_unassign() {
        let h = Harness::new(cfg(
            vec![],
            vec![],
            vec![
                LifecycleAction::LabelRemove("afkd/claimed".into()),
                LifecycleAction::Unassign,
            ],
        ));
        h.client.add_issue_assigned(
            1,
            "Fix",
            "do it",
            &["afkd/ready", "afkd/claimed"],
            &["me", "josefandersson"],
        );

        assert!(h.finish(&unit(1), UnitOutcome::Failed, &Facts::none()));

        // `on_fail` removed the claimed label BY NAME (not the all-clearing path) and took
        // the bot back out of the assignees — only the bot.
        assert!(h
            .client
            .actions()
            .iter()
            .any(|a| matches!(a, Action::Unlabel { name, .. } if name == "afkd/claimed")));
        assert!(h.client.actions().iter().any(|a| matches!(
            a,
            Action::RemoveAssignees { assignees, .. } if assignees == &vec!["me".to_string()]
        )));
        assert_eq!(h.client.assignees_of(1), vec!["josefandersson".to_string()]);
        assert!(!h.client.has_label(1, "afkd/claimed"));
    }

    /// GitHub has no `on_park`: a parked attempt runs `on_fail`, with the run's facts.
    #[test]
    fn a_park_runs_on_fail_with_the_run_facts() {
        let h = Harness::new(cfg(
            vec![],
            vec![LifecycleAction::Close],
            vec![LifecycleAction::Comment(
                "Stopped after @{run:duration}: over to a human.".into(),
            )],
        ));
        h.client.add_issue(1, "Fix", "do it", &["afkd/ready"]);
        let facts = Facts {
            duration_ms: 168_000,
            ..Facts::none()
        };

        assert!(h.finish(&unit(1), UnitOutcome::Park, &facts));

        assert!(h.client.actions().iter().any(|a| matches!(
            a,
            Action::Comment { body, .. } if body == "Stopped after 2m48s: over to a human."
        )));
        assert!(
            !h.client
                .actions()
                .iter()
                .any(|a| matches!(a, Action::State { .. })),
            "`on_done` did not run"
        );
    }

    /// A terminal moment that did not land answers `false` — the plugin's `held` — and
    /// the marker still goes: the lock is the label, and afkd's later `release` takes it.
    #[test]
    fn an_undelivered_on_done_returns_false_and_still_drops_the_marker() {
        let h = Harness::new(cfg(vec![], vec![LifecycleAction::Close], vec![]));
        h.client.add_issue(1, "Fix", "do it", &["afkd/ready"]);
        let unit = h.poll().expect("claimed");
        h.client.fail("set state");

        assert!(!h.finish(&unit, UnitOutcome::Clean, &Facts::none()));

        assert_eq!(
            h.diag.lines(),
            ["github set state: no response (mock failure)"]
        );
        assert!(claim_markers_on(&h, 1).is_empty());
        assert!(h.client.has_label(1, "afkd/claimed"));
    }

    // --- Env + scratch threading ---

    #[test]
    fn the_wire_unit_carries_the_built_ins_key_thread_env_and_layout() {
        let h = Harness::new(cfg(vec![], vec![], vec![]));
        let unit = Unit {
            claim_id: 1_000_001,
            title: "修复 the retry storm 🚨".into(),
            body: "It retries forever.\n\n```rust\nlet backoff = 0;\n```".into(),
            claimed_as: "björn-öst[bot]".into(),
            ..unit(4)
        };
        let wire = h.units.wire_unit(&unit);

        // The claim-journal key the built-in wrote, and the session thread it resumes
        // under — both unchanged across the switch.
        assert_eq!(wire.id, "4");
        assert_eq!(wire.key, "acme/widgets#4#1000001");
        assert_eq!(wire.thread, "acme/widgets#4");
        assert!(wire.seen.is_empty());
        assert_eq!(wire.me, "björn-öst[bot]");
        // `unit_env` ∪ `creds_env`: exactly the four names the skill's scripts read, the
        // host as written.
        assert_eq!(
            wire.env,
            BTreeMap::from([
                ("GITHUB_HOST".to_string(), "github.com".to_string()),
                ("GITHUB_ISSUE_NUMBER".to_string(), "4".to_string()),
                ("GITHUB_REPO".to_string(), "acme/widgets".to_string()),
                ("GITHUB_TOKEN".to_string(), "PAT".to_string()),
            ])
        );
        // `scratch_layout`, the brief unframed: afkd frames `task.md` itself.
        assert_eq!(
            wire.files,
            [
                WireFile {
                    path: "task.md".into(),
                    text: "修复 the retry storm 🚨\n\nIt retries forever.\n\n```rust\nlet backoff = 0;\n```"
                        .into(),
                },
                WireFile {
                    path: "issue/number".into(),
                    text: "4".into(),
                },
            ]
        );
    }

    #[test]
    fn a_bodyless_issue_briefs_with_just_the_title() {
        let h = Harness::new(cfg(vec![], vec![], vec![]));
        let bare = Unit {
            body: "  \n ".into(),
            title: "Just a title".into(),
            ..unit(9)
        };
        assert_eq!(h.units.wire_unit(&bare).files[0].text, "Just a title");
    }

    // --- Swallowed errors, renewal, and the poll's time budget ---

    /// A forge error on the listing is the beat's error — the plugin's idle beat — and
    /// the next beat, with the forge back, claims.
    #[test]
    fn a_poll_forge_error_is_returned_and_the_next_poll_claims() {
        let h = Harness::new(cfg(vec![], vec![], vec![]));
        h.client.add_issue(1, "Fix", "do it", &["afkd/ready"]);
        h.client.fail("list issues");

        let err = h
            .units
            .try_claim_next("me", &h.diag, &h.clock)
            .expect_err("the listing failed");
        assert_eq!(err.stage(), "list issues");
        h.client.clear_failure();
        assert!(h.poll().is_some());
    }

    #[test]
    fn an_on_claim_failure_is_logged_but_the_issue_is_still_taken_on() {
        // `on_claim` runs after a won claim; a failure there is logged and swallowed, and
        // the unit is still returned. A `close` in `on_claim` the claim itself does not
        // perform is failed here — and so is the status label, which is logged the same.
        let h = Harness::new(cfg(vec![LifecycleAction::Close], vec![], vec![]));
        h.client.add_issue(1, "Fix", "do it", &["afkd/ready"]);
        h.client.fail("set state");

        let unit = h.poll().expect("claimed anyway");
        assert_eq!(unit.number, 1);
        assert_eq!(
            h.diag.lines(),
            ["github set state: no response (mock failure)"]
        );

        let h = Harness::new(cfg(vec![], vec![], vec![]));
        h.client.add_issue(1, "Fix", "do it", &["afkd/ready"]);
        h.client.fail("add label");
        assert_eq!(h.poll().expect("claimed anyway").number, 1);
        assert_eq!(
            h.diag.lines(),
            ["github add label: no response (mock failure)"]
        );
    }

    /// The renewal edits the claim marker **in place** — same id, a body that still reads
    /// as a claim — and the forge's last-touched stamp moves with it, which is the whole
    /// liveness signal.
    #[test]
    fn renewing_a_claim_edits_the_marker_in_place() {
        let h = Harness::new(cfg(vec![], vec![], vec![]));
        h.client.add_issue(1, "Fix", "do it", &["afkd/ready"]);
        h.client
            .add_comment_body(1, 9, "me", &claim_text("me"), 100);
        let unit = Unit {
            claim_id: 9,
            ..unit(1)
        };

        h.units.renew(&unit, 3, &h.diag);

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
            .list_issue_comments(&repo(), 1)
            .expect("read the thread");
        assert_eq!(thread.len(), 1, "a renewal minted a second comment");
        assert_eq!(thread[0].id, 9);
        assert_eq!(thread[0].body, renewed);
        assert!(
            thread[0].updated_at > thread[0].created_at,
            "the liveness half did not move"
        );
    }

    /// A renewal the forge refuses is one diagnostic line and nothing else.
    #[test]
    fn a_failing_renewal_raises_one_diagnostic_and_nothing_else() {
        let h = Harness::new(cfg(vec![], vec![], vec![]));
        h.client.add_issue(1, "Fix", "do it", &["afkd/ready"]);
        let claim = claim_text("me");
        h.client.add_comment_body(1, 9, "me", &claim, 100);
        h.client.fail("edit comment");
        let unit = Unit {
            claim_id: 9,
            ..unit(1)
        };

        h.units.renew(&unit, 1, &h.diag);

        assert_eq!(
            h.diag.lines(),
            ["github edit comment: no response (mock failure)"]
        );
        let thread = h
            .client
            .list_issue_comments(&repo(), 1)
            .expect("read the thread");
        assert_eq!(thread[0].body, claim, "a failed renewal changed the marker");
    }

    /// A scan that runs past [`POLL_BUDGET`](crate::common::POLL_BUDGET) stops claiming
    /// and says so, well inside afkd's 60-second call deadline. Every candidate here loses
    /// its race to a live rival, and each attempt settles one second, so the twenty-first
    /// candidate is the first the budget turns away — and nothing after it is posted to.
    #[test]
    fn the_poll_budget_ends_the_scan_and_says_so() {
        let h = Harness::new(cfg(vec![], vec![], vec![]));
        h.client.set_clock(T);
        for n in 1..=25 {
            h.client.add_issue(
                n,
                "修复 the retry storm 🚨",
                "It retries forever.",
                &["afkd/ready"],
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
}
