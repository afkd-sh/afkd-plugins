//! The `gitlab_mr_review` kind's vendor half (ADR-0041), ported from afkd's
//! `crates/gitlab/src/trigger_mr.rs`.
//!
//! It polls a single project's **open** merge requests through the mockable
//! [`GitlabClient`] seam — only the bot's own with `author_me` — and treats an MR as
//! eligible when it carries a **note newer than the bot's last word**
//! ([`mr_has_new_feedback`]): the watermark is the newest of the bot's own notes, claim
//! markers excepted, and the agent's reply through the `gitlab` skill is what advances it.
//! It claims one with the same `[afkd-claim]` marker note as the issue kind, writes the
//! `afkd::claimed` label as **status** only — the watermark, not the label, is the re-pick
//! gate — and loops across polls until a human merges or closes the MR, which drops it out
//! of the `state=opened` set. There is **no `on_done close`**, no park and no
//! clarification gate.
//!
//! afkd keeps the spine — the cadence, the queue lane, the claim journal, the attempt
//! bound, the mid-run watch's cursor, and the framing of `task.md` — so what is here is
//! exactly what the built-in's `ForgeUnits` impl does on the vendor side. The poll's scan
//! budget is checked where the issue kind checks it: after a candidate passes eligibility,
//! before its claim attempt.
//!
//! GitLab has no Gitea-style review list, so MR feedback is **notes** only. The
//! watermark/feedback-delta decision is in pure, unit-tested helpers ([`mr_watermark`],
//! [`mr_feedback_delta`], [`mr_has_new_feedback`]).

use std::collections::BTreeMap;
use std::time::SystemTime;

use crate::claim::is_claim;
use crate::client::{GitlabClient, GitlabError, ItemKind, Note, Project, User};
use crate::common::{
    apply_actions, claim_item, claim_key_for, creds_env, delete_marker, release_claim,
    release_stale, renew_marker, unit_key, Claimed, Clock, Diag, ScanBudget, CLAIMED_LABEL,
    ENV_MR_BRANCH, ENV_MR_NUMBER, ENV_PROJECT,
};
use crate::kind::{ClaimedUnit, Units};
use crate::lifecycle::LifecycleAction;
use crate::settings::GitlabConfig;
use crate::wire::{Facts, UnitOutcome, WireFile, WireUnit};
use crate::{MR_DIR, NUMBER_FILE, TASK_FILE};

/// One MR taken on as a unit of work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Unit {
    pub(crate) project: Project,
    pub(crate) iid: u64,
    /// The note id of the `[afkd-claim]` marker this claim holds the MR with — released at
    /// run end, and the tail of the claim-journal key so a crashed run's marker is reaped.
    pub(crate) claim_id: u64,
    /// The MR's source branch, threaded into the run's env so a prompted `git fetch` /
    /// checkout reconstructs it.
    pub(crate) source_branch: String,
    /// The human-feedback delta delivered in this run's brief (oldest-first), each item
    /// carrying its author so the brief reads as a thread with speakers.
    pub(crate) feedback: Vec<FeedbackItem>,
    /// The ids of every note on the MR at claim time — the `seen` afkd's watch starts from,
    /// so the thread the brief was built from is never re-delivered as new.
    pub(crate) before_notes: Vec<u64>,
    /// The user this MR was claimed as: the unit's `self` (its username), and the identity
    /// the terminal lifecycle assigns and unassigns (its id).
    pub(crate) claimed_as: User,
}

impl Unit {
    /// The claim-journal key — the wire unit's `key`. It carries the claim marker, so it
    /// changes on every claim of the MR.
    pub(crate) fn key(&self) -> String {
        claim_key_for(&self.project, self.iid, self.claim_id)
    }

    /// The MR's stable coordinate, `<project>#<iid>` (ADR-0067): the wire unit's `thread`,
    /// the same across every claim, so an agent session resumes per MR across the review
    /// rounds.
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
}

/// The `gitlab_mr_review` kind's vendor half: the single-project target, the `author_me`
/// filter, the claim, and the three lifecycle action lists.
pub(crate) struct MrUnits {
    client: Box<dyn GitlabClient>,
    project: Project,
    /// Restrict to the bot's own MRs (the `author_me` flag).
    author_me: bool,
    on_claim: Vec<LifecycleAction>,
    on_done: Vec<LifecycleAction>,
    on_fail: Vec<LifecycleAction>,
    /// The token and base URL every unit's `env` carries.
    creds: BTreeMap<String, String>,
}

impl MrUnits {
    pub(crate) fn new(client: Box<dyn GitlabClient>, cfg: &GitlabConfig) -> Self {
        Self {
            client,
            project: Project::new(cfg.project.clone()),
            author_me: cfg.author_me,
            on_claim: cfg.on_claim.clone(),
            on_done: cfg.on_done.clone(),
            on_fail: cfg.on_fail.clone(),
            creds: creds_env(cfg),
        }
    }

