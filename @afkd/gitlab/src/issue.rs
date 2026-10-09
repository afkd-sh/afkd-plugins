//! The `issue` kind's vendor half (ADR-0041) — `service(gitlab)` in a config — ported from
//! afkd's `crates/gitlab/src/trigger_issue.rs`.
//!
//! It polls a single project through the mockable [`GitlabClient`] seam for issues that
//! are **open** and carry the configured **source label**; claims one with a
//! `[afkd-claim]` marker note (post → settle → re-read → decide, then the `afkd::claimed`
//! status label); and does each action afkd's hooks call through GitLab's native
//! assignee/label/state primitives. Its logic is unit-tested with **no network** against
//! [`MockClient`](crate::client::MockClient) and a fake clock.
//!
//! afkd keeps the spine — the cadence, the queue lane, the claim journal, the attempt
//! bound, the mid-run watch's cursor, and the framing of `task.md` — so what is here is
//! exactly what the built-in's `ForgeUnits` impl does on the vendor side.
//!
//! Work is only ever claimed from the source-labelled, unclaimed set; an issue already
//! carrying `afkd::claimed` is never re-picked. With `source_label` unset the label filter
//! is empty, so that set is *every* open issue — the key is what narrows intake. There is
//! no group-wide polling: a GitLab trigger addresses a single `project`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::client::{GitlabClient, GitlabError, Issue, ItemKind, Note, Project, User};
use crate::common::{
    claim_item, claim_key_for, creds_env, lock, release_claim, release_stale, renew_marker,
    unit_key, Claimed, Clock, Diag, ScanBudget, CLAIMED_LABEL, ENV_ISSUE_NUMBER, ENV_PROJECT,
};
use crate::kind::{ClaimedUnit, Units};
use crate::lifecycle::{Vocabulary, ISSUE_VOCABULARY};
use crate::outbox::{lost_line, Item, Outbox, Write};
use crate::settings::GitlabConfig;
use crate::wire::{Fields, IssueFields, WireFile, WireUnit};
use crate::{ISSUE_DIR, NUMBER_FILE, TASK_FILE};

/// One issue taken on as a unit of work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Unit {
    pub(crate) project: Project,
    pub(crate) iid: u64,
    /// The note id of the `[afkd-claim]` marker this claim holds the issue with —
    /// released at run end, and the tail of the claim-journal key so a crashed run's
    /// marker is reaped.
    pub(crate) claim_id: u64,
    pub(crate) title: String,
    /// The issue's page on GitLab, the handle's `url`.
    pub(crate) url: String,
    /// The issue's label names as the claiming poll read them — before the claim's own
    /// `afkd/claimed` — the handle's `labels`.
    pub(crate) labels: Vec<String>,
    pub(crate) body: String,
    /// The user this issue was claimed as: the unit's `self` (its username), and the
    /// identity the hooks' actions assign and unassign (its id).
    pub(crate) claimed_as: User,
}

impl Unit {
    /// The claim-journal key — the wire unit's `key`, and what every later call names it
    /// by. It carries the claim marker, so it changes on every claim of the issue.
    pub(crate) fn key(&self) -> String {
        claim_key_for(&self.project, self.iid, self.claim_id)
    }

    /// The issue's stable coordinate, `<project>#<iid>` (ADR-0067): the wire unit's
    /// `thread`, the same across every claim, so an agent session resumes per issue.
    pub(crate) fn thread(&self) -> String {
        unit_key(&self.project, self.iid)
    }
}

impl ClaimedUnit for Unit {
    fn thread(&self) -> String {
        Unit::thread(self)
    }

    fn claimed_as(&self) -> &str {
        &self.claimed_as.username
    }

    fn item(&self) -> Item {
        Item {
            noun: "issue",
            project: self.project.clone(),
            kind: ItemKind::Issue,
            iid: self.iid,
            title: self.title.clone(),
            me: self.claimed_as.clone(),
        }
    }
}

/// The `issue` kind's vendor half: the single-project target, the source-label intake
/// gate, the claim, and the three lifecycle action lists.
pub(crate) struct IssueUnits {
    client: Box<dyn GitlabClient>,
    project: Project,
    source_label: String,
    /// The token and base URL every unit's `env` carries.
    creds: BTreeMap<String, String>,
    /// The current call's deadline, `None` between calls: the poll's scan reads it for
    /// its claim reserve.
    call_deadline: Mutex<Option<Instant>>,
    /// The writes the forge could not take, which the plugin's worker retries.
    outbox: Arc<Outbox>,
}