    /// Claim the next MR with a note newer than the bot's last word — the claim race runs
    /// here, inside afkd's `poll`.
    ///
    /// The scan stops once it has run [`POLL_BUDGET`](crate::common::POLL_BUDGET), checked
    /// before each claim attempt, and answers "nothing this beat".
    pub(crate) fn try_claim_next(
        &self,
        me: &User,
        diag: &dyn Diag,
        clock: &dyn Clock,
    ) -> Result<Option<Unit>, GitlabError> {
        let budget = ScanBudget::start(clock, diag);
        for mr in self.client.list_open_mrs(&self.project)? {
            if self.author_me && mr.author.username != me.username {
                continue;
            }
            let notes = self
                .client
                .list_notes(&self.project, ItemKind::MergeRequest, mr.iid)?;
            if !mr_has_new_feedback(&notes, &me.username) {
                continue;
            }
            if budget.spent() {
                return Ok(None);
            }
            match claim_item(
                &*self.client,
                &self.project,
                ItemKind::MergeRequest,
                mr.iid,
                me,
                clock,
                diag,
            )? {
                Claimed::Won(claim_id) => {
                    // The claim is the marker; the label is the status it shows.
                    if let Err(e) = self.client.add_label(
                        &self.project,
                        ItemKind::MergeRequest,
                        mr.iid,
                        CLAIMED_LABEL,
                    ) {
                        diag.err(&e);
                    }
                    // `on_claim` runs before any fire, so neutral facts.
                    if let Err(e) = apply_actions(
                        &*self.client,
                        &self.project,
                        ItemKind::MergeRequest,
                        mr.iid,
                        &self.on_claim,
                        me,
                        &Facts::none(),
                    ) {
                        diag.err(&e);
                    }
                    return Ok(Some(Unit {
                        project: self.project.clone(),
                        iid: mr.iid,
                        claim_id,
                        source_branch: mr.source_branch,
                        feedback: mr_feedback_delta(&notes, &me.username),
                        before_notes: notes.iter().map(|n| n.id).collect(),
                        claimed_as: me.clone(),
                    }));
                }
                // Lost the race for this MR; try the next one.
                Claimed::Lost => continue,
            }
        }
        Ok(None)
    }

    /// The unit as it crosses the wire: the built-in's `unit_key` / `unit_thread` /
    /// `unit_env` ∪ `creds_env` / `scratch_layout`, with the claim identity's username as
    /// `self` and the claim-time note ids as `seen`. The brief is unframed — afkd frames
    /// `task.md` itself.
    pub(crate) fn wire_unit(&self, unit: &Unit) -> WireUnit {
        let mut env = self.creds.clone();
        env.insert(ENV_PROJECT.to_string(), unit.project.raw().to_string());
        env.insert(ENV_MR_NUMBER.to_string(), unit.iid.to_string());
        env.insert(ENV_MR_BRANCH.to_string(), unit.source_branch.clone());
        WireUnit {
            id: unit.iid.to_string(),
            key: unit.key(),
            thread: unit.thread(),
            seen: unit.before_notes.iter().map(u64::to_string).collect(),
            me: unit.claimed_as.username.clone(),
            env,
            files: vec![
                WireFile {
                    path: TASK_FILE.to_string(),
                    text: brief_text(unit.iid, &unit.feedback),
                },
                WireFile {
                    path: format!("{MR_DIR}/{NUMBER_FILE}"),
                    text: unit.iid.to_string(),
                },
            ],
        }
    }

    /// The terminal lifecycle: `on_done` (clean) or `on_fail` (exhausted). There is no
    /// `on_done close` here — a human's merge ends the loop by dropping the MR from the
    /// open set — and no `on_park`, so a park runs `on_fail`, as the built-in does.
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
            &unit.project,
            ItemKind::MergeRequest,
            unit.iid,
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
        // The claim is over however this round ended, so its marker goes — a finished
        // round must leave no marker to out-order the next claim. Best-effort: a leaked
        // marker ages out after `CLAIM_LIFETIME`.
        delete_marker(
            &*self.client,
            &unit.project,
            ItemKind::MergeRequest,
            unit.iid,
            unit.claim_id,
            diag,
        );
        delivered
    }
}

/// The `gitlab_mr_review` kind behind the plugin's seam.
impl Units for MrUnits {
    type Unit = Unit;

    /// It neither classifies an attempt nor marks a failed one — the built-in keeps the
    /// spine's defaults for both, so it never parks — so `classify` and `attempt_failed`
    /// are not among them.
    const CALLS: &'static [&'static str] = &["release", "renew", "comments"];

    fn resolve_me(&self) -> Result<User, GitlabError> {
        self.client.current_user()
    }

    fn try_claim_next(
        &self,
        me: &User,
        diag: &dyn Diag,
        clock: &dyn Clock,
    ) -> Result<Option<Unit>, GitlabError> {
        MrUnits::try_claim_next(self, me, diag, clock)
    }

    fn wire_unit(&self, unit: &Unit) -> WireUnit {
        MrUnits::wire_unit(self, unit)
    }

    fn release(&self, unit: &Unit, diag: &dyn Diag) {
        release_claim(
            &*self.client,
            &unit.project,
            ItemKind::MergeRequest,
            unit.iid,
            &unit.claimed_as,
            unit.claim_id,
            diag,
        );
    }

    /// Release one stale claim named by the whole journal key: remove the
    /// `afkd::claimed` status label, take back only the bot's own assignee row, and
    /// delete the crashed run's marker. GitLab is single-project, so the release targets
    /// this kind's own project and fixed kind.
    fn release_stale(&self, key: &str, me: &User, diag: &dyn Diag) -> Option<bool> {
        release_stale(
            &*self.client,
            &self.project,
            ItemKind::MergeRequest,
            key,
            me,
            diag,
        )
    }

    fn renew(&self, unit: &Unit, renewal: u64, diag: &dyn Diag) {
        renew_marker(
            &*self.client,
            &unit.project,
            ItemKind::MergeRequest,
            unit.iid,
            unit.claim_id,
            &unit.claimed_as.username,
            renewal,
            diag,
        );
    }

    fn comments(&self, unit: &Unit) -> Result<Vec<Note>, GitlabError> {
        self.client
            .list_notes(&unit.project, ItemKind::MergeRequest, unit.iid)
    }

    fn finish(&self, unit: &Unit, outcome: UnitOutcome, facts: &Facts, diag: &dyn Diag) -> bool {
        MrUnits::finish(self, unit, outcome, facts, diag)
    }
}

// --- Pure watermark/feedback helpers (no seam). ------------------------------

/// One piece of human feedback delivered to the brief: the commenter's name alongside
/// what they said. `author` is a display handle, not an identity key — nothing routes or
/// gates on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FeedbackItem {
    /// Who said it, as GitLab names them.
    pub(crate) author: String,
    /// What they said, verbatim.
    pub(crate) body: String,
}

/// The watermark: the newest `updated_at` among the bot's **own** notes (matched by
/// username). `None` when the bot has said nothing yet — every human note then counts as
/// new feedback (first round).
///
/// A claim marker ([`is_claim`]) is skipped whatever its author: ours is bookkeeping, not
/// a word to measure a reply against, and a *rival's* — authored by a different username —
/// must not read as feedback and re-fire the MR forever.
pub(crate) fn mr_watermark(notes: &[Note], me: &str) -> Option<SystemTime> {
    notes
        .iter()
        .filter(|n| n.author.username == me && !is_claim(&n.body))
        .map(|n| n.updated_at)
        .max()
}

/// The human-feedback delta to deliver this run: others' notes with a timestamp strictly
/// after the watermark (or all of them, first round), each carrying its author,
/// **oldest-first** so a review thread reads chronologically. Claim markers are dropped on
/// the same grounds as in [`mr_watermark`]: they are the lock's bookkeeping, never
/// conversation.
pub(crate) fn mr_feedback_delta(notes: &[Note], me: &str) -> Vec<FeedbackItem> {
    let watermark = mr_watermark(notes, me);
    let after = |t: SystemTime| watermark.is_none_or(|w| t > w);
    let mut items: Vec<(SystemTime, FeedbackItem)> = Vec::new();
    for n in notes {
        if n.author.username != me && !is_claim(&n.body) && after(n.updated_at) {
            items.push((
                n.updated_at,
                FeedbackItem {
                    author: n.author.username.clone(),
                    body: n.body.clone(),
                },
            ));
        }
    }
    items.sort_by_key(|(t, _)| *t);
    items.into_iter().map(|(_, item)| item).collect()
}

/// Whether an MR carries human feedback newer than the bot's last word.
pub(crate) fn mr_has_new_feedback(notes: &[Note], me: &str) -> bool {
    !mr_feedback_delta(notes, me).is_empty()
}

/// The brief written to `task.md`: a header naming the MR, then the feedback delta
/// (oldest-first, each line attributed to its author). Empty feedback still writes the
/// header.
fn brief_text(iid: u64, feedback: &[FeedbackItem]) -> String {
    let mut s = format!("Address review feedback on MR !{iid}.");
    s.push_str(&render_feedback_section("New feedback", feedback));
    s
}