impl IssueUnits {
    pub(crate) fn new(client: Box<dyn GitlabClient>, cfg: &GitlabConfig) -> Self {
        Self {
            client,
            project: Project::new(cfg.project.clone()),
            source_label: cfg.source_label.clone(),
            creds: creds_env(cfg),
            call_deadline: Mutex::new(None),
            outbox: Arc::default(),
        }
    }

    /// Whether `issue` is claimable: open and not already `afkd::claimed`. (The
    /// `state`/`labels` filter is also applied server-side, but the guard is enforced
    /// here too.)
    fn eligible(&self, issue: &Issue) -> bool {
        issue.state == "opened" && !issue.has_label(CLAIMED_LABEL)
    }

    /// Find the first eligible issue in the polled project and claim it — the claim race
    /// runs here, inside afkd's `poll`.
    ///
    /// The scan stops once it has run [`POLL_BUDGET`](crate::common::POLL_BUDGET), checked
    /// before each claim attempt, and answers "nothing this beat".
    ///
    /// It stops the same way, before any claim is posted, once less than
    /// [`CLAIM_RESERVE`](crate::common::CLAIM_RESERVE) of the call's budget is left. A
    /// claim already under way always finishes.
    pub(crate) fn try_claim_next(
        &self,
        me: &User,
        diag: &dyn Diag,
        clock: &dyn Clock,
    ) -> Result<Option<Unit>, GitlabError> {
        let budget = ScanBudget::start(clock, diag, *lock(&self.call_deadline));
        let issues = self
            .client
            .list_issues(&self.project, "opened", &self.source_label)?;
        for issue in issues {
            if !self.eligible(&issue) {
                continue;
            }
            if budget.spent() {
                return Ok(None);
            }
            match claim_item(
                &*self.client,
                &self.project,
                ItemKind::Issue,
                issue.iid,
                me,
                clock,
                diag,
            )? {
                Claimed::Won(claim_id) => {
                    // The claim is the marker; the label is the status it shows — and the
                    // re-pick gate `eligible` reads, so the kind adds it itself rather
                    // than trusting an `on_claim` hook to.
                    if let Err(e) = self.client.add_label(
                        &self.project,
                        ItemKind::Issue,
                        issue.iid,
                        CLAIMED_LABEL,
                    ) {
                        diag.err(&e);
                    }
                    return Ok(Some(Unit {
                        project: self.project.clone(),
                        iid: issue.iid,
                        claim_id,
                        title: issue.title,
                        url: issue.url,
                        labels: issue.labels,
                        body: issue.body,
                        claimed_as: me.clone(),
                    }));
                }
                // Lost the race for this issue; try the next one.
                Claimed::Lost => continue,
            }
        }
        Ok(None)
    }

    /// The unit as it crosses the wire: the built-in's `unit_key` / `unit_thread` /
    /// `unit_env` ∪ `creds_env` / `scratch_layout`, with the claim identity's username as
    /// `self` and the issue's handle fields. `seen` is empty: the issue claim reads no
    /// notes, so afkd's watch takes its own baseline on its first read. The brief is
    /// unframed — afkd frames `task.md` itself.
    pub(crate) fn wire_unit(&self, unit: &Unit) -> WireUnit {
        let mut env = self.creds.clone();
        env.insert(ENV_PROJECT.to_string(), unit.project.raw().to_string());
        env.insert(ENV_ISSUE_NUMBER.to_string(), unit.iid.to_string());
        WireUnit {
            id: unit.iid.to_string(),
            key: unit.key(),
            fields: Fields::Issue(IssueFields {
                title: unit.title.clone(),
                url: unit.url.clone(),
                number: unit.iid,
                labels: unit.labels.clone(),
            }),
            thread: unit.thread(),
            seen: Vec::new(),
            me: unit.claimed_as.username.clone(),
            env,
            files: vec![
                WireFile {
                    path: TASK_FILE.to_string(),
                    text: brief_text(&unit.title, &unit.body),
                },
                WireFile {
                    path: format!("{ISSUE_DIR}/{NUMBER_FILE}"),
                    text: unit.iid.to_string(),
                },
            ],
        }
    }
}