/// Render the brief's feedback section: a `## {heading}` rule, then one
/// `**{author}:** {body}` paragraph per item, oldest-first as `items` is ordered. An empty
/// slice renders `""`. Byte for byte afkd's `afkd_forge::feedback::render_feedback_section`,
/// so a brief reads the same whichever of the two wrote it.
fn render_feedback_section(heading: &str, items: &[FeedbackItem]) -> String {
    if items.is_empty() {
        return String::new();
    }
    let mut s = format!("\n\n## {heading}\n");
    for item in items {
        s.push('\n');
        s.push_str(&format!("**{}:** {}", item.author, item.body.trim_end()));
        s.push('\n');
    }
    s
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
    use crate::claim::{claim_renewal_text, claim_text, split_claim_key, CLAIM_MARKER};
    use crate::client::{Action, MockClient};
    use crate::common::{CaptureDiag, FakeClock};
    use std::sync::Arc;
    use std::time::{Duration, UNIX_EPOCH};

    fn cfg() -> GitlabConfig {
        GitlabConfig {
            base_url: "https://gitlab.example.com".into(),
            project: "group/widgets".into(),
            token: "PAT".into(),
            author_me: true,
            on_claim: vec![
                LifecycleAction::AssignMe,
                LifecycleAction::LabelAdd("afkd::reviewing".into()),
            ],
            on_fail: vec![LifecycleAction::Unassign],
            ..GitlabConfig::default()
        }
    }

    /// The kind over a shared [`MockClient`], with a capturing diagnostic sink and a fake
    /// clock — the spine's side of each call, played by hand: [`poll`](Self::poll) is the
    /// `poll` call, [`finish`](Self::finish) the `finish` call.
    struct Harness {
        client: Arc<MockClient>,
        units: MrUnits,
        diag: CaptureDiag,
        clock: FakeClock,
    }

    impl Harness {
        fn new(cfg: GitlabConfig) -> Self {
            let client = Arc::new(MockClient::new(1, "me"));
            let units = MrUnits::new(Box::new(Arc::clone(&client)), &cfg);
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

        /// The `finish` call for `unit`, ended `outcome` with `facts`.
        fn finish(&self, unit: &Unit, outcome: UnitOutcome, facts: &Facts) -> bool {
            self.units.finish(unit, outcome, facts, &self.diag)
        }

        /// Seed the bot's open MR !`iid` on `feature/x` with one human note — an MR with
        /// new feedback, the shape most tests claim.
        fn mr_with_feedback(&self, iid: u64) {
            self.client.add_mr(iid, 1, "me", "feature/x");
            self.client
                .add_note(ItemKind::MergeRequest, iid, 1, 99, "human", 100);
        }
    }

    fn me() -> User {
        User {
            id: 1,
            username: "me".into(),
        }
    }

    /// The delta's bodies alone.
    fn bodies(delta: &[FeedbackItem]) -> Vec<String> {
        delta.iter().map(|i| i.body.clone()).collect()
    }

    fn authors(delta: &[FeedbackItem]) -> Vec<String> {
        delta.iter().map(|i| i.author.clone()).collect()
    }

    /// An unedited note: written and last touched at the same instant.
    fn note(id: u64, author: &str, secs: u64) -> Note {
        edited_note(id, author, secs, secs)
    }

    /// A note written at `created` and last edited at `updated` — the shape that tells the
    /// watermark's two times apart.
    fn edited_note(id: u64, author: &str, created: u64, updated: u64) -> Note {
        body_note(id, author, &format!("note {id}"), created, updated)
    }

    /// A note with an explicit body — the shape a claim marker and a real, multi-line
    /// review comment both need.
    fn body_note(id: u64, author: &str, body: &str, created: u64, updated: u64) -> Note {
        Note {
            id,
            body: body.into(),
            author: User {
                id: if author == "me" { 1 } else { 99 },
                username: author.into(),
            },
            created_at: UNIX_EPOCH + Duration::from_secs(created),
            updated_at: UNIX_EPOCH + Duration::from_secs(updated),
        }
    }

    /// The second a claim race is staged in, frozen on the mock's post clock so a rival
    /// marker can be placed a known distance either side of our own.
    const T: u64 = 1_700_000_000;

    /// The `[afkd-claim]` marker bodies on MR `iid`, read through the trait's own list
    /// call — the same thing the claim reads.
    fn claim_markers_on(h: &Harness, iid: u64) -> Vec<String> {
        h.client
            .list_notes(&Project::new("group/widgets"), ItemKind::MergeRequest, iid)
            .expect("read the thread")
            .into_iter()
            .filter(|n| is_claim(&n.body))
            .map(|n| n.body)
            .collect()
    }

    /// Every recorded mutation is on the MR path: the kind never writes to an issue.
    fn assert_all_on_the_mr_path(h: &Harness) {
        let kinds: Vec<ItemKind> = h
            .client
            .actions()
            .into_iter()
            .map(|a| match a {
                Action::Assign { kind, .. }
                | Action::Label { kind, .. }
                | Action::Unlabel { kind, .. }
                | Action::State { kind, .. }
                | Action::Comment { kind, .. }
                | Action::DeleteComment { kind, .. }
                | Action::EditComment { kind, .. } => kind,
            })
            .collect();
        assert!(
            kinds.iter().all(|k| *k == ItemKind::MergeRequest),
            "{:?}",
            h.client.actions()
        );
    }

    // --- Pure helpers ---

    #[test]
    fn watermark_is_the_newest_of_the_bots_own_notes() {
        let notes = [
            note(1, "human", 100),
            note(2, "me", 200),
            note(3, "me", 150),
        ];
        assert_eq!(
            mr_watermark(&notes, "me"),
            Some(UNIX_EPOCH + Duration::from_secs(200))
        );
        // With nothing said by the bot, there is no watermark.
        assert_eq!(mr_watermark(&[note(1, "human", 100)], "me"), None);
    }

    #[test]
    fn new_feedback_only_after_the_bots_last_word() {
        // bot@200; a human note at 300 is new, one at 150 is already addressed.
        let notes = [
            note(1, "human", 150),
            note(2, "me", 200),
            note(3, "human", 300),
        ];
        assert!(mr_has_new_feedback(&notes, "me"));
        let delta = mr_feedback_delta(&notes, "me");
        assert_eq!(bodies(&delta), vec!["note 3".to_string()]);

        // No human note newer than the bot's last word ⇒ idle.
        let addressed = [note(1, "human", 150), note(2, "me", 200)];
        assert!(!mr_has_new_feedback(&addressed, "me"));
    }

    #[test]
    fn feedback_delta_is_oldest_first_and_excludes_the_bots_own() {
        let notes = [
            note(2, "human", 300),
            note(1, "human", 100),
            note(9, "me", 50),
        ];
        let delta = mr_feedback_delta(&notes, "me");
        // Oldest-first; the bot's own note is dropped.
        assert_eq!(
            bodies(&delta),
            vec!["note 1".to_string(), "note 2".to_string()]
        );
    }

    #[test]
    fn first_round_keeps_all_human_feedback() {
        // No prior bot note: every human note is new.
        let notes = [note(1, "human", 100), note(2, "human", 200)];
        let delta = mr_feedback_delta(&notes, "me");
        assert_eq!(
            bodies(&delta),
            vec!["note 1".to_string(), "note 2".to_string()]
        );
    }

    /// Two distinct humans in one thread, out of order, with the bot interleaved: the
    /// delta keeps each speaker's name attached to their own body, stays oldest-first,
    /// and still excludes the bot's own word.
    #[test]
    fn the_feedback_delta_carries_each_notes_author() {
        let notes = [
            note(2, "bob", 300),
            note(9, "me", 50),
            note(1, "alice", 100),
        ];
        let delta = mr_feedback_delta(&notes, "me");
        assert_eq!(authors(&delta), vec!["alice", "bob"]);
        assert_eq!(
            bodies(&delta),
            vec!["note 1", "note 2"],
            "each author keeps their own body"
        );
        assert!(!authors(&delta).contains(&"me".to_string()));
    }

    /// The watermark and delta read `updated_at`: the question is "has this been touched
    /// since the bot last spoke", and an edit *is* something new to answer. The fixture
    /// flips under the other key — the bot wrote first but edited last, one human wrote
    /// before the bot's word and edited after it, one wrote after the bot's word but has
    /// not touched it since, and the two survivors sort in opposite orders.
    #[test]
    fn the_note_watermark_reads_updated_at_not_created_at() {
        let notes = [
            edited_note(1, "me", 100, 500),
            edited_note(2, "josefandersson", 200, 600),
            edited_note(3, "björn-öst", 300, 400),
            edited_note(4, "陳大文", 550, 560),
        ];

        // The watermark is the bot's *edit* (500), not when it wrote (100).
        assert_eq!(
            mr_watermark(&notes, "me"),
            Some(UNIX_EPOCH + Duration::from_secs(500))
        );
        assert!(mr_has_new_feedback(&notes, "me"));
        // Only the two touched since t=500, oldest *edit* first. Under `created_at` this
        // would be all three humans, ordered 2, 3, 4.
        assert_eq!(
            authors(&mr_feedback_delta(&notes, "me")),
            vec!["陳大文", "josefandersson"]
        );

        // And the note written after the bot but not touched since is not new feedback on
        // its own — under `created_at` (300 > 100) it would be.
        let stale = [notes[0].clone(), notes[2].clone()];
        assert!(!mr_has_new_feedback(&stale, "me"));
    }

    /// A claim marker is never read as conversation, asserted as **parity**: the same
    /// thread read with and without a pair of markers interleaved must give the *same*
    /// watermark, the *same* delta and the *same* `has_new` — one fixture through the
    /// shared helpers, not two hand-written expectations that could drift. The markers sit
    /// exactly where they would do damage if counted: **ours** newest after the bot's last
    /// word (it would push the watermark past the human's reply and leave the MR idle
    /// forever) and a **rival's** newest of all, authored by a different username (it
    /// would be delivered to the agent as fresh feedback and re-fire the MR every poll).
    ///
    /// The thread underneath is the adversarial one: an edited note, multi-line prose with
    /// a code block, non-ASCII and wide handles, a bot reply.
    #[test]
    fn a_claim_marker_is_not_feedback() {
        let conversation = [
            edited_note(1, "me", 100, 500),
            body_note(2, "bob-döner", "still broken\n\n    retry(1);\n", 510, 600),
            body_note(3, "陳大文", "看起来不对 🚨 — §2 的说法是错的", 700, 700),
        ];

        let mut with_markers = conversation.to_vec();
        // Ours, newest of the bot's own words…
        with_markers.push(body_note(4, "me", &claim_text("me"), 900, 900));
        // …and a rival's, newest of all.
        with_markers.push(body_note(
            5,
            "björn-öst[bot]",
            &claim_text("björn-öst[bot]"),
            1000,
            1000,
        ));

        assert_eq!(
            mr_watermark(&with_markers, "me"),
            mr_watermark(&conversation, "me"),
            "a marker is not the bot's last word"
        );
        assert_eq!(
            mr_feedback_delta(&with_markers, "me"),
            mr_feedback_delta(&conversation, "me"),
            "a marker is neither delivered nor a boundary"
        );
        assert_eq!(
            mr_has_new_feedback(&with_markers, "me"),
            mr_has_new_feedback(&conversation, "me")
        );
        // Not vacuous: the marker-free read really does carry the thread, and no marker
        // text reaches the delta.
        assert_eq!(
            authors(&mr_feedback_delta(&with_markers, "me")),
            vec!["bob-döner", "陳大文"]
        );
        assert!(!bodies(&mr_feedback_delta(&with_markers, "me"))
            .iter()
            .any(|b| b.contains(CLAIM_MARKER)));
    }

    // --- The brief + the wire unit ---

    /// The brief a two-human thread produces: each line is attributed by name, over a
    /// multi-line body with trailing whitespace and non-ASCII text — the shapes a real
    /// review thread carries. Unframed: afkd frames `task.md` itself.
    #[test]
    fn the_brief_attributes_each_note_to_its_author() {
        let h = Harness::new(cfg());
        let unit = Unit {
            project: Project::new("group/widgets"),
            iid: 7,
            claim_id: 1,
            source_branch: "feature/x".into(),
            feedback: vec![
                FeedbackItem {
                    author: "alice".into(),
                    body: "this breaks the retry path when the token expires\n\n    let x = 1;\n  "
                        .into(),
                },
                FeedbackItem {
                    author: "bob-döner".into(),
                    body: "disagree — the retry path already handles that, see !412".into(),
                },
            ],
            before_notes: Vec::new(),
            claimed_as: me(),
        };

        assert_eq!(
            h.units.wire_unit(&unit).files[0].text,
            "Address review feedback on MR !7.\n\n## New feedback\n\
             \n**alice:** this breaks the retry path when the token expires\n\n    let x = 1;\n\
             \n**bob-döner:** disagree — the retry path already handles that, see !412\n"
        );

        // Empty feedback (claimed mid-race) still writes the header, and no section.
        let bare = Unit {
            feedback: Vec::new(),
            ..unit
        };
        assert_eq!(
            h.units.wire_unit(&bare).files[0].text,
            "Address review feedback on MR !7."
        );
    }

    /// The unit a real claim hands over: the built-in's journal key (which round-trips to
    /// the coordinate and the marker), the per-MR thread, the claim-time note ids as
    /// `seen`, the claim identity as `self`, exactly the five env names the skill reads —
    /// the branch verbatim, however non-ASCII — and the scratch layout.
    #[test]
    fn the_wire_unit_carries_the_built_ins_key_thread_env_and_layout() {
        let h = Harness::new(cfg());
        h.client.add_mr(7, 1, "me", "feature/重试-backoff");
        h.client.add_note_body(
            ItemKind::MergeRequest,
            7,
            41,
            1,
            "me",
            "Pushed a fix for the backoff.",
            100,
        );
        h.client.add_note_body(
            ItemKind::MergeRequest,
            7,
            42,
            99,
            "陳大文",
            "看起来不对 🚨\n\nthe cap is still 0",
            200,
        );
        let unit = h.poll().expect("claimed");
        let wire = h.units.wire_unit(&unit);

        assert_eq!(wire.id, "7");
        assert_eq!(
            split_claim_key(&wire.key),
            Some(("group/widgets", 7, unit.claim_id))
        );
        assert_eq!(wire.thread, "group/widgets#7");
        assert_eq!(
            wire.seen,
            ["41", "42"],
            "the claim-time thread, not the marker"
        );
        assert_eq!(wire.me, "me");
        assert_eq!(
            wire.env,
            BTreeMap::from([
                (
                    "GITLAB_BASE_URL".to_string(),
                    "https://gitlab.example.com".to_string()
                ),
                (
                    "GITLAB_MR_BRANCH".to_string(),
                    "feature/重试-backoff".to_string()
                ),
                ("GITLAB_MR_NUMBER".to_string(), "7".to_string()),
                ("GITLAB_PROJECT".to_string(), "group/widgets".to_string()),
                ("GITLAB_TOKEN".to_string(), "PAT".to_string()),
            ])
        );
        assert_eq!(
            wire.files,
            [
                WireFile {
                    path: "task.md".into(),
                    text: "Address review feedback on MR !7.\n\n## New feedback\n\n\
                           **陳大文:** 看起来不对 🚨\n\nthe cap is still 0\n"
                        .into(),
                },
                WireFile {
                    path: "mr/number".into(),
                    text: "7".into(),
                },
            ]
        );
    }

    // --- Eligibility + the claim ---

    #[test]
    fn polls_only_author_me_open_mrs_and_claims_one_with_new_feedback() {
        let h = Harness::new(cfg());
        // Someone else's MR, listed first → filtered out by `author_me`.
        h.client.add_mr(8, 2, "other", "feature/y");
        h.client
            .add_note(ItemKind::MergeRequest, 8, 2, 99, "human", 100);
        // Mine, with new human feedback → eligible.
        h.mr_with_feedback(7);

        let unit = h.poll().expect("claimed");
        assert_eq!(unit.iid, 7);
        assert_eq!(unit.source_branch, "feature/x");
        assert_eq!(bodies(&unit.feedback), ["note 1"]);
        // The kind's status label, then `on_claim` — all on the MR path.
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
                    kind: ItemKind::MergeRequest,
                    iid: 7,
                    name: "afkd::claimed".into()
                },
                Action::Assign {
                    kind: ItemKind::MergeRequest,
                    iid: 7,
                    ids: vec![1]
                },
                Action::Label {
                    kind: ItemKind::MergeRequest,
                    iid: 7,
                    name: "afkd::reviewing".into()
                },
            ]
        );
        assert!(
            claim_markers_on(&h, 8).is_empty(),
            "the foreign MR is untouched"
        );
    }

    /// The flag's other half: without `author_me` a foreign MR with feedback is claimed.
    #[test]
    fn without_author_me_a_foreign_mr_is_claimed() {
        let h = Harness::new(GitlabConfig {
            author_me: false,
            ..cfg()
        });
        h.client.add_mr(8, 2, "other", "feature/y");
        h.client
            .add_note(ItemKind::MergeRequest, 8, 2, 99, "human", 100);

        let unit = h.poll().expect("a foreign MR is claimed without author_me");
        assert_eq!((unit.iid, unit.source_branch.as_str()), (8, "feature/y"));
    }

    #[test]
    fn a_not_me_mr_is_skipped_without_claiming() {
        // Only a foreign-authored MR is open: the `author_me` filter skips it, so the poll
        // walks the whole list and claims nothing (no marker, assign or label posted).
        let h = Harness::new(cfg());
        h.client.add_mr(8, 2, "other", "feature/y");
        h.client
            .add_note(ItemKind::MergeRequest, 8, 2, 99, "human", 100);
        assert!(h.poll().is_none());
        assert!(
            h.client.actions().is_empty(),
            "a filtered-out MR is never claimed"
        );
    }

    #[test]
    fn an_mr_with_no_new_feedback_is_idle() {
        let h = Harness::new(cfg());
        h.client.add_mr(7, 1, "me", "feature/x");
        // The bot already replied last (t=200) and nothing newer arrived.
        h.client
            .add_note(ItemKind::MergeRequest, 7, 1, 99, "human", 100);
        h.client
            .add_note(ItemKind::MergeRequest, 7, 2, 1, "me", 200);
        assert!(h.poll().is_none());
        assert!(h.client.actions().is_empty(), "{:?}", h.client.actions());
    }

    /// A merged or closed MR leaves the `state=opened` set, so even with fresh feedback on
    /// it nothing is claimed — there is no special "until closed" case.
    #[test]
    fn a_merged_or_closed_mr_drops_out_of_the_open_set() {
        let h = Harness::new(cfg());
        h.mr_with_feedback(7);
        h.client.close_mr(7);
        assert!(h.poll().is_none());
        assert!(h.client.actions().is_empty(), "{:?}", h.client.actions());
    }

    /// An MR whose only note newer than the bot's last word is a **rival's** claim marker
    /// stays idle: a marker is bookkeeping, not feedback, so it must not re-fire a review
    /// round on every poll.
    #[test]
    fn an_mr_whose_only_new_note_is_a_claim_marker_is_idle() {
        let h = Harness::new(cfg());
        h.client.add_mr(7, 1, "me", "feature/x");
        h.client
            .add_note(ItemKind::MergeRequest, 7, 1, 99, "human", 100);
        h.client
            .add_note(ItemKind::MergeRequest, 7, 2, 1, "me", 200);
        h.client.add_note_body(
            ItemKind::MergeRequest,
            7,
            3,
            99,
            "björn-öst[bot]",
            &claim_text("björn-öst[bot]"),
            300,
        );

        assert!(
            h.poll().is_none(),
            "the marker is not an answer to the bot's last word"
        );
        assert!(
            h.client.actions().is_empty(),
            "an idle MR is never claimed: {:?}",
            h.client.actions()
        );
    }

    #[test]
    fn a_lost_claim_releases_the_marker_and_claims_no_unit() {
        // A rival's marker lands in the thread while we read it, one second ahead of ours
        // in the order and well inside the claim lifetime, so the claim loses the race:
        // no unit, our marker taken back, and the status label never applied.
        let h = Harness::new(cfg());
        h.mr_with_feedback(7);
        h.client.set_clock(T);
        h.client.rival_claims_next(99, "rival", 42, T - 1);

        assert!(h.poll().is_none());
        assert_eq!(
            claim_markers_on(&h, 7),
            vec![claim_text("rival")],
            "only the winner's marker is left"
        );
        assert!(
            !h.client
                .has_label(ItemKind::MergeRequest, 7, "afkd::claimed"),
            "a lost claim shows no status"
        );
        assert!(h.client.assignee_ids(ItemKind::MergeRequest, 7).is_empty());
        assert_all_on_the_mr_path(&h);
    }

    /// The `Lost` arm continues the candidate loop rather than ending the poll: the rival
    /// takes the first MR, so the *second* eligible one is claimed — and the first is left
    /// with neither our marker nor the claimed label.
    #[test]
    fn a_lost_claim_moves_on_to_the_next_mr() {
        let h = Harness::new(cfg());
        h.mr_with_feedback(7);
        h.mr_with_feedback(8);
        // Armed for the next note read, which is MR !7's eligibility read; the marker
        // stays in its thread for the claim's re-read.
        h.client.set_clock(T);
        h.client.rival_claims_next(99, "rival", 42, T - 1);

        let unit = h.poll().expect("the second MR is claimed");
        assert_eq!(unit.iid, 8);
        assert_eq!(claim_markers_on(&h, 7), vec![claim_text("rival")]);
        assert!(!h
            .client
            .has_label(ItemKind::MergeRequest, 7, "afkd::claimed"));
        assert!(h
            .client
            .has_label(ItemKind::MergeRequest, 8, "afkd::claimed"));
    }

    #[test]
    fn an_on_claim_failure_is_logged_but_the_claim_proceeds() {
        // `on_claim` runs after a won claim; a failure there is logged and swallowed, and
        // the unit is still taken on. A `close` the claim itself does not perform is
        // failed here.
        let h = Harness::new(GitlabConfig {
            on_claim: vec![LifecycleAction::Close],
            ..cfg()
        });
        h.mr_with_feedback(7);
        h.client.fail("set state");

        let unit = h.poll().expect("claimed anyway");
        assert_eq!(unit.iid, 7);
        assert_eq!(
            h.diag.lines(),
            ["gitlab set state: no response (mock failure)"]
        );
    }

    /// A forge failure *during the claim itself* (the marker post errors) is not a lost
    /// race but a transport fault: it propagates out of the poll, which the plugin turns
    /// into an idle beat.
    #[test]
    fn a_claim_forge_error_propagates_out_of_the_poll() {
        let h = Harness::new(cfg());
        h.mr_with_feedback(7);
        h.client.fail("post comment");

        let err = h
            .units
            .try_claim_next(&me(), &h.diag, &h.clock)
            .expect_err("the claim's post failed");
        assert_eq!(err.stage(), "post comment");
        assert!(claim_markers_on(&h, 7).is_empty());
    }

    /// A forge error on the listing is the beat's error — the plugin's idle beat — and the
    /// next beat, with the forge back, claims.
    #[test]
    fn a_forge_error_during_poll_is_returned() {
        let h = Harness::new(cfg());
        h.mr_with_feedback(7);
        h.client.fail("list merge requests");

        let err = h
            .units
            .try_claim_next(&me(), &h.diag, &h.clock)
            .expect_err("the listing failed");
        assert_eq!(err.stage(), "list merge requests");
        h.client.clear_failure();
        assert!(h.poll().is_some());
    }

    /// A scan that runs past [`POLL_BUDGET`](crate::common::POLL_BUDGET) stops claiming
    /// and says so, well inside afkd's 60-second call deadline. Every candidate has fresh
    /// feedback and loses its race to a live rival, and each attempt settles one second,
    /// so the twenty-first candidate is the first the budget turns away — and nothing
    /// after it is posted to.
    #[test]
    fn the_poll_budget_ends_the_scan_and_says_so() {
        let h = Harness::new(cfg());
        h.client.set_clock(T);
        for n in 1..=25 {
            h.client.add_mr(n, 1, "me", "feature/重试-backoff");
            h.client.add_note_body(
                ItemKind::MergeRequest,
                n,
                1,
                99,
                "陳大文",
                "看起来不对 🚨",
                100,
            );
            let rival = claim_text("björn-öst[bot]");
            h.client.add_note_body(
                ItemKind::MergeRequest,
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

    // --- Lifecycle on terminal outcomes ---

    /// A finished round leaves no marker: the release runs whatever the terminal moment
    /// did, so the next round's claim is not out-ordered by a marker nobody is acting on
    /// (and the MR's own watermark never meets one). All three dispositions are driven.
    #[test]
    fn a_finished_round_leaves_no_claim_marker() {
        for outcome in [UnitOutcome::Clean, UnitOutcome::Failed, UnitOutcome::Park] {
            let h = Harness::new(cfg());
            h.mr_with_feedback(7);

            let unit = h.poll().expect("claimed");
            assert_eq!(claim_markers_on(&h, 7).len(), 1, "the claim is held");

            assert!(h.finish(&unit, outcome, &Facts::none()));
            assert!(
                claim_markers_on(&h, 7).is_empty(),
                "a round ending {outcome:?} released its marker: {:?}",
                claim_markers_on(&h, 7)
            );
            assert_all_on_the_mr_path(&h);
        }
    }

    /// The journal key carries the claim, the session thread does not: two successive
    /// claims of the same MR give two different keys (each naming its own marker) and one
    /// identical thread, so an agent session resumes per MR across review rounds.
    #[test]
    fn the_journal_key_carries_the_claim_but_the_thread_does_not() {
        let h = Harness::new(cfg());
        h.mr_with_feedback(7);

        let first = h.poll().expect("claimed");
        // Wind the round down as a finished run does (marker released), then let the human
        // speak again so the MR is eligible for a second round.
        h.finish(&first, UnitOutcome::Clean, &Facts::none());
        h.client
            .add_note(ItemKind::MergeRequest, 7, 2, 99, "human", 300);
        let second = h.poll().expect("re-claimed");

        let (key_a, key_b) = (first.key(), second.key());
        assert_ne!(key_a, key_b, "a fresh claim is a fresh journal key");
        assert_eq!(
            first.thread(),
            second.thread(),
            "the session thread is the MR, not the claim"
        );
        assert_eq!(first.thread(), "group/widgets#7");
        // Each key round-trips to the coordinate + the marker the reaper deletes.
        for (key, unit) in [(&key_a, &first), (&key_b, &second)] {
            assert_eq!(
                split_claim_key(key),
                Some(("group/widgets", 7, unit.claim_id))
            );
        }
    }

    /// The MR-review service configures no `on_done` — a human's merge ends the loop — so
    /// a clean round records no state change (no `close`), and the MR stays open.
    #[test]
    fn a_clean_run_emits_no_on_done_close() {
        let h = Harness::new(cfg());
        h.mr_with_feedback(7);
        let unit = h.poll().expect("claimed");

        assert!(h.finish(&unit, UnitOutcome::Clean, &Facts::none()));

        assert!(!h
            .client
            .actions()
            .iter()
            .any(|a| matches!(a, Action::State { .. })));
        assert_eq!(
            h.client
                .list_open_mrs(&Project::new("group/widgets"))
                .unwrap()
                .len(),
            1
        );
    }

    /// GitLab has no `on_park`: a parked attempt runs `on_fail`, with the run's facts.
    #[test]
    fn a_park_runs_on_fail_with_the_run_facts() {
        let h = Harness::new(GitlabConfig {
            on_done: vec![LifecycleAction::LabelAdd("afkd::reviewed".into())],
            on_fail: vec![LifecycleAction::Comment(
                "Stopped after @{run:duration}: over to a human.".into(),
            )],
            ..cfg()
        });
        h.mr_with_feedback(7);
        let unit = h.poll().expect("claimed");
        let facts = Facts {
            duration_ms: 168_000,
            ..Facts::none()
        };

        assert!(h.finish(&unit, UnitOutcome::Park, &facts));

        assert!(h.client.actions().contains(&Action::Comment {
            kind: ItemKind::MergeRequest,
            iid: 7,
            body: "Stopped after 2m48s: over to a human.".into(),
        }));
        assert!(
            !h.client
                .has_label(ItemKind::MergeRequest, 7, "afkd::reviewed"),
            "`on_done` did not run"
        );
    }

    /// A terminal moment that did not land answers `false` — the plugin's `held` — and is
    /// diagnosed by its stage; the marker still goes. `unassign` reads the set before
    /// replacing it, so the read is where a transport fault strikes first.
    #[test]
    fn a_terminal_lifecycle_failure_returns_false_and_is_logged() {
        let h = Harness::new(cfg());
        h.mr_with_feedback(7);
        let unit = h.poll().expect("claimed");
        h.client.fail("get item");

        assert!(!h.finish(&unit, UnitOutcome::Failed, &Facts::none()));

        assert_eq!(
            h.diag.lines(),
            ["gitlab get item: no response (mock failure)"]
        );
        assert!(claim_markers_on(&h, 7).is_empty());
        assert!(h
            .client
            .has_label(ItemKind::MergeRequest, 7, "afkd::claimed"));
    }

    /// The renewal edits the claim marker **in place** — same id, a body that still reads
    /// as a claim — on the MR's own thread, and the forge's last-touched stamp moves with
    /// it.
    #[test]
    fn renewing_a_claim_edits_the_marker_in_place() {
        let h = Harness::new(cfg());
        h.client.add_mr(7, 1, "me", "feature/x");
        h.client.add_note_body(
            ItemKind::MergeRequest,
            7,
            9,
            1,
            "me",
            &claim_text("me"),
            100,
        );
        let unit = Unit {
            project: Project::new("group/widgets"),
            iid: 7,
            claim_id: 9,
            source_branch: "feature/x".into(),
            feedback: Vec::new(),
            before_notes: Vec::new(),
            claimed_as: me(),
        };

        h.units.renew(&unit, 3, &h.diag);

        let renewed = claim_renewal_text("me", 3);
        assert_eq!(
            h.client.actions(),
            vec![Action::EditComment {
                kind: ItemKind::MergeRequest,
                iid: 7,
                id: 9,
                body: renewed.clone(),
            }],
            "the marker was not edited by its own id, on its own item"
        );
        let thread = h
            .client
            .list_notes(&Project::new("group/widgets"), ItemKind::MergeRequest, 7)
            .expect("read the thread");
        assert_eq!(thread.len(), 1, "a renewal minted a second note");
        assert_eq!(thread[0].id, 9, "the note id moved");
        assert_eq!(thread[0].body, renewed);
        assert!(thread[0].updated_at > thread[0].created_at);
    }
}