/// The `issue` kind behind the plugin's seam.
impl Units for IssueUnits {
    type Unit = Unit;

    /// It neither classifies an attempt nor marks a failed one — the built-in keeps the
    /// spine's defaults for both — so `classify` and `attempt_failed` are not among them.
    const CALLS: &'static [&'static str] = &["release", "renew", "comments"];

    const VOCABULARY: Vocabulary = ISSUE_VOCABULARY;

    fn set_call_deadline(&self, deadline: Option<Instant>) {
        *lock(&self.call_deadline) = deadline;
        self.client.set_call_deadline(deadline);
    }

    fn resolve_me(&self) -> Result<User, GitlabError> {
        self.client.current_user()
    }

    fn try_claim_next(
        &self,
        me: &User,
        diag: &dyn Diag,
        clock: &dyn Clock,
    ) -> Result<Option<Unit>, GitlabError> {
        IssueUnits::try_claim_next(self, me, diag, clock)
    }

    fn wire_unit(&self, unit: &Unit) -> WireUnit {
        IssueUnits::wire_unit(self, unit)
    }

    fn release(&self, unit: &Unit, diag: &dyn Diag) {
        release_claim(
            &*self.client,
            &unit.project,
            ItemKind::Issue,
            unit.iid,
            &unit.claimed_as,
            unit.claim_id,
            diag,
        );
    }

    /// Release one stale claim named by the whole journal key: remove the
    /// `afkd::claimed` status label, take back only the bot's own assignee row, and
    /// delete the crashed run's marker. GitLab is single-project, so the release targets
    /// this kind's own project.
    fn release_stale(&self, key: &str, me: &User, diag: &dyn Diag) -> Option<bool> {
        release_stale(&*self.client, &self.project, ItemKind::Issue, key, me, diag)
    }

    fn renew(&self, unit: &Unit, renewal: u64, diag: &dyn Diag) {
        renew_marker(
            &*self.client,
            &unit.project,
            ItemKind::Issue,
            unit.iid,
            unit.claim_id,
            &unit.claimed_as.username,
            renewal,
            diag,
        );
    }

    fn comments(&self, unit: &Unit) -> Result<Vec<Note>, GitlabError> {
        self.client
            .list_notes(&unit.project, ItemKind::Issue, unit.iid)
    }

    /// The plugin-owned end of a run, whatever its outcome: the claim marker's release,
    /// so a finished unit leaves no marker to out-order the next claim. It goes through
    /// the outbox — one GitLab could not take is retried, ahead of the `on_done`/`on_fail`
    /// hook's actions, which afkd sends after this, each one `call`. Best-effort: a leaked
    /// marker ages out after `CLAIM_LIFETIME`.
    fn finish(&self, unit: &Unit, now: Instant, diag: &dyn Diag) {
        let release = Write::Release {
            marker: unit.claim_id,
        };
        if let Err(e) = self.deliver(unit, release.clone(), now, diag) {
            diag.err(&lost_line(&unit.item(), &release, &e, None));
        }
    }

    fn client(&self) -> &dyn GitlabClient {
        &*self.client
    }

    fn outbox(&self) -> &Arc<Outbox> {
        &self.outbox
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
    //! env, the end of a run and the hooks' actions. The drive around it (attempt counting,
    //! the claim journal, the run-name mint, the cadence, the framing of `task.md`) is
    //! afkd's, on the far side of the wire; the wire itself is `tests/wire.rs`.

    use super::*;
    use crate::claim::{claim_renewal_text, claim_text, is_claim, split_claim_key, CLAIM_SETTLE};
    use crate::client::{Action, MockClient};
    use crate::common::{CaptureDiag, FakeClock, CALL_BUDGET, CLAIM_RESERVE};
    use crate::lifecycle::LifecycleAction;
    use std::time::Duration;

    fn cfg() -> GitlabConfig {
        GitlabConfig {
            base_url: "https://gitlab.example.com".into(),
            project: "group/widgets".into(),
            token: "PAT".into(),
            source_label: "afkd::ready".into(),
            author_me: false,
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
        fn new(cfg: GitlabConfig) -> Self {
            let client = Arc::new(MockClient::new(1, "me"));
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
            match self.units.try_claim_next(&me(), &self.diag, &self.clock) {
                Ok(unit) => unit,
                Err(e) => {
                    self.diag.err(&e);
                    None
                }
            }
        }

        /// The `finish` call for `unit`.
        fn finish(&self, unit: &Unit) {
            self.units.finish(unit, self.clock.now(), &self.diag);
        }

        /// One `call` of a hook's `action` on `unit`, as afkd sends it after a `poll`
        /// (`on_claim`) or a `finish` (the post-run hooks).
        fn act(&self, unit: &Unit, action: LifecycleAction) -> Result<(), GitlabError> {
            self.units
                .deliver(unit, Write::Act(action), self.clock.now(), &self.diag)
                .map(drop)
        }
    }

    fn me() -> User {
        User {
            id: 1,
            username: "me".into(),
        }
    }

    /// The second a claim race is staged in, frozen on the mock's post clock so a rival
    /// marker can be placed a known distance either side of our own.
    const T: u64 = 1_700_000_000;

    /// The `[afkd-claim]` marker bodies on issue `iid`, read through the trait's own list
    /// call — the same thing the claim reads.
    fn claim_markers_on(h: &Harness, iid: u64) -> Vec<String> {
        h.client
            .list_notes(&Project::new("group/widgets"), ItemKind::Issue, iid)
            .expect("read the thread")
            .into_iter()
            .filter(|n| is_claim(&n.body))
            .map(|n| n.body)
            .collect()
    }

    fn unit(iid: u64) -> Unit {
        Unit {
            project: Project::new("group/widgets"),
            iid,
            claim_id: 1,
            title: "T".into(),
            url: String::new(),
            labels: Vec::new(),
            body: "B".into(),
            claimed_as: me(),
        }
    }

    // --- Eligibility + the claim ---

    #[test]
    fn only_open_source_labelled_unclaimed_issues_are_eligible() {
        let h = Harness::new(cfg());
        // Eligible.
        h.client.add_issue(1, "Fix", "do it", &["afkd::ready"]);
        // Already claimed → never re-picked.
        h.client
            .add_issue(2, "Other", "", &["afkd::ready", "afkd::claimed"]);
        // Closed → passed over.
        h.client.add_issue(3, "Done", "", &["afkd::ready"]);
        h.client.close(3);
        // Not source-labelled → not listed.
        h.client.add_issue(4, "Someone else's", "", &[]);

        let unit = h.poll().expect("claimed");
        assert_eq!(unit.iid, 1);
        // The claimed issue (2), the closed one and the unlabelled one are never picked,
        // even on a second poll.
        assert!(h.poll().is_none());
        assert_eq!(claim_markers_on(&h, 2), Vec::<String>::new());
        assert_eq!(claim_markers_on(&h, 3), Vec::<String>::new());
    }

    /// With `source_label` unset the label filter is empty, so every open issue is a
    /// candidate — the key is what narrows intake.
    #[test]
    fn without_a_source_label_every_open_issue_is_a_candidate() {
        let mut c = cfg();
        c.source_label = String::new();
        let h = Harness::new(c);
        h.client.add_issue(4, "Unlabelled", "do it", &[]);
        assert_eq!(h.poll().expect("claimed").iid, 4);
    }

    /// A claim does only the plugin-owned writes — the marker and the `afkd::claimed`
    /// gate — and no hook action: afkd runs `on_claim` after the `poll`, and its actions
    /// arrive as `call`s that act as the claim identity (by id).
    #[test]
    fn a_claim_does_only_the_plugin_owned_writes_and_on_claim_acts_after() {
        let h = Harness::new(cfg());
        h.client.add_issue(1, "Fix", "do it", &["afkd::ready"]);

        let unit = h.poll().expect("claimed");
        let writes = || -> Vec<Action> {
            h.client
                .actions()
                .into_iter()
                .filter(|a| !matches!(a, Action::Comment { .. }))
                .collect()
        };
        assert_eq!(
            writes(),
            [Action::Label {
                kind: ItemKind::Issue,
                iid: 1,
                name: "afkd::claimed".into()
            }]
        );
        assert_eq!(h.clock.sleeps(), [CLAIM_SETTLE]);

        h.act(&unit, LifecycleAction::AssignMe).unwrap();
        h.act(&unit, LifecycleAction::LabelAdd("afkd::working ⚙".into()))
            .unwrap();
        assert_eq!(
            writes()[1..],
            [
                Action::Assign {
                    kind: ItemKind::Issue,
                    iid: 1,
                    ids: vec![1]
                },
                Action::Label {
                    kind: ItemKind::Issue,
                    iid: 1,
                    name: "afkd::working ⚙".into()
                },
            ]
        );
    }
    /// The claim's status half is the kind's own: a won claim adds `afkd::claimed` (the
    /// re-pick gate) with no hook calling for it.
    #[test]
    fn a_won_claim_labels_the_issue_with_no_hook() {
        let h = Harness::new(cfg());
        h.client.add_issue(1, "Fix", "do it", &["afkd::ready"]);

        h.poll().expect("claimed");
        assert!(h.client.has_label(ItemKind::Issue, 1, "afkd::claimed"));
    }

    /// A human assignee is **not** a rival claimant: an eligible issue a person already
    /// assigned themselves to is claimed and run, and the human keeps their assignment
    /// through `on_claim`'s `assign_me`, which under GitLab's replace-set `PUT` used to
    /// evict them.
    #[test]
    fn an_issue_with_a_human_assignee_is_still_claimable() {
        let h = Harness::new(cfg());
        h.client
            .add_issue_assigned(1, "Fix", "do it", &["afkd::ready"], &[(99, "陳大文")]);

        let unit = h.poll().expect("an issue a human is on is still claimable");
        assert_eq!(unit.iid, 1);
        h.act(&unit, LifecycleAction::AssignMe).unwrap();
        assert_eq!(h.client.assignee_ids(ItemKind::Issue, 1), vec![99, 1]);
        assert!(
            !h.client
                .actions()
                .iter()
                .any(|a| matches!(a, Action::Unlabel { .. } | Action::DeleteComment { .. })),
            "a won claim releases nothing: {:?}",
            h.client.actions()
        );
        assert!(h.client.has_label(ItemKind::Issue, 1, "afkd::claimed"));
    }

    #[test]
    fn lost_claim_race_releases_the_marker_and_skips() {
        let h = Harness::new(cfg());
        h.client.add_issue(1, "Fix", "do it", &["afkd::ready"]);
        // A rival's marker lands between our post and our re-read, one second ahead of
        // ours in the order and well inside the claim lifetime.
        h.client.set_clock(T);
        h.client.rival_claims_next(99, "rival", 42, T - 1);

        assert!(h.poll().is_none());
        // The status label was never applied, no unit was handed over for `on_claim` to
        // act on, and our marker went with the loss.
        assert!(!h.client.has_label(ItemKind::Issue, 1, "afkd::claimed"));
        assert!(h.client.assignee_ids(ItemKind::Issue, 1).is_empty());
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
        let h = Harness::new(cfg());
        h.client.add_issue(1, "Fix", "do it", &["afkd::ready"]);
        h.client.add_issue(2, "Other", "do that", &["afkd::ready"]);
        // Armed for the next note read, which is issue #1's claim re-read.
        h.client.set_clock(T);
        h.client.rival_claims_next(99, "rival", 42, T - 1);

        let unit = h.poll().expect("the second issue is claimed");
        assert_eq!(unit.iid, 2);
        assert_eq!(
            claim_markers_on(&h, 1),
            vec![claim_text("rival")],
            "issue #1 keeps only the rival's marker"
        );
        assert!(!h.client.has_label(ItemKind::Issue, 1, "afkd::claimed"));
        assert!(h.client.has_label(ItemKind::Issue, 2, "afkd::claimed"));
        assert_eq!(h.clock.sleeps(), [CLAIM_SETTLE, CLAIM_SETTLE]);
    }

    /// A finished unit leaves no marker: the release runs whatever the run did, so the next
    /// claim of this issue is not out-ordered by a marker nobody is acting on — and it is
    /// the finish's only write: closing, unlabelling and unassigning are the hooks'.
    #[test]
    fn a_finish_releases_the_marker_and_does_nothing_else() {
        let h = Harness::new(cfg());
        h.client
            .add_issue_assigned(1, "Fix", "do it", &["afkd::ready"], &[(1, "me")]);

        let unit = h.poll().expect("claimed");
        assert_eq!(claim_markers_on(&h, 1).len(), 1, "the claim is held");
        let before = h.client.actions().len();

        h.finish(&unit);
        assert_eq!(
            h.client.actions()[before..],
            [Action::DeleteComment {
                kind: ItemKind::Issue,
                iid: 1,
                id: unit.claim_id
            }]
        );
        assert!(claim_markers_on(&h, 1).is_empty());
        assert!(h.client.has_label(ItemKind::Issue, 1, "afkd::claimed"));
        assert_eq!(h.client.assignee_ids(ItemKind::Issue, 1), vec![1]);
    }
    /// The journal key carries the claim, the session thread does not: two successive
    /// claims of the same issue give two different keys (each naming its own marker) and
    /// one identical thread, so an agent session resumes per issue.
    #[test]
    fn the_journal_key_carries_the_claim_but_the_thread_does_not() {
        let h = Harness::new(cfg());
        h.client.add_issue(7, "Fix", "do it", &["afkd::ready"]);

        let first = h.poll().expect("claimed");
        // Wind the claim down as a finished run does, so the issue is claimable again.
        h.finish(&first);
        h.client
            .remove_label(
                &Project::new("group/widgets"),
                ItemKind::Issue,
                7,
                CLAIMED_LABEL,
            )
            .expect("drop the re-pick gate");
        let second = h.poll().expect("re-claimed");

        let (key_a, key_b) = (first.key(), second.key());
        assert_ne!(key_a, key_b, "a fresh claim is a fresh journal key");
        assert_eq!(first.thread(), second.thread());
        assert_eq!(first.thread(), "group/widgets#7");
        // Each key round-trips to the coordinate + the marker the reaper deletes.
        for (key, unit) in [(&key_a, &first), (&key_b, &second)] {
            assert_eq!(
                split_claim_key(key),
                Some(("group/widgets", 7, unit.claim_id))
            );
        }
    }

    // --- The post-run hooks' `call`s ---

    /// Each action a post-run hook calls does its one forge operation on the finished
    /// unit's issue, as its claim identity: `label_remove` removes by **name**, `unassign`
    /// takes the bot alone back out, a comment posts its text byte for byte, and `close`
    /// sends the close state event.
    #[test]
    fn a_post_run_hooks_actions_act_on_the_units_issue() {
        let h = Harness::new(cfg());
        h.client.add_issue_assigned(
            1,
            "Fix",
            "do it",
            &["afkd::ready", "afkd::claimed"],
            &[(1, "me"), (99, "josefandersson")],
        );
        let unit = unit(1);
        h.finish(&unit);
        let text = "## 失敗 ❌\n\nran out of attempts: #{run.x}";
        for action in [
            LifecycleAction::LabelRemove("afkd::claimed".into()),
            LifecycleAction::Unassign,
            LifecycleAction::Comment(text.into()),
            LifecycleAction::Close,
        ] {
            h.act(&unit, action).unwrap();
        }

        assert!(h.client.actions().iter().any(|a| matches!(
            a,
            Action::Unlabel { name, .. } if name == "afkd::claimed"
        )));
        assert_eq!(h.client.assignee_ids(ItemKind::Issue, 1), vec![99]);
        assert!(!h.client.has_label(ItemKind::Issue, 1, "afkd::claimed"));
        assert!(h
            .client
            .actions()
            .iter()
            .any(|a| matches!(a, Action::Comment { body, .. } if body == text)));
        assert!(h.client.actions().iter().any(|a| matches!(
            a,
            Action::State { event, .. } if event == "close"
        )));
    }

    /// A marker release the forge refused is said lost in one line: the finish still
    /// ends, and a leaked marker ages out. A hook's action the forge refused is that
    /// `call`'s error, for afkd to fail the hook with, and nothing is queued.
    #[test]
    fn a_refused_release_is_diagnosed_and_a_refused_action_is_the_calls_error() {
        let h = Harness::new(cfg());
        h.client.add_issue(1, "Fix", "do it", &["afkd::ready"]);
        let unit = h.poll().expect("claimed");
        h.client.refuse("delete comment", 403);
        h.finish(&unit);
        assert_eq!(
            h.diag.lines(),
            [
                "issue \"Fix\" (group/widgets#1): deleting the claim marker is lost — gitlab \
                 delete comment: forge returned status 403"
            ]
        );

        h.client.refuse("set state", 403);
        let err = h.act(&unit, LifecycleAction::Close).unwrap_err();
        assert_eq!(
            err.to_string(),
            "gitlab set state: forge returned status 403"
        );
        assert!(!h.units.outbox().owes(&unit.thread()));
        assert!(h.client.has_label(ItemKind::Issue, 1, "afkd::claimed"));
    }

    // --- Env + scratch threading ---

    /// The handle fields come off the issue as the claiming poll listed it — its title,
    /// page and iid, and its label names before the claim's own `afkd::claimed` — and a
    /// bare issue still sends its `url` and `labels`, empty, never leaving one out.
    #[test]
    fn the_wire_unit_carries_the_issues_fields_as_the_poll_read_them() {
        let h = Harness::new(cfg());
        h.client.add_issue(
            4,
            "修复 the retry storm 🚨",
            "B",
            &["afkd::ready", "優先::high"],
        );
        let unit = h.poll().expect("claimed");
        assert_eq!(
            serde_json::to_value(h.units.wire_unit(&unit).fields).unwrap(),
            serde_json::json!({
                "title": "修复 the retry storm 🚨",
                "url": "https://gitlab.example.com/group/widgets/-/issues/4",
                "number": 4,
                "labels": ["afkd::ready", "優先::high"],
            })
        );
        assert_eq!(
            serde_json::to_value(h.units.wire_unit(&self::unit(9)).fields).unwrap(),
            serde_json::json!({"title": "T", "url": "", "number": 9, "labels": []})
        );
    }

    #[test]
    fn the_wire_unit_carries_the_built_ins_key_thread_env_and_layout() {
        let h = Harness::new(cfg());
        let unit = Unit {
            claim_id: 1_000_001,
            title: "修复 the retry storm 🚨".into(),
            body: "It retries forever.\n\n```rust\nlet backoff = 0;\n```".into(),
            claimed_as: User {
                id: 7,
                username: "björn-öst[bot]".into(),
            },
            ..unit(4)
        };
        let wire = h.units.wire_unit(&unit);

        // The claim-journal key the built-in wrote, and the session thread it resumes
        // under — both unchanged across the switch.
        assert_eq!(wire.id, "4");
        assert_eq!(wire.key, "group/widgets#4#1000001");
        assert_eq!(wire.thread, "group/widgets#4");
        assert!(wire.seen.is_empty());
        assert_eq!(wire.me, "björn-öst[bot]");
        // `unit_env` ∪ `creds_env`: exactly the four names the skill's scripts read.
        assert_eq!(
            wire.env,
            BTreeMap::from([
                (
                    "GITLAB_BASE_URL".to_string(),
                    "https://gitlab.example.com".to_string()
                ),
                ("GITLAB_ISSUE_NUMBER".to_string(), "4".to_string()),
                ("GITLAB_PROJECT".to_string(), "group/widgets".to_string()),
                ("GITLAB_TOKEN".to_string(), "PAT".to_string()),
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
        let h = Harness::new(cfg());
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
        let h = Harness::new(cfg());
        h.client.add_issue(1, "Fix", "do it", &["afkd::ready"]);
        h.client.fail("list issues");

        let err = h
            .units
            .try_claim_next(&me(), &h.diag, &h.clock)
            .expect_err("the listing failed");
        assert_eq!(err.stage(), "list issues");
        h.client.clear_failure();
        assert!(h.poll().is_some());
    }

    /// A status label the forge refused is logged and swallowed, and the unit is still
    /// taken on: the claim is the marker.
    #[test]
    fn a_status_label_failure_is_logged_but_the_issue_is_still_taken_on() {
        let h = Harness::new(cfg());
        h.client.add_issue(1, "Fix", "do it", &["afkd::ready"]);
        h.client.fail("add label");
        assert_eq!(h.poll().expect("claimed anyway").iid, 1);
        assert_eq!(
            h.diag.lines(),
            ["gitlab add label: no response (mock failure)"]
        );
    }
    /// The renewal edits the claim marker **in place** — same id, a body that still reads
    /// as a claim — and the forge's last-touched stamp moves with it.
    #[test]
    fn renewing_a_claim_edits_the_marker_in_place() {
        let h = Harness::new(cfg());
        h.client.add_issue(1, "Fix", "do it", &["afkd::ready"]);
        h.client
            .add_note_body(ItemKind::Issue, 1, 9, 1, "me", &claim_text("me"), 100);
        let unit = Unit {
            claim_id: 9,
            ..unit(1)
        };

        h.units.renew(&unit, 3, &h.diag);

        let renewed = claim_renewal_text("me", 3);
        assert_eq!(
            h.client.actions(),
            vec![Action::EditComment {
                kind: ItemKind::Issue,
                iid: 1,
                id: 9,
                body: renewed.clone(),
            }]
        );
        let thread = h
            .client
            .list_notes(&Project::new("group/widgets"), ItemKind::Issue, 1)
            .expect("read the thread");
        assert_eq!(thread.len(), 1, "a renewal minted a second note");
        assert_eq!(thread[0].id, 9);
        assert_eq!(thread[0].body, renewed);
        assert!(thread[0].updated_at > thread[0].created_at);
    }

    /// A renewal the forge refuses is one diagnostic line and nothing else.
    #[test]
    fn a_failing_renewal_raises_one_diagnostic_and_nothing_else() {
        let h = Harness::new(cfg());
        h.client.add_issue(1, "Fix", "do it", &["afkd::ready"]);
        let claim = claim_text("me");
        h.client
            .add_note_body(ItemKind::Issue, 1, 9, 1, "me", &claim, 100);
        h.client.fail("edit comment");
        let unit = Unit {
            claim_id: 9,
            ..unit(1)
        };

        h.units.renew(&unit, 1, &h.diag);

        assert_eq!(
            h.diag.lines(),
            ["gitlab edit comment: no response (mock failure)"]
        );
        let thread = h
            .client
            .list_notes(&Project::new("group/widgets"), ItemKind::Issue, 1)
            .expect("read the thread");
        assert_eq!(thread[0].body, claim, "a failed renewal changed the marker");
    }

    /// A scan that runs past [`POLL_BUDGET`](crate::common::POLL_BUDGET) stops claiming
    /// and says so, well inside afkd's 60-second call deadline. Every candidate here loses
    /// its race to a live rival, and each attempt settles one second, so the twenty-first
    /// candidate is the first the budget turns away — and nothing after it is posted to.
    #[test]
    fn the_poll_budget_ends_the_scan_and_says_so() {
        let h = Harness::new(cfg());
        h.client.set_clock(T);
        for n in 1..=25 {
            h.client.add_issue(
                n,
                "修复 the retry storm 🚨",
                "It retries forever.",
                &["afkd::ready"],
            );
            let rival = claim_text("björn-öst[bot]");
            h.client.add_note_body(
                ItemKind::Issue,
                n,
                1_000 + n,
                7,
                "björn-öst[bot]",
                &rival,
                T - 60,
            );
        }

        assert_eq!(h.poll(), None);

        let posted: Vec<u64> = h
            .client
            .actions()
            .into_iter()
            .filter_map(|a| match a {
                Action::Comment { iid, body, .. } if is_claim(&body) => Some(iid),
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
            ["gitlab poll: the scan ran past its 20s budget; the rest of it waits for the next poll"]
        );
    }

    // --- The call's claim reserve ---

    const RESERVE_LINE: &str = "gitlab poll: less than 10s of the call's 45s budget is left; the \
                                rest of the scan waits for the next poll";

    /// A call with less than the claim reserve left ends the scan before any marker is
    /// posted, idle and with one line saying why — a claimable issue waits for the next
    /// beat rather than risk a marker the call could not see through.
    #[test]
    fn a_scan_with_less_than_the_claim_reserve_left_posts_no_claim() {
        let h = Harness::new(cfg());
        h.client.add_issue(
            7,
            "Fix the 修复 path — 🚨",
            "Steps:\n1. run it\n2. ship it",
            &["afkd::ready"],
        );
        h.units.set_call_deadline(Some(
            h.clock.now() + CLAIM_RESERVE - Duration::from_millis(1),
        ));

        assert!(h.poll().is_none(), "the beat ends idle");
        assert_eq!(claim_markers_on(&h, 7), Vec::<String>::new());
        assert!(h.client.actions().is_empty(), "{:?}", h.client.actions());
        assert!(h.clock.sleeps().is_empty(), "no claim settled");
        assert_eq!(h.diag.lines(), [RESERVE_LINE]);

        // The same issue claims on a beat with the budget intact.
        h.units.set_call_deadline(Some(h.clock.now() + CALL_BUDGET));
        let unit = h.poll().expect("claimed");
        assert_eq!(unit.iid, 7);
        assert_eq!(claim_markers_on(&h, 7).len(), 1);
    }
}
