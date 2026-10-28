//! The `issue` kind's **vendor half**: the Linear side of afkd's unit spine, `@afkd/trello`'s
//! card kind re-seated on a team's issues.
//!
//! afkd keeps the spine — the cadence, the queue lane, the claim journal, the attempt loop,
//! the framing of `task.md`, the mid-run watch. What lives here is what only the tracker can
//! answer: the comment-lock claim, the eligibility gates, the blocked-by rule, the
//! parked-issue sweep and its ownership rule, the brief, the attempt markers, the park badge
//! and owner marker at the end of a run, and the owed-finish retry a `held` finish leaves
//! for `release`. It reaches Linear through the mockable [`Tracker`] seam, so that logic is
//! unit-tested with no network against the in-memory `MockTracker`.
//!
//! Work is only ever *claimed from a `pick_from` state*, or re-claimed by the parked sweep
//! from wherever a badged issue sits. A state's issues are read in the team's manual order,
//! which plays the part of a Trello list's position. A parked issue is the sweep's alone,
//! wherever it sits: one whose slots did not move it on is still in the state it was claimed
//! from, and the state scan passes it over.
//!
//! The actions a slot calls on an issue ([`crate::action`]) come through here too: a write
//! is [`deliver`](LinearUnits::deliver)ed like the kind's own, and a read is done at once.
//!
//! Every marker text that crosses the tracker is trello's, byte for byte, with one
//! difference: `[afkd-ran]`'s `upto=` is in epoch **milliseconds**, since Linear stamps a
//! comment to the millisecond and a seconds watermark would deliver a comment posted at
//! `x.500` a second time. Every marker is read through [`unescape`], in case Linear hands
//! one back escaped.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::action::{self, action_line, IssueAction, Read};
use crate::claim::{
    claim_renewal_text, claim_text, is_claim, unescape, won_claim, ClaimMarker, CLAIM_LIFETIME,
    CLAIM_SETTLE,
};
use crate::client::GRAPHQL_PATH;
use crate::common::{
    lock, Clock, Diag, ScanBudget, ENV_API_URL, ENV_AUTHORIZATION, ENV_ISSUE, ENV_ISSUE_ID,
    ENV_TEAM_ID,
};
use crate::file;
use crate::outbox::{lost_line, named, queued_behind_line, queued_line, Outbox, Write, RETRY_FOR};
use crate::settings::{DiscussWith, Holder, IssueConfig};
use crate::tracker::{state_named, Child, Comment, Issue, LinearError, State, Team, Tracker};
use crate::wire::{Facts, IssueFields, UnitOutcome, WireFile, WireUnit};
use crate::{PARK_FILE, TASK_FILE};

/// Marker prefix on a failed-attempt comment.
const ATTEMPT_MARKER: &str = "[afkd-attempt]";

/// The bound on remembered undelivered finishes. Empty in the steady state — every
/// `release` drains its entry — so this only bites where afkd never asks.
const PENDING_MAX: usize = 64;

/// The most UTF-16 code units an attempt note's text runs to: trello's comment limit, kept
/// as a bound on how large a note a fault's sentence can make, since Linear documents none.
const COMMENT_MAX: usize = 16_384;

/// What an attempt note whose reason was cut to [`COMMENT_MAX`] ends with.
const CUT_MARK: &str = "… (cut; see run.log)";

/// Marker prefix on the durable high-water-mark comment posted at run end.
const RAN_MARKER: &str = "[afkd-ran]";

/// Marker prefix on the **park owner** comment: posted when a run parks an issue, naming the
/// configured service that parked it, so the parked sweep hands the issue back to that
/// service alone. Present only while the issue is parked — the re-claim deletes it.
const PARK_MARKER: &str = "[afkd-park]";

/// The label the kind adds when a run **parks** an issue awaiting a human reply, and takes
/// off again on the next claim. It also does the finding: the parked sweep asks Linear for
/// the issues carrying it.
const NEEDS_INFO_LABEL: &str = "Needs Info";

/// How many badged issues one beat considers, from a rotating cursor. A bound, not a limit
/// on how many issues may be parked: the cursor still reaches every one within a few beats.
const PARK_SCAN_MAX: usize = 16;

/// The categories of a workflow state an issue is done in: a blocker in one of them holds
/// nothing back. A canceled or duplicate blocker never completes, so waiting on it would
/// hold the issue forever.
const CLOSED_KINDS: &[&str] = &["completed", "canceled", "duplicate"];

/// The tag every success line wears, naming the vendor.
pub(crate) const ACTION_TAG: &str = "[linear]";

/// One issue taken on as a unit of work.
pub(crate) struct Unit {
    issue: Issue,
    /// Our claim comment id, to delete on lease release.
    claim_id: String,
    /// The full claim-read comment set (the read used to judge the claim). The brief renders
    /// the whole thread from this; the new/prior split and the delivery boundary are derived
    /// from it.
    comments: Vec<Comment>,
    /// The delivery boundary stamped onto this run's `[afkd-ran]` watermark: the newest
    /// comment time this run has accounted for. Fixed at the claim read, so a comment that
    /// arrives mid-run falls strictly after it and reaches the next run.
    delivered_upto: SystemTime,
    /// The ids afkd's own comments come back authored by ([`SelfIds`]).
    me: SelfIds,
    /// The `pick_from` state the issue was claimed from, when it is not the first one
    /// named; `None` for the first, and for an issue the sweep found outside every one.
    source: Option<State>,
    /// The `Authorization` the run reaches Linear with, as of the claim.
    authorization: String,
}

impl Unit {
    /// The issue's identifier — the handle a log line names it by.
    pub(crate) fn identifier(&self) -> &str {
        &self.issue.identifier
    }

    /// The issue, as the claiming read saw it.
    pub(crate) fn issue(&self) -> &Issue {
        &self.issue
    }

    /// The ids afkd's own comments carry.
    pub(crate) fn me(&self) -> &SelfIds {
        &self.me
    }

    /// The claim-journal key afkd keeps this unit under ([`issue_key`]).
    fn key(&self) -> String {
        issue_key(
            &self.issue.id,
            &self.claim_id,
            self.source.as_ref().map(|s| s.id.as_str()),
        )
    }
}

/// The author ids afkd's own comments come back under: the viewer's, and the one Linear
/// reported on a comment this plugin itself posted (its claim, or the park marker the sweep
/// finds), when that is another.
///
/// That an app's own comments are authored by its user — the viewer — is a guess: Linear
/// may name its bot actor instead. Keying on both covers either answer, so afkd never reads
/// its own words as a human's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SelfIds(Vec<String>);

impl SelfIds {
    /// The viewer's id alone.
    pub(crate) fn viewer(id: String) -> Self {
        Self(vec![id])
    }

    /// These ids and `author`, when it is a new one and not empty.
    fn with(mut self, author: &str) -> Self {
        if !author.is_empty() && !self.contains(author) {
            self.0.push(author.to_string());
        }
        self
    }

    /// Whether `author` is one of them. An empty author — a comment Linear names no author
    /// for — is never afkd's by id.
    pub(crate) fn contains(&self, author: &str) -> bool {
        !author.is_empty() && self.0.iter().any(|id| id == author)
    }

    /// The viewer's id, the first: the wire unit's `self`.
    fn first(&self) -> &str {
        self.0.first().map_or("", String::as_str)
    }
}

/// One issue's finish that never reached Linear: what is still owed, so a later `release`
/// of the key finishes it rather than re-running the agent.
struct PendingFinish {
    /// The journal key the `release` arrives with ([`issue_key`]).
    key: String,
    /// The name of the state a give-up returns the issue to, for its line.
    returns_to: String,
    /// The issue itself: its title for the log lines, its id for every call.
    issue: Issue,
    /// Whether the [`NEEDS_INFO_LABEL`] **badge** a park owes is still unwritten. The badge is
    /// what the next beat's sweep re-finds the issue by, so a park whose label write failed
    /// is *not* delivered.
    park: bool,
    /// Whether the [`PARK_MARKER`] **owner marker** a park owes is still unposted. A badged
    /// issue naming no owner is anyone's, so a park that stopped there is not delivered
    /// either.
    park_marker: bool,
    /// The prior [`PARK_MARKER`] comments the owed marker's post still has to prune, trimmed
    /// to the ids Linear has not already dropped.
    park_priors: Vec<String>,
    /// Whether the claim comment is still on the issue, so the lease is still owed.
    release: bool,
    /// When the finish first failed to land: the start of its [`RETRY_FOR`].
    since: Instant,
}

impl PendingFinish {
    /// What of the finish is still owed, for the line that gives up on it.
    fn what(&self) -> String {
        [
            (self.park, "the park badge"),
            (self.park_marker, "the park owner marker"),
            (self.release, "the claim release"),
        ]
        .into_iter()
        .filter_map(|(owed, what)| owed.then_some(what))
        .collect::<Vec<_>>()
        .join(", ")
    }
}

/// The `discuss_with` gate resolved for one poll: whether any author is allowed, and — when
/// not — the allow-list of user ids, self never among them.
struct DiscussGate {
    anyone: bool,
    allowed: HashSet<String>,
}

/// The `require_member` gate resolved for one poll: the ids of the members it names, and
/// whether an issue nobody holds passes too.
struct MemberGate {
    ids: Vec<String>,
    nobody: bool,
}

impl MemberGate {
    /// Whether `issue` passes: nobody holds it and the gate takes that, or one of the members
    /// is its assignee or its delegate.
    fn holds(&self, issue: &Issue) -> bool {
        let held_by = |who: &Option<String>| who.as_ref().is_some_and(|id| self.ids.contains(id));
        (self.nobody && issue.assignee.is_none() && issue.delegate.is_none())
            || held_by(&issue.assignee)
            || held_by(&issue.delegate)
    }
}

/// The intake gates' identities, resolved once per poll.
struct Gates {
    /// The viewer: the tail boundary's and the brief's self.
    me: SelfIds,
    member: Option<MemberGate>,
    discuss: Option<DiscussGate>,
}

/// One `pick_from` scan's context, shared by every candidate it considers.
struct Beat<'a> {
    team_id: &'a str,
    gates: Gates,
    budget: &'a ScanBudget<'a>,
    diag: &'a dyn Diag,
    clock: &'a dyn Clock,
}

/// What considering one candidate decided.
enum Considered {
    /// It passed every gate and we won it. Boxed, as [`ClaimRound::Won`] is.
    Claimed(Box<Unit>),
    /// A gate passed it over, or a rival won it: the scan moves on.
    Passed,
    /// Too little of the call is left to post a claim: the scan stops, for the next beat.
    OutOfTime,
}

/// What one claim attempt on one candidate decided.
enum ClaimRound {
    /// We hold the sole live claim, and the unit is built from the very comment read that
    /// judged the race. Boxed because a bare `Unit` is several hundred bytes against an
    /// empty variant.
    Won(Box<Unit>),
    /// A rival's claim out-orders ours. Our claim comment is gone; the next candidate may be
    /// tried.
    Lost,
}

/// What the parked-issue sweep decided for the whole badged set.
enum ParkScan {
    /// A parked issue was answered and re-claimed; this is the beat's unit and `pick_from`
    /// is not read at all.
    Claimed(Box<Unit>),
    /// The scan ran past its budget: the beat ends idle, and the rest waits.
    Spent,
    /// Nothing parked, or nothing answered: fall through to the `pick_from` scan.
    Nothing,
}

/// Who this kind is, as `hello` says: on the tracker (`owner`, the per-process string the
/// claim comment carries) and in the config (`service`, plus `roster` — every service the
/// daemon runs, so a [`PARK_MARKER`] naming a service that is gone reads as orphaned rather
/// than as someone else's live work).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Identity {
    /// This afkd process's claim owner, carried on the claim and watermark comments.
    pub(crate) owner: String,
    /// The configured service, instance suffix stripped.
    pub(crate) service: String,
    /// Every service the daemon runs.
    pub(crate) roster: Vec<String>,
}

/// Where a write [`deliver`](LinearUnits::deliver)ed went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Delivery {
    /// Linear took it.
    Landed,
    /// Linear could not be reached, or the issue still owes earlier writes: the [`Outbox`]
    /// has it, and said so.
    Queued,
}

/// The `issue` kind's vendor half: the tracker seam, the intake gates, the comment-lock
/// claim and the brief.
pub(crate) struct LinearUnits {
    tracker: Box<dyn Tracker>,
    team_key: String,
    /// The team's id, resolved from its key on the first poll that needs it, and kept: a
    /// team's id never changes.
    team_id: Mutex<Option<String>>,
    /// The GraphQL endpoint a run reaches Linear at.
    api_url: String,
    /// The workflow states new work is claimed from, by name, in the order written.
    pick_from: Vec<String>,
    require_member: Option<Vec<Holder>>,
    require_label: Option<String>,
    without_label: Vec<String>,
    discuss_with: Option<DiscussWith>,
    settle: Duration,
    claim_lifetime: Duration,
    /// This process's claim owner identity.
    owner: String,
    /// The configured service: the name written onto a parked issue's [`PARK_MARKER`], and
    /// the one the parked sweep compares a marker against.
    service: String,
    /// Every service the daemon runs.
    roster: Vec<String>,
    /// Where the next beat's badged-issue sweep resumes, so a badged set larger than
    /// [`PARK_SCAN_MAX`] is covered across consecutive beats.
    park_cursor: Mutex<usize>,
    /// Finishes this process could not deliver, drained by
    /// [`release_stale`](Self::release_stale).
    pending: Mutex<Vec<PendingFinish>>,
    /// The current call's deadline, `None` between calls: the poll's scan reads it for its
    /// claim reserve.
    call_deadline: Mutex<Option<Instant>>,
    /// The writes Linear could not take, which the plugin's worker retries.
    outbox: Arc<Outbox>,
}

impl LinearUnits {
    /// The vendor half for `cfg`, talking to Linear through `tracker`, as `id`.
    pub(crate) fn new(tracker: Box<dyn Tracker>, cfg: &IssueConfig, id: Identity) -> Self {
        Self {
            tracker,
            team_key: cfg.team_key.clone(),
            team_id: Mutex::new(None),
            api_url: format!("{}{GRAPHQL_PATH}", cfg.base_url),
            pick_from: cfg.pick_from.clone(),
            require_member: cfg.require_member.clone(),
            require_label: cfg.require_label.clone(),
            without_label: cfg.without_label.clone(),
            discuss_with: cfg.discuss_with.clone(),
            settle: CLAIM_SETTLE,
            claim_lifetime: CLAIM_LIFETIME,
            owner: id.owner,
            service: id.service,
            roster: id.roster,
            park_cursor: Mutex::new(0),
            pending: Mutex::new(Vec::new()),
            call_deadline: Mutex::new(None),
            outbox: Arc::default(),
        }
    }

    /// The writes this kind owes Linear, for the worker that delivers them.
    pub(crate) fn outbox(&self) -> &Arc<Outbox> {
        &self.outbox
    }

    /// Do `write` on `issue`, or queue it: behind the issue's earlier writes while any is
    /// still owed, so one issue's writes land in the order they were asked for; and for a
    /// retry when Linear cannot take it now, its wait counted from when the inline try failed,
    /// read off `clock` after it. Every write queued is said, one line. A refusal no retry can
    /// fix is the caller's error, and nothing is queued.
    pub(crate) fn deliver(
        &self,
        issue: &Issue,
        write: Write,
        clock: &dyn Clock,
        diag: &dyn Diag,
    ) -> Result<Delivery, LinearError> {
        if self.outbox.owes(&issue.id) {
            diag.err(&queued_behind_line(issue, &write));
            self.outbox.queue_behind(issue, write, clock.now());
            return Ok(Delivery::Queued);
        }
        match write.perform(&*self.tracker, issue) {
            Ok(()) => Ok(Delivery::Landed),
            Err(e) if e.transient() => {
                diag.err(&queued_line(issue, &write, &e));
                self.outbox
                    .queue_retry(issue, write, clock.now(), e.retry_after());
                Ok(Delivery::Queued)
            }
            Err(e) => Err(e),
        }
    }

    /// Set (or, with `None`, clear) the current call's deadline: kept for the poll's claim
    /// reserve, and handed to the tracker, which clips every request to it.
    pub(crate) fn set_call_deadline(&self, deadline: Option<Instant>) {
        *lock(&self.call_deadline) = deadline;
        self.tracker.set_call_deadline(deadline);
    }

    /// The team's id, resolved from its key once and kept.
    fn team_id(&self) -> Result<String, LinearError> {
        let mut slot = lock(&self.team_id);
        if let Some(id) = &*slot {
            return Ok(id.clone());
        }
        let id = self.tracker.resolve_team(&self.team_key)?.id;
        *slot = Some(id.clone());
        Ok(id)
    }

    /// The team, its id resolved once and kept: what an action's write is done in.
    fn team(&self) -> Result<Team, LinearError> {
        Ok(Team {
            id: self.team_id()?,
            key: self.team_key.clone(),
        })
    }

    /// Every `pick_from` name resolved to its state, in the order named. An unknown name
    /// fails the poll naming it.
    fn sources(&self, team_id: &str) -> Result<Vec<State>, LinearError> {
        let states = self.tracker.states(team_id)?;
        self.pick_from
            .iter()
            .map(|name| state_named(&states, name, &self.team_key))
            .collect()
    }

    /// Do the write `action` a slot called on `issue`, in the service's team, or queue it
    /// as [`deliver`](Self::deliver) does; one that lands is narrated. A refusal no retry
    /// can fix is the caller's error.
    pub(crate) fn act(
        &self,
        issue: &Issue,
        action: IssueAction,
        clock: &dyn Clock,
        diag: &dyn Diag,
    ) -> Result<(), LinearError> {
        let team = self.team()?;
        let line = action_line(&action, &team, issue);
        if self.deliver(issue, Write::Act { team, action }, clock, diag)? == Delivery::Landed {
            diag.narrate(&line);
        }
        Ok(())
    }

    /// Do the read `read` a slot called on `issue` now, and write it to `to`, the comments
    /// rendered for a reader whose own ids are `me` ([`action::read`]).
    pub(crate) fn read(
        &self,
        issue: &Issue,
        me: &SelfIds,
        read: Read,
        to: &str,
        diag: &dyn Diag,
    ) -> Result<(), String> {
        action::read(&*self.tracker, issue, me, read, to, diag)
    }

    /// Fetch the files `issue` carries now, and write them into the directory `to`
    /// ([`file::fetch`]).
    pub(crate) fn fetch_files(
        &self,
        issue: &Issue,
        to: &str,
        diag: &dyn Diag,
    ) -> Result<(), String> {
        file::fetch(&*self.tracker, issue, to, diag)
    }

    /// The intake gates' identities for one poll: the viewer, the `require_member` users,
    /// resolved in the order written, and the `discuss_with` allow-list, self removed from it.
    /// Unset gates resolve nothing but the viewer, which the claim itself needs.
    fn resolve_gates(&self) -> Result<Gates, LinearError> {
        let me = SelfIds::viewer(self.tracker.viewer()?);
        let member = match &self.require_member {
            None => None,
            Some(holders) => {
                let mut gate = MemberGate {
                    ids: Vec::new(),
                    nobody: false,
                };
                for holder in holders {
                    match holder {
                        Holder::Member(who) => gate.ids.push(self.tracker.resolve_user(who)?),
                        Holder::Nobody => gate.nobody = true,
                    }
                }
                Some(gate)
            }
        };
        let discuss = match &self.discuss_with {
            None => None,
            Some(DiscussWith::Anyone) => Some(DiscussGate {
                anyone: true,
                allowed: HashSet::new(),
            }),
            Some(DiscussWith::Members(refs)) => {
                let mut allowed = HashSet::new();
                for who in refs {
                    allowed.insert(self.tracker.resolve_user(who)?);
                }
                // Self is never an allowed author, however it was named: afkd must not
                // answer itself.
                allowed.retain(|id| !me.contains(id));
                Some(DiscussGate {
                    anyone: false,
                    allowed,
                })
            }
        };
        Ok(Gates {
            me,
            member,
            discuss,
        })
    }

    /// The parked-issue sweep, run **ahead of** the `pick_from` scan on every beat: ask
    /// Linear which of the team's issues carry [`NEEDS_INFO_LABEL`], and re-claim the first one
    /// a human has answered — wherever it sits.
    ///
    /// The reply test is the feedback delta ([`feedback_comments`]), so afkd's own comments
    /// cannot re-arm an issue, and neither can comments a run already answered. Two gates are
    /// re-applied: `without_label`, a human's live veto, and blocked-by, since a blocker
    /// added while the issue was parked holds it back as one there at the first claim would.
    ///
    /// An issue's [`PARK_MARKER`] names who parked it, and only that service resumes it; an
    /// issue naming a service off the roster is an orphan, taken over with one narrated line.
    fn scan_parked(
        &self,
        team_id: &str,
        diag: &dyn Diag,
        clock: &dyn Clock,
        budget: &ScanBudget,
    ) -> Result<ParkScan, LinearError> {
        let badged = self.tracker.parked_issues(team_id, NEEDS_INFO_LABEL)?;
        if badged.is_empty() {
            return Ok(ParkScan::Nothing);
        }
        // The viewer, read only once something is badged, which keeps the idle beat at one
        // request.
        let viewer = SelfIds::viewer(self.tracker.viewer()?);
        let take = badged.len().min(PARK_SCAN_MAX);
        let start = {
            let mut cursor = lock(&self.park_cursor);
            let start = *cursor % badged.len();
            *cursor = start + take;
            start
        };
        // The `pick_from` states, resolved once an issue is about to be taken.
        let mut sources = None;
        for step in 0..take {
            if budget.spent() {
                return Ok(ParkScan::Spent);
            }
            let issue = &badged[(start + step) % badged.len()];
            if self.without_label.iter().any(|l| issue.labels.contains(l)) || blocked(issue) {
                continue;
            }
            let comments = match self.tracker.issue_comments(&issue.id) {
                Ok(comments) => comments,
                // One issue's read failing is not the poll failing: it stays badged and is
                // retried on a later beat.
                Err(e) => {
                    diag.err(&e);
                    continue;
                }
            };
            // The park marker is the plugin's own post, so its author is afkd's too.
            let me = park_authors(&comments).fold(viewer.clone(), SelfIds::with);
            // Someone already has this issue in hand.
            if has_live_claim(&comments, self.claim_lifetime) {
                continue;
            }
            // Nobody has answered yet: leave it alone.
            if feedback_comments(&comments, &me).is_empty() {
                continue;
            }
            // Theirs, and they are still here: sweep past it, silently.
            if self.parked_elsewhere(&comments) {
                continue;
            }
            let source = self.sweep_source(team_id, issue, &mut sources)?;
            if budget.claim_reserve_short() {
                return Ok(ParkScan::Spent);
            }
            if let Some(line) = self.orphan_takeover(issue, &comments) {
                diag.narrate(&line);
            }
            match self.attempt_claim(team_id, issue.clone(), source.as_ref(), &me, diag, clock)? {
                ClaimRound::Won(unit) => return Ok(ParkScan::Claimed(unit)),
                ClaimRound::Lost => continue,
            }
        }
        Ok(ParkScan::Nothing)
    }

    /// The `pick_from` state an issue the sweep claims returns to: the later state it sits
    /// in, or `None` — the key's default, the first state — for an issue in the first state
    /// or in none of them. A lone `pick_from` resolves nothing.
    fn sweep_source(
        &self,
        team_id: &str,
        issue: &Issue,
        sources: &mut Option<Vec<State>>,
    ) -> Result<Option<State>, LinearError> {
        if self.pick_from.len() < 2 {
            return Ok(None);
        }
        let sources = match sources {
            Some(sources) => sources,
            None => sources.insert(self.sources(team_id)?),
        };
        Ok(sources
            .iter()
            .skip(1)
            .find(|source| source.id == issue.state.id)
            .cloned())
    }

    /// Whether a parked issue is **another live service's**: its newest [`PARK_MARKER`]
    /// names a configured service this daemon runs that is not us.
    fn parked_elsewhere(&self, comments: &[Comment]) -> bool {
        let Some(owner) = park_owner(comments) else {
            return false;
        };
        let owner = instance_base(&owner).unwrap_or(&owner).to_string();
        owner != self.service && self.roster.contains(&owner)
    }

    /// The handover line owed for a parked issue this service is about to take over from a
    /// parker the config no longer runs — `None` when nothing is owed.
    fn orphan_takeover(&self, issue: &Issue, comments: &[Comment]) -> Option<String> {
        let owner = park_owner(comments)?;
        let owner = instance_base(&owner).unwrap_or(&owner);
        (owner != self.service).then(|| orphan_line(issue, owner))
    }

    /// Claim the next issue via the comment lock: the parked sweep first, then the
    /// `pick_from` states in the order named, and in each the first issue, in manual order,
    /// that passes every gate and wins its claim race. Every name resolves before any state
    /// is read, so a name the team lacks fails the poll even while an earlier state holds
    /// work.
    ///
    /// The gates run cheapest first, strictly before the claim comment is posted, so a
    /// skipped issue is never commented on: the holder (a member as assignee or delegate, or
    /// nobody), the deny
    /// label, the required label, blocked-by and the park badge off the state read, then off
    /// the issue's comments a live claim and the `discuss_with` tail. A badged issue is the
    /// parked sweep's alone.
    ///
    /// The whole scan runs under [`ScanBudget`]: past it, the beat ends idle and the rest
    /// waits for the next poll; so it does, before a claim is posted, once less than the
    /// claim reserve of the call's budget is left. A claim already under way always
    /// finishes.
    pub(crate) fn try_claim_next(
        &self,
        diag: &dyn Diag,
        clock: &dyn Clock,
    ) -> Result<Option<Unit>, LinearError> {
        let budget = ScanBudget::start(clock, diag, *lock(&self.call_deadline));
        let team_id = self.team_id()?;
        match self.scan_parked(&team_id, diag, clock, &budget)? {
            ParkScan::Claimed(unit) => return Ok(Some(*unit)),
            ParkScan::Spent => return Ok(None),
            ParkScan::Nothing => {}
        }
        let sources = self.sources(&team_id)?;
        let beat = Beat {
            team_id: &team_id,
            gates: self.resolve_gates()?,
            budget: &budget,
            diag,
            clock,
        };
        for (at, source) in sources.iter().enumerate() {
            // The first state is the key's default, so only a later one rides in it.
            let keyed = (at > 0).then_some(source);
            let mut after: Option<String> = None;
            loop {
                if budget.spent() {
                    return Ok(None);
                }
                let page = self
                    .tracker
                    .issues_in_state(&team_id, &source.id, after.as_deref())?;
                for issue in page.items {
                    if budget.spent() {
                        return Ok(None);
                    }
                    match self.consider(&beat, issue, keyed)? {
                        Considered::Claimed(unit) => return Ok(Some(*unit)),
                        Considered::Passed => {}
                        Considered::OutOfTime => return Ok(None),
                    }
                }
                match page.next {
                    Some(next) => after = Some(next),
                    None => break,
                }
            }
        }
        Ok(None)
    }

    /// Run one candidate through the gates and, when it passes them all, race for it.
    fn consider(
        &self,
        beat: &Beat,
        issue: Issue,
        source: Option<&State>,
    ) -> Result<Considered, LinearError> {
        let gates = &beat.gates;
        if let Some(member) = &gates.member {
            if !member.holds(&issue) {
                return Ok(Considered::Passed);
            }
        }
        // Deny first, so deny wins over `require_label`.
        if self.without_label.iter().any(|l| issue.labels.contains(l)) {
            return Ok(Considered::Passed);
        }
        if let Some(label) = &self.require_label {
            if !issue.labels.contains(label) {
                return Ok(Considered::Passed);
            }
        }
        if blocked(&issue) {
            return Ok(Considered::Passed);
        }
        // Parked: the sweep re-claims it once a human answers, wherever it sits. A parked
        // issue no slot moved on still sits in the state it was claimed from, so claiming it
        // here would run it again unanswered.
        if issue.labels.iter().any(|l| l == NEEDS_INFO_LABEL) {
            return Ok(Considered::Passed);
        }
        let comments = self.tracker.issue_comments(&issue.id)?;
        // Someone holds this issue's lease: step over it.
        if has_live_claim(&comments, self.claim_lifetime) {
            return Ok(Considered::Passed);
        }
        if let Some(gate) = &gates.discuss {
            if !discuss_tail_passes(&comments, &gates.me, gate) {
                return Ok(Considered::Passed);
            }
        }
        // The gates' own reads may have spent the reserve: post nothing this late.
        if beat.budget.claim_reserve_short() {
            return Ok(Considered::OutOfTime);
        }
        let round = self.attempt_claim(
            beat.team_id,
            issue,
            source,
            &gates.me,
            beat.diag,
            beat.clock,
        )?;
        Ok(match round {
            ClaimRound::Won(unit) => Considered::Claimed(unit),
            ClaimRound::Lost => Considered::Passed,
        })
    }

    /// Race one candidate for the comment lock: post a claim, settle so a rival's earlier
    /// claim has time to be seen, re-read the thread, and win only as the single earliest
    /// still-live claim ([`won_claim`]).
    ///
    /// The win narrates **here**, and takes the park badge and owner marker off an issue
    /// that carried them. A failed re-read is a tracker fault, not a lost round: our claim is
    /// released and the error propagates.
    fn attempt_claim(
        &self,
        team_id: &str,
        issue: Issue,
        source: Option<&State>,
        me: &SelfIds,
        diag: &dyn Diag,
        clock: &dyn Clock,
    ) -> Result<ClaimRound, LinearError> {
        let claim = self
            .tracker
            .post_comment(&issue.id, &claim_text(&self.owner))?;
        // The claim is one afkd posted, so its author, whoever Linear says that is, is afkd.
        let me = me.clone().with(&claim.author);
        clock.sleep(self.settle);
        let mut comments = match self.tracker.issue_comments(&issue.id) {
            Ok(c) => c,
            Err(e) => {
                self.drop_claim(&claim.id, diag);
                return Err(e);
            }
        };
        if !won_claim(&claim_markers(&comments), &claim.id, self.claim_lifetime) {
            self.drop_claim(&claim.id, diag);
            return Ok(ClaimRound::Lost);
        }
        // The run's credential, read now: an app's token is the one the claim just used.
        let authorization = match self.tracker.authorization() {
            Ok(authorization) => authorization,
            Err(e) => {
                self.drop_claim(&claim.id, diag);
                return Err(e);
            }
        };
        diag.narrate(&claimed_line(&issue));
        // The issue is being worked, so it is no longer waiting on anyone: take the badge
        // off, and the owner marker with it. Best-effort: the claim is won either way.
        if issue.labels.iter().any(|l| l == NEEDS_INFO_LABEL) {
            if let Err(e) = self
                .tracker
                .remove_label(team_id, &issue.id, NEEDS_INFO_LABEL)
            {
                diag.err(&e);
            }
            let mut removed = Vec::new();
            for prior in comments.iter().filter(|c| is_park(&c.text)) {
                match self.tracker.delete_comment(&prior.id) {
                    Ok(()) => removed.push(prior.id.clone()),
                    Err(e) => diag.err(&e),
                }
            }
            // Keep the snapshot honest: `finish`'s park prune walks it, and a marker left in
            // it after Linear dropped it would be deleted a second time.
            comments.retain(|c| !removed.contains(&c.id));
        }
        let feedback = feedback_comments(&comments, &me);
        let delivered = delivered_upto(&comments, &me, &feedback);
        Ok(ClaimRound::Won(Box::new(Unit {
            issue,
            claim_id: claim.id,
            comments,
            delivered_upto: delivered,
            me,
            source: source.cloned(),
            authorization,
        })))
    }

    /// Release a claim comment this poll posted but will not keep. Best-effort but not
    /// silent.
    fn drop_claim(&self, claim_id: &str, diag: &dyn Diag) {
        if let Err(e) = self.tracker.delete_comment(claim_id) {
            diag.err(&e);
        }
    }

    /// On the `discuss_with` path, guarantee afkd is the last speaker after a turn: re-read
    /// the issue's comments and, if no comment authored by afkd this turn is new, post one
    /// backstop comment. If the re-read fails, post anyway — a rare redundant comment beats
    /// the re-fire loop. A Linear that cannot be reached gets it from the [`Outbox`].
    fn post_backstop_if_silent(
        &self,
        unit: &Unit,
        outcome: UnitOutcome,
        facts: &Facts,
        clock: &dyn Clock,
        diag: &dyn Diag,
    ) {
        let spoke = match self.tracker.issue_comments(&unit.issue.id) {
            Ok(after) => {
                let before: HashSet<&str> = unit.comments.iter().map(|c| c.id.as_str()).collect();
                after
                    .iter()
                    .any(|c| unit.me.contains(&c.author) && !before.contains(c.id.as_str()))
            }
            Err(e) => {
                diag.err(&e);
                false
            }
        };
        if !spoke {
            let backstop = Write::Note(backstop_text(outcome, facts));
            if let Err(e) = self.deliver(&unit.issue, backstop.clone(), clock, diag) {
                diag.err(&lost_line(&unit.issue, &backstop, &e, None));
            }
        }
    }

    /// Remember a finish this process could not deliver, for
    /// [`release_stale`](Self::release_stale) to replay. Past [`PENDING_MAX`] the oldest goes.
    fn remember_pending(&self, owed: PendingFinish) {
        let mut pending = lock(&self.pending);
        pending.push(owed);
        if pending.len() > PENDING_MAX {
            pending.remove(0);
        }
    }

    /// Take the outstanding finish recorded for `key`, if any.
    fn take_pending(&self, key: &str) -> Option<PendingFinish> {
        let mut pending = lock(&self.pending);
        let at = pending.iter().position(|p| p.key == key)?;
        Some(pending.remove(at))
    }

    /// One beat's attempt at an owed finish: the park badge, then the owner marker, then the
    /// lease release — `finish`'s order, so a partial delivery resumes rather than restarts.
    /// `Some(true)` when everything landed, `Some(false)` when something is still owed and
    /// kept for the next beat, and `None` once Linear has refused what is owed, or not taken
    /// it for [`RETRY_FOR`] — the entry is dropped and the caller falls through to the
    /// crash-victim reversal.
    ///
    /// A claim Linear says is already gone is released: a release whose reply was lost may
    /// well have landed.
    fn retry_pending(
        &self,
        mut owed: PendingFinish,
        claim_id: &str,
        now: Instant,
        diag: &dyn Diag,
    ) -> Option<bool> {
        let mut failed: Option<LinearError> = None;
        let mut fail = |e: LinearError| {
            diag.err(&e);
            failed = Some(e);
        };
        if owed.park {
            match self.team_id().and_then(|team_id| {
                self.tracker
                    .add_label(&team_id, &owed.issue.id, NEEDS_INFO_LABEL)
            }) {
                Ok(()) => {
                    diag.narrate(&parked_line(&owed.issue));
                    owed.park = false;
                }
                Err(e) => fail(e),
            }
        }
        if owed.park_marker {
            match self
                .tracker
                .post_comment(&owed.issue.id, &park_text(&self.service))
            {
                Ok(_) => {
                    owed.park_marker = false;
                    self.prune_park_priors(&mut owed.park_priors, diag);
                }
                Err(e) => fail(e),
            }
        }
        let owed_before_release = owed.park || owed.park_marker;
        if !owed_before_release && owed.release {
            match self.tracker.delete_comment(claim_id) {
                Ok(()) | Err(LinearError::NotFound { .. }) => {
                    diag.narrate(&released_line(&owed.issue));
                    owed.release = false;
                }
                Err(e) => fail(e),
            }
        }
        if !owed_before_release && !owed.release {
            return Some(true);
        }
        let refused = failed.as_ref().is_some_and(|e| !e.transient());
        if refused || now >= owed.since + RETRY_FOR {
            let why = failed.map_or_else(String::new, |e| format!(" — {e}"));
            diag.err(&format_args!(
                "giving up on the finish for {} after {}: {} still owed{why}; returning it to \
                 \"{}\"",
                named(&owed.issue),
                crate::outbox::span(now.saturating_duration_since(owed.since)),
                owed.what(),
                owed.returns_to
            ));
            return None;
        }
        self.remember_pending(owed);
        Some(false)
    }

    /// Delete the prior [`PARK_MARKER`] comments a just-posted marker supersedes, leaving in
    /// `priors` exactly the ids Linear still carries. Best-effort: [`park_owner`] reads the
    /// newest, and the fresh one always is.
    fn prune_park_priors(&self, priors: &mut Vec<String>, diag: &dyn Diag) {
        priors.retain(|id| match self.tracker.delete_comment(id) {
            Ok(()) => false,
            Err(e) => {
                diag.err(&e);
                true
            }
        });
    }

    /// Finish, or reverse, a claim named by a whole journal key — the `release` call.
    ///
    /// **The finish is still owed** (a `held` finish): replay it. **Otherwise it is a crash
    /// victim**, or a unit afkd handed back unrun: prune the `[afkd-claim]` comment and move
    /// the issue back to the **bottom** of the state it was claimed from — the first
    /// `pick_from` state for a key that names none — so a repeatedly-crashing issue cannot
    /// starve the queue head. Best-effort: a tracker failure is diagnosed and the key still
    /// released.
    ///
    /// `None` for a key with no `#`, which names nothing releasable.
    pub(crate) fn release_stale(&self, key: &str, now: Instant, diag: &dyn Diag) -> Option<bool> {
        let (issue_id, claim_id, state_id) = split_issue_key(key)?;
        if let Some(owed) = self.take_pending(key) {
            if let Some(verdict) = self.retry_pending(owed, claim_id, now, diag) {
                return Some(verdict);
            }
        }
        if let Err(e) = self.tracker.delete_comment(claim_id) {
            diag.err(&e);
        }
        let moved = self.team_id().and_then(|team_id| {
            let state_id = match state_id {
                Some(state_id) => state_id.to_string(),
                None => match self.sources(&team_id)?.into_iter().next() {
                    Some(first) => first.id,
                    None => return Ok(()),
                },
            };
            self.tracker.move_to_bottom(&team_id, issue_id, &state_id)
        });
        if let Err(e) = moved {
            diag.err(&e);
        }
        Some(true)
    }

    /// Keep this issue's claim comment alive for as long as afkd's fire holds it.
    /// Best-effort: a failed edit is diagnosed, and the next renewal is minutes away.
    pub(crate) fn renew(&self, unit: &Unit, renewal: u64, diag: &dyn Diag) {
        if let Err(e) = self
            .tracker
            .edit_comment(&unit.claim_id, &claim_renewal_text(&self.owner, renewal))
        {
            diag.err(&e);
        }
    }

    /// The unit as it crosses the wire in a `poll` reply:
    ///
    /// - `id`: the sanitized identifier (the run-name component);
    /// - `key`: the claim-journal key ([`issue_key`]);
    /// - `fields`: the handle's fields, as the claiming read saw the issue;
    /// - `thread`: the issue's uuid, stable across every claim and every move of team;
    /// - `seen`: the claim-read comment ids, which the brief already carries;
    /// - `self`: the viewer's id;
    /// - `env`: the endpoint, the credential, and the team's and the issue's ids;
    /// - `files`: the brief, unframed — afkd frames `task.md` itself.
    pub(crate) fn wire_unit(&self, unit: &Unit) -> WireUnit {
        let issue = &unit.issue;
        WireUnit {
            id: sanitize(&issue.identifier),
            key: unit.key(),
            fields: IssueFields {
                title: issue.title.clone(),
                url: issue.url.clone(),
                labels: issue.labels.clone(),
                state: issue.state.name.clone(),
                parent: issue
                    .parent
                    .as_ref()
                    .map_or_else(String::new, |p| p.identifier.clone()),
                sub_issues: issue
                    .children
                    .iter()
                    .map(|c| c.identifier.clone())
                    .collect(),
                blocked_by: issue
                    .blockers
                    .iter()
                    .map(|b| b.identifier.clone())
                    .collect(),
            },
            thread: issue.id.clone(),
            seen: unit.comments.iter().map(|c| c.id.clone()).collect(),
            me: unit.me.first().to_string(),
            env: BTreeMap::from([
                (ENV_API_URL.to_string(), self.api_url.clone()),
                (ENV_AUTHORIZATION.to_string(), unit.authorization.clone()),
                (
                    ENV_TEAM_ID.to_string(),
                    lock(&self.team_id).clone().unwrap_or_default(),
                ),
                (ENV_ISSUE_ID.to_string(), issue.id.clone()),
                (ENV_ISSUE.to_string(), issue.identifier.clone()),
            ]),
            files: vec![WireFile {
                path: TASK_FILE.to_string(),
                text: brief_text(issue, &unit.comments, &unit.me),
            }],
        }
    }

    /// The clarification gate's read: a [`PARK_FILE`] marker under the attempt's scratch
    /// root means the agent asked for human input, so the attempt **parks** the issue. The
    /// marker overrides afkd's own verdict.
    pub(crate) fn classify(scratch: &Path, verdict: UnitOutcome) -> UnitOutcome {
        if scratch.join(PARK_FILE).is_file() {
            return UnitOutcome::Park;
        }
        verdict
    }

    /// A faulting attempt leaves a marked comment on the issue, so the budget spent on it is
    /// legible from Linear itself. afkd sends the fault's sentence; with none there is
    /// nothing to mark. A Linear that cannot take it now gets it from the [`Outbox`].
    pub(crate) fn attempt_failed(
        &self,
        unit: &Unit,
        n: u32,
        max: u32,
        reason: Option<&str>,
        clock: &dyn Clock,
        diag: &dyn Diag,
    ) {
        let Some(reason) = reason else {
            return;
        };
        let note = Write::Note(attempt_text(n, max, reason));
        if let Err(e) = self.deliver(&unit.issue, note.clone(), clock, diag) {
            diag.err(&lost_line(&unit.issue, &note, &e, None));
        }
    }

    /// Every comment on the unit's issue, for afkd's mid-run watch.
    pub(crate) fn comments(&self, unit: &Unit) -> Result<Vec<Comment>, LinearError> {
        self.tracker.issue_comments(&unit.issue.id)
    }

    /// The end of a unit's run: either the durable `[afkd-ran]` watermark (the default path)
    /// or the last-speaker backstop (the `discuss_with` path), and the lease release last of
    /// all. A park adds the [`NEEDS_INFO_LABEL`] badge first, and the [`PARK_MARKER`] naming
    /// this service after the branch and **before** the release, so the issue is never
    /// badged, unclaimed and ownerless at once.
    ///
    /// Returns whether the finish was delivered. Anything short of "the badge and owner
    /// marker are up, and the claim comment is gone" is remembered, and a later `release` of
    /// the key finishes it. The watermark or backstop is not part of that: one Linear cannot
    /// take goes to the [`Outbox`].
    pub(crate) fn finish(
        &self,
        unit: &Unit,
        outcome: UnitOutcome,
        facts: &Facts,
        clock: &dyn Clock,
        diag: &dyn Diag,
    ) -> bool {
        let issue = &unit.issue;
        let mut park_owed = matches!(outcome, UnitOutcome::Park);
        if park_owed {
            match self.team_id().and_then(|team_id| {
                self.tracker
                    .add_label(&team_id, &issue.id, NEEDS_INFO_LABEL)
            }) {
                Ok(()) => {
                    diag.narrate(&parked_line(issue));
                    park_owed = false;
                }
                Err(e) => diag.err(&e),
            }
        }
        match &self.discuss_with {
            // Default path: post the durable high-water mark carrying `delivered_upto`, then
            // prune the priors out of the claim-read snapshot, once it has landed.
            None => {
                let priors: Vec<String> = unit
                    .comments
                    .iter()
                    .filter(|c| is_ran(&c.text))
                    .map(|c| c.id.clone())
                    .collect();
                let watermark = Write::Watermark {
                    text: ran_text(&self.owner, unit.delivered_upto),
                    priors: priors.clone(),
                };
                match self.deliver(issue, watermark.clone(), clock, diag) {
                    Ok(Delivery::Landed) => crate::outbox::prune(&*self.tracker, &priors, diag),
                    Ok(Delivery::Queued) => {}
                    Err(e) => diag.err(&lost_line(issue, &watermark, &e, None)),
                }
            }
            // Grooming path: afkd's own last comment IS the tail boundary, so the backstop
            // guarantees afkd is the last speaker.
            Some(_) => self.post_backstop_if_silent(unit, outcome, facts, clock, diag),
        }
        // The park owner marker: AFTER the watermark/backstop branch (posted before it, it
        // would count as afkd speaking and suppress the backstop) and BEFORE the release (a
        // badged issue naming no owner is anyone's).
        let mut marker_owed = matches!(outcome, UnitOutcome::Park);
        let mut park_priors: Vec<String> = if marker_owed {
            unit.comments
                .iter()
                .filter(|c| is_park(&c.text))
                .map(|c| c.id.clone())
                .collect()
        } else {
            Vec::new()
        };
        if marker_owed {
            match self
                .tracker
                .post_comment(&issue.id, &park_text(&self.service))
            {
                Ok(_) => {
                    marker_owed = false;
                    self.prune_park_priors(&mut park_priors, diag);
                }
                Err(e) => diag.err(&e),
            }
        }
        let mut release_owed = true;
        if !park_owed && !marker_owed {
            match self.tracker.delete_comment(&unit.claim_id) {
                Ok(()) => {
                    diag.narrate(&released_line(issue));
                    release_owed = false;
                }
                Err(e) => diag.err(&e),
            }
        }
        if release_owed {
            self.remember_pending(PendingFinish {
                key: unit.key(),
                returns_to: unit.source.as_ref().map_or_else(
                    || self.pick_from.first().cloned().unwrap_or_default(),
                    |source| source.name.clone(),
                ),
                issue: issue.clone(),
                park: park_owed,
                park_marker: marker_owed,
                park_priors,
                release: true,
                since: clock.now(),
            });
        }
        !release_owed
    }
}

/// Reduce an identifier to a slug safe for a run name. An identifier is `ENG-123` already,
/// so this only ever bites on a team key Linear let through with another character.
fn sanitize(id: &str) -> String {
    let slug: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if slug.is_empty() {
        "issue".to_string()
    } else {
        slug
    }
}

// --- Pure helpers (no seam): claim/marker logic, task text, the brief.

/// Whether an issue is held back by a blocker not yet done: one whose state is in none of
/// the [`CLOSED_KINDS`].
fn blocked(issue: &Issue) -> bool {
    issue
        .blockers
        .iter()
        .any(|b| !CLOSED_KINDS.contains(&b.state_kind.as_str()))
}

/// The base name of an instance copy — `afkd::develop+1` is `afkd::develop` — or `None`
/// for a name that is not instance-shaped: the suffix is read off the **last** `+`, both
/// halves must be non-empty, and the tail must be all ASCII digits.
fn instance_base(name: &str) -> Option<&str> {
    let (base, digits) = name.rsplit_once('+')?;
    let shaped =
        !base.is_empty() && !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit());
    shaped.then_some(base)
}

/// The claim-journal key for an issue's in-flight claim: the issue's uuid (the put-back
/// subject at reap), the claim comment's uuid (the prune target) and, for an issue claimed
/// from a `pick_from` state after the first, that state's id (where the reversal returns
/// it), joined by `#`.
fn issue_key(issue_id: &str, claim_id: &str, state_id: Option<&str>) -> String {
    match state_id {
        Some(state_id) => format!("{issue_id}#{claim_id}#{state_id}"),
        None => format!("{issue_id}#{claim_id}"),
    }
}

/// Split an [`issue_key`] back into its `(issue_id, claim_id, state_id)`. `None` if the key
/// carries no `#`. A uuid holds no `#`, and an empty state part names no state.
fn split_issue_key(key: &str) -> Option<(&str, &str, Option<&str>)> {
    let (issue_id, rest) = key.split_once('#')?;
    Some(match rest.split_once('#') {
        Some((claim_id, state_id)) => {
            (issue_id, claim_id, Some(state_id).filter(|s| !s.is_empty()))
        }
        None => (issue_id, rest, None),
    })
}

/// Map an issue's comments into the view the claim decision reads. The whole list is
/// handed over, unfiltered, because the marker predicate lives inside the decision.
fn claim_markers(comments: &[Comment]) -> Vec<ClaimMarker> {
    comments
        .iter()
        .map(|c| ClaimMarker {
            id: c.id.clone(),
            posted_at: c.posted_at,
            renewed_at: c.renewed_at,
            text: c.text.clone(),
        })
        .collect()
}

/// The text of a failed-attempt marker comment (`n/max: reason`), the reason cut so the
/// whole comment fits [`COMMENT_MAX`].
fn attempt_text(n: u32, max: u32, reason: &str) -> String {
    let head = format!("{ATTEMPT_MARKER} {n}/{max}: ");
    let units = |s: &str| s.encode_utf16().count();
    let room = COMMENT_MAX - units(&head);
    if units(reason) <= room {
        return format!("{head}{reason}");
    }
    let mut left = room - units(CUT_MARK);
    let kept: String = reason
        .chars()
        .take_while(|c| {
            let fits = c.len_utf16() <= left;
            left = left.saturating_sub(c.len_utf16());
            fits
        })
        .collect();
    format!("{head}{kept}{CUT_MARK}")
}

/// How a success line names an issue: its identifier, which finds it, and its title, which
/// a person reads.
fn issue_words(issue: &Issue) -> String {
    format!("issue {} \"{}\"", issue.identifier, issue.title)
}

/// The success line for a won claim.
fn claimed_line(issue: &Issue) -> String {
    format!("{ACTION_TAG} claimed {}", issue_words(issue))
}

/// The success line for a released lease.
fn released_line(issue: &Issue) -> String {
    format!("{ACTION_TAG} released claim on {}", issue_words(issue))
}

/// The handover line for a parked issue whose owner marker names a service this config no
/// longer runs. A handover is not a fault, so it is narrated, not diagnosed.
fn orphan_line(issue: &Issue, gone: &str) -> String {
    format!(
        "{ACTION_TAG} resuming {}, parked by service \"{gone}\" which this config no longer runs",
        issue_words(issue)
    )
}

/// The success line for a parked issue, naming the badge a person will see in Linear.
fn parked_line(issue: &Issue) -> String {
    format!(
        "{ACTION_TAG} parked {} awaiting a reply (label \"{NEEDS_INFO_LABEL}\")",
        issue_words(issue)
    )
}

/// The task text: the title, or the title, a blank line, then the description.
fn task_text(issue: &Issue) -> String {
    if issue.description.trim().is_empty() {
        issue.title.clone()
    } else {
        format!("{}\n\n{}", issue.title, issue.description)
    }
}

/// Where the issue sits among others: the line naming its parent, and a `## Sub-issues`
/// section listing each child with its state; `""` for an issue with neither.
fn render_family(issue: &Issue) -> String {
    let mut parts = Vec::new();
    if let Some(parent) = &issue.parent {
        parts.push(format!(
            "Sub-issue of {}: {}",
            parent.identifier, parent.title
        ));
    }
    if !issue.children.is_empty() {
        let mut s = String::from("## Sub-issues\n");
        for child in &issue.children {
            s.push('\n');
            s.push_str(&sub_issue_line(child));
        }
        parts.push(s);
    }
    parts.join("\n\n")
}

/// One sub-issue as the brief lists it, and `list_sub_issues` writes it: its identifier,
/// its state and its title.
pub(crate) fn sub_issue_line(child: &Child) -> String {
    format!("- {} ({}) {}", child.identifier, child.state, child.title)
}

/// Text of the run-end watermark comment: the marker, `owner`, and the delivery boundary
/// `upto=<epoch-milliseconds>` the next run measures its delta from.
fn ran_text(owner: &str, upto: SystemTime) -> String {
    let millis = upto.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis());
    format!("{RAN_MARKER} owner={owner} upto={millis}")
}

/// The backstop comment posted at the end of a silent `discuss_with` turn. It carries no
/// marker: afkd posts it, so it is self-authored, which is what both the tail boundary and
/// the brief key on.
fn backstop_text(outcome: UnitOutcome, facts: &Facts) -> String {
    match outcome {
        UnitOutcome::Clean => "reviewed, nothing to add".to_string(),
        UnitOutcome::Park => "awaiting a human reply".to_string(),
        UnitOutcome::Failed => format!("run did not complete: {}", facts.fault().unwrap_or("")),
    }
}

/// Whether a comment's text, unescaped and its leading space dropped, starts with `marker`.
fn starts_with_marker(text: &str, marker: &str) -> bool {
    unescape(text).trim_start().starts_with(marker)
}

/// Whether a comment is the run-end watermark marker.
fn is_ran(text: &str) -> bool {
    starts_with_marker(text, RAN_MARKER)
}

/// Text of the park owner comment: the marker and the **configured service** that parked
/// the issue — never the claim owner, which changes on every restart.
fn park_text(service: &str) -> String {
    format!("{PARK_MARKER} service={service}")
}

/// Whether a comment is the park owner marker — the prefix test, so a marker whose
/// `service=` is missing or garbled is still pruned, deleted and hidden from the brief.
fn is_park(text: &str) -> bool {
    starts_with_marker(text, PARK_MARKER)
}

/// The value after `key=` among the words of a marker comment starting with `marker`.
fn marker_field(text: &str, marker: &str, key: &str) -> Option<String> {
    let text = unescape(text);
    let t = text.trim_start();
    if !t.starts_with(marker) {
        return None;
    }
    let prefix = format!("{key}=");
    t.split_whitespace()
        .find_map(|w| w.strip_prefix(prefix.as_str()))
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// The service named on the issue's **newest** `[afkd-park]` marker, if any — newest, since
/// the prune that should leave exactly one is best-effort. A newest marker whose value is
/// garbled names *nobody* rather than deferring to an older one.
fn park_owner(comments: &[Comment]) -> Option<String> {
    comments
        .iter()
        .filter(|c| is_park(&c.text))
        .max_by_key(|c| (c.posted_at, c.id.as_str()))
        .and_then(|c| marker_field(&c.text, PARK_MARKER, "service"))
}

/// The authors of the issue's `[afkd-park]` markers: afkd's own ids, since only the plugin
/// posts one.
fn park_authors(comments: &[Comment]) -> impl Iterator<Item = &str> {
    comments
        .iter()
        .filter(|c| is_park(&c.text))
        .map(|c| c.author.as_str())
}

/// The delivery boundary stamped on an `[afkd-ran]` marker's text, if present and parseable.
fn ran_upto(text: &str) -> Option<SystemTime> {
    marker_field(text, RAN_MARKER, "upto")
        .and_then(|s| s.parse::<u64>().ok())
        .map(|millis| UNIX_EPOCH + Duration::from_millis(millis))
}

/// The high-water mark set by prior `[afkd-ran]` markers: the newest delivery boundary they
/// carry. A marker whose `upto=` cannot be parsed contributes nothing.
fn watermark_of(comments: &[Comment]) -> Option<SystemTime> {
    comments.iter().filter_map(|c| ran_upto(&c.text)).max()
}

/// The one line between prior and new comments, for the parked sweep, the brief and the
/// delivery boundary alike: the later of the [`watermark_of`] and afkd's newest
/// **conversational** comment — control markers excluded, so this run's own claim cannot
/// mask every real human comment. On a park, that comment is the question the parking run
/// asked. `None` only when the issue carries neither.
fn feedback_boundary(comments: &[Comment], me: &SelfIds) -> Option<SystemTime> {
    let last_word = comments
        .iter()
        .filter(|c| is_afkd(c, me) && !is_control_marker(&c.text))
        .map(|c| c.posted_at)
        .max();
    watermark_of(comments).max(last_word)
}

/// The delivery boundary to stamp on this run's watermark: the [`feedback_boundary`] the
/// delta was measured from or the newest delivered comment, whichever is later — anchored to
/// Linear's post times, never the local clock.
fn delivered_upto(comments: &[Comment], me: &SelfIds, feedback: &[Comment]) -> SystemTime {
    let prior = feedback_boundary(comments, me);
    let newest_delivered = feedback.iter().map(|c| c.posted_at).max();
    [prior, newest_delivered]
        .into_iter()
        .flatten()
        .max()
        .unwrap_or(UNIX_EPOCH)
}

/// Whether a comment is one of afkd's bookkeeping **control markers** (claim / attempt /
/// ran / park), dropped from the brief entirely.
pub(crate) fn is_control_marker(text: &str) -> bool {
    is_claim(text) || starts_with_marker(text, ATTEMPT_MARKER) || is_ran(text) || is_park(text)
}

/// Whether a comment is afkd's own: authored under one of its ids, or a control marker,
/// which nothing else writes.
pub(crate) fn is_afkd(c: &Comment, me: &SelfIds) -> bool {
    me.contains(&c.author) || is_control_marker(&c.text)
}

/// The human-feedback delta to deliver this run: comments posted strictly after the
/// [`feedback_boundary`], with afkd's own dropped, oldest-first. With no boundary, all human
/// comments are kept.
fn feedback_comments(comments: &[Comment], me: &SelfIds) -> Vec<Comment> {
    let boundary = feedback_boundary(comments, me);
    let mut kept: Vec<Comment> = comments
        .iter()
        .filter(|c| !is_afkd(c, me))
        .filter(|c| boundary.is_none_or(|b| c.posted_at > b))
        .cloned()
        .collect();
    kept.sort_by(|a, b| (a.posted_at, &a.id).cmp(&(b.posted_at, &b.id)));
    kept
}

/// One comment rendered for the brief: `**<speaker>:** <body>`, the speaker the literal
/// `afkd` for afkd's own, else the commenter's display name.
fn render_comment(c: &Comment, me: &SelfIds) -> String {
    let speaker = if is_afkd(c, me) {
        "afkd"
    } else {
        &c.author_name
    };
    format!("**{speaker}:** {}", c.text.trim())
}

/// Comments rendered as the brief renders a section of its thread, and `list_comments`
/// writes the thread: each [`render_comment`], a blank line between two, every one ending
/// its line. `""` for none.
pub(crate) fn render_comments(comments: &[&Comment], me: &SelfIds) -> String {
    comments
        .iter()
        .map(|c| format!("{}\n", render_comment(c, me)))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The brief afkd writes to `task.md`: the task text, the issue's parent and sub-issues,
/// then the **full comment thread** — prior conversation and the new, actionable comments,
/// each under its own heading, both oldest-first. Control markers are dropped.
fn brief_text(issue: &Issue, comments: &[Comment], me: &SelfIds) -> String {
    let mut s = task_text(issue);
    let family = render_family(issue);
    if !family.is_empty() {
        s.push_str("\n\n");
        s.push_str(&family);
    }
    let new = feedback_comments(comments, me);
    let new_ids: HashSet<&str> = new.iter().map(|c| c.id.as_str()).collect();
    let mut prior: Vec<&Comment> = comments
        .iter()
        .filter(|c| !is_control_marker(&c.text))
        .filter(|c| !new_ids.contains(c.id.as_str()))
        .collect();
    prior.sort_by(|a, b| (a.posted_at, &a.id).cmp(&(b.posted_at, &b.id)));

    if !prior.is_empty() {
        s.push_str("\n\n## Earlier conversation\n\n");
        s.push_str(&render_comments(&prior, me));
    }
    if !new.is_empty() {
        s.push_str("\n\n## New comments\n\n");
        s.push_str(&render_comments(&new.iter().collect::<Vec<_>>(), me));
    }
    s
}

/// How old a comment is right now, saturating at zero for a time in the future. The one
/// place this kind reads a wall clock.
fn age_of(at: SystemTime) -> Duration {
    SystemTime::now().duration_since(at).unwrap_or_default()
}

/// Whether `comments` carry a still-live `[afkd-claim]` — someone's lease on this issue, so
/// the scan must step over it. Measured on the **renewal**, against the wall clock, with the
/// inclusive `<= lifetime` boundary [`won_claim`] uses.
fn has_live_claim(comments: &[Comment], lifetime: Duration) -> bool {
    comments
        .iter()
        .any(|c| is_claim(&c.text) && age_of(c.renewed_at) <= lifetime)
}

/// Whether an issue's `comments` pass the `discuss_with` claim gate. The boundary is afkd's
/// own last comment, a `[afkd-claim]` (the lease, not speech) and a `[afkd-park]`
/// (bookkeeping posted after the turn's last word) aside. On first sight — afkd never
/// commented — the issue fires unconditionally; after that, iff a comment past the boundary
/// is from an allowed author who is not afkd. The whole tail is scanned, so a disallowed
/// author cannot mask an allowed one.
fn discuss_tail_passes(comments: &[Comment], me: &SelfIds, gate: &DiscussGate) -> bool {
    let spoke = |c: &&Comment| me.contains(&c.author) && !is_claim(&c.text) && !is_park(&c.text);
    let Some(boundary) = comments.iter().filter(spoke).map(|c| c.posted_at).max() else {
        return true;
    };
    comments.iter().any(|c| {
        c.posted_at > boundary
            && !me.contains(&c.author)
            && (gate.anyone || gate.allowed.contains(&c.author))
    })
}

#[cfg(test)]
mod tests {
    //! No network: these drive the in-memory `MockTracker` on a fake clock.

    use super::*;
    use crate::common::{CaptureDiag, FakeClock, TempDir};
    use crate::settings::issue_config;
    use crate::tracker::{Action, Blocker, Child, MockTracker, Related, SELF_ID, TEAM_ID};
    use serde_json::{json, Value};

    /// An issue title carrying the delimiter a success line wraps it in, wide CJK, an emoji
    /// and an em dash.
    const TITLE: &str = "修复 the retry storm 🚨 — \"backoff\" resets";
    const BODY: &str = "The client retries forever.\n\n```rust\nlet backoff = Duration::ZERO;\n```";
    const DEVELOP: &str = "afkd::develop";
    const DISCUSS: &str = "afkd::discuss";

    /// The wall clock's second now: a claim's liveness is judged against it, so a comment
    /// meant to be live is stamped near it.
    fn now_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    struct Fx {
        tracker: Arc<MockTracker>,
        units: LinearUnits,
        clock: FakeClock,
        diag: CaptureDiag,
    }

    impl Fx {
        fn poll(&self) -> Option<Unit> {
            self.units
                .try_claim_next(&self.diag, &self.clock)
                .expect("the poll reaches the tracker")
        }

        /// The error a poll fails with.
        fn poll_err(&self) -> LinearError {
            match self.units.try_claim_next(&self.diag, &self.clock) {
                Ok(_) => panic!("the poll did not fail"),
                Err(e) => e,
            }
        }

        fn wire(&self) -> WireUnit {
            let unit = self.poll().expect("a unit");
            self.units.wire_unit(&unit)
        }

        fn finish(&self, unit: &Unit, outcome: UnitOutcome) -> bool {
            self.units
                .finish(unit, outcome, &Facts::none(), &self.clock, &self.diag)
        }

        fn texts(&self, id: &str) -> Vec<String> {
            self.tracker
                .comments_on(id)
                .into_iter()
                .map(|c| c.text)
                .collect()
        }
    }

    /// A team with a state of each category, every comment the plugin posts stamped ten
    /// minutes ago and on, and a service armed as `service` over `settings` on top of the
    /// required ones.
    fn fx_as(service: &str, settings: Value) -> Fx {
        let tracker = Arc::new(MockTracker::new());
        for (state, kind) in [
            ("Todo", "unstarted"),
            ("Backlog", "backlog"),
            ("In Progress", "started"),
            ("Done", "completed"),
            ("Canceled", "canceled"),
            ("Duplicate", "duplicate"),
        ] {
            tracker.add_state(state, kind);
        }
        tracker.set_clock(now_secs() - 600);
        let mut block = json!({"team": "ENG", "api_key": "lin_api_k", "pick_from": "Todo"});
        for (k, v) in settings.as_object().unwrap() {
            block[k] = v.clone();
        }
        let cfg = issue_config(&block).expect("valid settings");
        let units = LinearUnits::new(
            Box::new(Arc::clone(&tracker)),
            &cfg,
            Identity {
                owner: "afkd-4242".into(),
                service: service.into(),
                roster: vec![DEVELOP.into(), DISCUSS.into()],
            },
        );
        Fx {
            tracker,
            units,
            clock: FakeClock::new(),
            diag: CaptureDiag::default(),
        }
    }

    fn fx(settings: Value) -> Fx {
        fx_as(DEVELOP, settings)
    }

    fn comment(id: &str, author: &str, text: &str, millis: u64) -> Comment {
        let at = UNIX_EPOCH + Duration::from_millis(millis);
        Comment {
            id: id.into(),
            text: text.into(),
            author: author.into(),
            author_name: format!("{author} (name)"),
            posted_at: at,
            renewed_at: at,
        }
    }

    fn me() -> SelfIds {
        SelfIds::viewer(SELF_ID.into())
    }

    // --- pure helpers ---

    #[test]
    fn task_text_is_the_title_or_the_title_a_blank_line_and_the_description() {
        let f = fx(json!({}));
        f.tracker.add_issue("Todo", "ENG-1", TITLE, "");
        f.tracker.add_issue("Todo", "ENG-2", TITLE, BODY);
        assert_eq!(task_text(&f.tracker.issue("ENG-1")), TITLE);
        assert_eq!(
            task_text(&f.tracker.issue("ENG-2")),
            format!("{TITLE}\n\n{BODY}")
        );
    }

    #[test]
    fn sanitize_keeps_an_identifier_and_folds_anything_else() {
        assert_eq!(sanitize("ENG-123"), "ENG-123");
        assert_eq!(sanitize("ÅÄÖ-1"), "----1");
        assert_eq!(sanitize(""), "issue");
    }

    #[test]
    fn instance_base_strips_only_an_instance_suffix() {
        assert_eq!(instance_base("afkd::develop+1"), Some("afkd::develop"));
        assert_eq!(instance_base("a+b+12"), Some("a+b"));
        for name in ["afkd::develop", "+1", "x+", "x+1a"] {
            assert_eq!(instance_base(name), None, "{name}");
        }
    }

    #[test]
    fn an_issue_key_round_trips_through_split() {
        let issue = "0f6c1d2e-3b4a-4c5d-8e9f-a0b1c2d3e4f5";
        let claim = "5e2a9c71-0d4b-4f3e-9a8c-7b6d5e4f3a21";
        let state = "1a2b3c4d-5e6f-4708-9a1b-2c3d4e5f6a7b";
        assert_eq!(
            split_issue_key(&issue_key(issue, claim, None)),
            Some((issue, claim, None))
        );
        assert_eq!(
            split_issue_key(&issue_key(issue, claim, Some(state))),
            Some((issue, claim, Some(state)))
        );
        assert_eq!(
            split_issue_key(&format!("{issue}#{claim}#")),
            Some((issue, claim, None))
        );
        assert_eq!(split_issue_key("no-hash"), None);
    }

    /// The watermark carries its boundary to the millisecond, so a comment posted half a
    /// second after the last one delivered is past it, and reads back exactly.
    #[test]
    fn the_upto_milliseconds_round_trip() {
        let at = UNIX_EPOCH + Duration::from_millis(1_793_095_080_087);
        let text = ran_text("afkd-4242", at);
        assert_eq!(text, "[afkd-ran] owner=afkd-4242 upto=1793095080087");
        assert_eq!(ran_upto(&text), Some(at));
        let comments = [
            comment("ran", SELF_ID, &text, 1_793_095_090_000),
            comment("same", "u-chen", "delivered", 1_793_095_080_087),
            comment("later", "u-chen", "half a second on", 1_793_095_080_587),
        ];
        let new: Vec<String> = feedback_comments(&comments, &me())
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(new, ["later"]);
        assert_eq!(ran_upto("[afkd-ran] owner=a upto=soon"), None);
    }

    /// A marker Linear hands back escaped reads as the marker it is: a watermark, a park
    /// marker naming its service, an attempt note, all hidden from the brief.
    #[test]
    fn an_escaped_marker_reads_as_its_marker() {
        let ran = r"\[afkd\-ran\] owner=afkd\-4242 upto=1793095080087";
        assert!(is_ran(ran));
        assert_eq!(
            ran_upto(ran),
            Some(UNIX_EPOCH + Duration::from_millis(1_793_095_080_087))
        );
        let park = r"\[afkd\-park\] service=afkd\:\:discuss";
        assert!(is_park(park));
        assert_eq!(
            park_owner(&[comment("p", SELF_ID, park, 1)]).as_deref(),
            Some("afkd::discuss")
        );
        for text in [
            ran,
            park,
            r"\[afkd-attempt\] 1/2: boom",
            r"\[afkd-claim\] owner=x",
        ] {
            assert!(is_control_marker(text), "{text}");
        }
        assert!(!is_control_marker(r"see \[afkd-ran\] above"));
    }

    #[test]
    fn an_attempt_note_is_cut_to_the_comment_bound() {
        assert_eq!(
            attempt_text(1, 2, "cargo test: 3 failed"),
            "[afkd-attempt] 1/2: cargo test: 3 failed"
        );
        let long = "看".repeat(20_000);
        let note = attempt_text(2, 2, &long);
        assert!(note.encode_utf16().count() <= COMMENT_MAX);
        assert!(note.ends_with(CUT_MARK));
        let wide = "🚨".repeat(9_000);
        let note = attempt_text(1, 1, &wide);
        assert!(
            note.encode_utf16().count() <= COMMENT_MAX,
            "a surrogate pair is never split"
        );
    }

    /// The first run sees every human comment; after a watermark, only those past it, and
    /// never afkd's own — under the viewer's id or the bot actor its claim came back as.
    #[test]
    fn feedback_is_what_humans_said_past_the_boundary() {
        let comments = [
            comment("h1", "u-phil", "Please cap it at 30s.", 1_000),
            comment("a1", SELF_ID, "Which header?", 2_000),
            comment("h2", "u-chen", "The Retry-After one.", 3_000),
            comment("b1", "bot-afkd", "Looking now.", 4_000),
        ];
        let ids = |me: &SelfIds| -> Vec<String> {
            feedback_comments(&comments, me)
                .into_iter()
                .map(|c| c.id)
                .collect()
        };
        assert_eq!(
            ids(&me()),
            ["h2", "b1"],
            "the bot's word reads as a human's to the viewer alone"
        );
        let both = me().with("bot-afkd");
        assert_eq!(
            ids(&both),
            Vec::<String>::new(),
            "its own last word bounds the delta"
        );
        let first_run = [comments[0].clone(), comments[2].clone()];
        let ids: Vec<String> = feedback_comments(&first_run, &me())
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(ids, ["h1", "h2"]);
    }

    /// The brief: the title and description, the parent and each sub-issue with its state,
    /// then the earlier conversation and the new comments, each oldest-first, afkd's own
    /// words as `afkd` and every control marker dropped.
    #[test]
    fn the_brief_lists_the_parent_the_sub_issues_and_the_thread() {
        let f = fx(json!({}));
        f.tracker.add_issue("Todo", "ENG-7", TITLE, BODY);
        f.tracker.edit_issue("ENG-7", |i| {
            i.parent = Some(Related {
                identifier: "ENG-1".into(),
                title: "Epic: 重试 that works".into(),
            });
            i.children = vec![
                Child {
                    id: "ENG-8".into(),
                    identifier: "ENG-8".into(),
                    title: "Cap the backoff".into(),
                    state: "Todo".into(),
                },
                Child {
                    id: "ENG-9".into(),
                    identifier: "ENG-9".into(),
                    title: "Read Retry-After 🚦".into(),
                    state: "Done".into(),
                },
            ];
        });
        let comments = [
            comment("h1", "u-phil", "Please cap it.", 1_000),
            comment("a1", SELF_ID, "  Which cap?  ", 2_000),
            comment("m1", SELF_ID, "[afkd-ran] owner=a upto=2000", 2_500),
            comment("h2", "u-chen", "30s.\n\n```\nmax = 30\n```", 3_000),
        ];
        assert_eq!(
            brief_text(&f.tracker.issue("ENG-7"), &comments, &me()),
            format!(
                "{TITLE}\n\n{BODY}\n\nSub-issue of ENG-1: Epic: 重试 that works\n\n## Sub-issues\n\
                 \n- ENG-8 (Todo) Cap the backoff\n- ENG-9 (Done) Read Retry-After 🚦\
                 \n\n## Earlier conversation\n\n**u-phil (name):** Please cap it.\n\n**afkd:** \
                 Which cap?\n\n\n## New comments\n\n**u-chen (name):** 30s.\n\n```\nmax = 30\n```\n"
            )
        );
        f.tracker.add_issue("Todo", "ENG-2", "Bare", "");
        assert_eq!(brief_text(&f.tracker.issue("ENG-2"), &[], &me()), "Bare");
    }

    /// What `list_comments` and `list_sub_issues` write is what the brief renders of the
    /// same issue: the thread its two sections hold, back to back, and the lines of its
    /// `## Sub-issues` — over a thread of multi-line, wide and empty comments, afkd's own
    /// words and a control marker, and sub-issues with wide titles.
    #[test]
    fn the_reads_write_what_the_brief_renders() {
        let f = fx(json!({}));
        f.tracker.add_issue("Todo", "ENG-7", TITLE, BODY);
        f.tracker.edit_issue("ENG-7", |i| {
            i.children = [
                ("ENG-8", "In Progress", "Cap the backoff at 30s"),
                (
                    "ENG-9",
                    "Done ✅",
                    "子任务: read Retry-After 🚦 — \"seconds\"",
                ),
            ]
            .iter()
            .map(|(id, state, title)| Child {
                id: format!("uuid-{id}"),
                identifier: (*id).into(),
                title: (*title).into(),
                state: (*state).into(),
            })
            .collect();
        });
        let at = |millis: u64| UNIX_EPOCH + Duration::from_millis(millis);
        for (id, author, text, millis) in [
            (
                "h1",
                "u-phil",
                "Please cap it.\n\n- at 30s\n- per header",
                1_000,
            ),
            ("a1", SELF_ID, "  Which header? 🤔  ", 2_000),
            ("m1", SELF_ID, "[afkd-ran] owner=a upto=2000", 2_500),
            (
                "h2",
                "u-chen",
                "看起来不对 🚨\n\n```\nPOST /graphql 401\n```",
                3_000,
            ),
            ("h3", "u-alvaro", "", 3_500),
        ] {
            f.tracker
                .push_comment("ENG-7", id, text, author, at(millis));
        }
        let issue = f.tracker.issue("ENG-7");
        let brief = brief_text(&issue, &f.tracker.comments_on("ENG-7"), &me());
        let (_, thread) = brief
            .split_once("## Earlier conversation\n\n")
            .expect("an earlier section");
        let (earlier, new) = thread
            .split_once("\n\n## New comments\n\n")
            .expect("a new section");
        let subs = brief
            .split_once("## Sub-issues\n\n")
            .and_then(|(_, rest)| rest.split_once("\n\n## "))
            .expect("a sub-issue section")
            .0;

        let dir = TempDir::new("parity");
        let read = |read: Read, name: &str| {
            let to = dir.path().join(name).display().to_string();
            f.units.read(&issue, &me(), read, &to, &f.diag).unwrap();
            std::fs::read_to_string(&to).unwrap()
        };
        assert_eq!(
            read(Read::Comments, "thread.md"),
            format!("{earlier}\n{new}")
        );
        assert_eq!(read(Read::SubIssues, "subs.md"), format!("{subs}\n"));
        assert!(new.ends_with("**u-alvaro:** \n"), "{new}");
        assert!(
            earlier.contains("**afkd:** Which header? 🤔\n"),
            "{earlier}"
        );
    }

    /// An issue is blocked while any `blocks` blocker is in a state not done; a completed,
    /// a canceled and a duplicate one hold nothing back.
    #[test]
    fn a_blocker_holds_an_issue_back_until_it_is_closed() {
        let f = fx(json!({}));
        f.tracker.add_issue("Todo", "ENG-1", "t", "");
        let with = |kinds: &[&str]| {
            let mut issue = f.tracker.issue("ENG-1");
            issue.blockers = kinds
                .iter()
                .map(|k| Blocker {
                    identifier: "ENG-0".into(),
                    state_kind: k.to_string(),
                })
                .collect();
            blocked(&issue)
        };
        assert!(!with(&[]));
        for open in ["triage", "backlog", "unstarted", "started"] {
            assert!(with(&[open]), "{open}");
            assert!(with(&["completed", open]), "{open} beside a done one");
        }
        assert!(!with(&["completed", "canceled", "duplicate"]));
    }

    // --- the scan and the claim ---

    /// The won race hands over the issue as a unit: the identifier as its id and its key's
    /// uuid and claim, the uuid as its thread, every handle field, the viewer as `self`, the
    /// five env names, and the brief.
    #[test]
    fn a_won_claim_hands_over_the_issue_as_a_unit() {
        let f = fx(json!({}));
        f.tracker.add_issue("Todo", "ENG-7", TITLE, BODY);
        f.tracker.seed_label("ENG-7", "afkd/ready");
        f.tracker.seed_label("ENG-7", "Väntar på svar");
        f.tracker.edit_issue("ENG-7", |i| {
            i.blockers = vec![Blocker {
                identifier: "ENG-3".into(),
                state_kind: "completed".into(),
            }];
            i.children = vec![Child {
                id: "ENG-8".into(),
                identifier: "ENG-8".into(),
                title: "c".into(),
                state: "Todo".into(),
            }];
        });
        f.tracker
            .seed_comment_by("ENG-7", "h1", "看起来不对 🚨", "u-chen", now_secs() - 3600);
        let wire = f.wire();
        let claim = &f.tracker.comments_on("ENG-7")[1];
        assert_eq!(claim.text, "[afkd-claim] owner=afkd-4242");
        assert_eq!(wire.id, "ENG-7");
        assert_eq!(wire.key, format!("ENG-7#{}", claim.id));
        assert_eq!(wire.thread, "ENG-7");
        assert_eq!(wire.me, SELF_ID);
        assert_eq!(wire.seen, ["h1".to_string(), claim.id.clone()]);
        assert_eq!(
            wire.fields,
            IssueFields {
                title: TITLE.into(),
                url: "https://linear.app/acme/issue/ENG-7".into(),
                labels: vec!["afkd/ready".into(), "Väntar på svar".into()],
                state: "Todo".into(),
                parent: String::new(),
                sub_issues: vec!["ENG-8".into()],
                blocked_by: vec!["ENG-3".into()],
            }
        );
        assert_eq!(
            wire.env,
            BTreeMap::from([
                (ENV_API_URL.into(), "https://api.linear.app/graphql".into()),
                (ENV_AUTHORIZATION.into(), "lin_api_mock".into()),
                (ENV_TEAM_ID.into(), TEAM_ID.into()),
                (ENV_ISSUE_ID.into(), "ENG-7".into()),
                (ENV_ISSUE.into(), "ENG-7".into()),
            ])
        );
        assert_eq!(wire.files[0].path, TASK_FILE);
        assert!(wire.files[0]
            .text
            .ends_with("## New comments\n\n**u-chen:** 看起来不对 🚨\n"));
        assert_eq!(
            f.diag.narrated(),
            [format!("[linear] claimed issue ENG-7 \"{TITLE}\"")]
        );
        assert!(f.diag.errs().is_empty(), "{:?}", f.diag.errs());
    }

    /// `require_member` holds an issue to the user it names, as its assignee or its
    /// delegate — `me` the viewer, which an app the issue is delegated to is — and passes
    /// over the rest; an unknown name fails the poll.
    #[test]
    fn require_member_matches_the_assignee_or_the_delegate() {
        let f = fx(json!({"require_member": ["chen@acme.example"]}));
        f.tracker.seed_user("chen@acme.example", "u-chen");
        f.tracker.add_issue("Todo", "ENG-1", "nobody's", "");
        f.tracker.add_issue("Todo", "ENG-2", "Phil's", "");
        f.tracker
            .edit_issue("ENG-2", |i| i.assignee = Some("u-phil".into()));
        f.tracker.add_issue("Todo", "ENG-3", "Chen's", "");
        f.tracker
            .edit_issue("ENG-3", |i| i.assignee = Some("u-chen".into()));
        assert_eq!(f.wire().id, "ENG-3");

        let f = fx(json!({"require_member": ["me"]}));
        f.tracker
            .add_issue("Todo", "ENG-1", "assigned to a human", "");
        f.tracker
            .edit_issue("ENG-1", |i| i.assignee = Some("u-phil".into()));
        f.tracker
            .add_issue("Todo", "ENG-2", "delegated to the app", "");
        f.tracker.edit_issue("ENG-2", |i| {
            i.assignee = Some("u-phil".into());
            i.delegate = Some(SELF_ID.into());
        });
        assert_eq!(f.wire().id, "ENG-2");

        let f = fx(json!({"require_member": ["ghost@acme.example"]}));
        f.tracker.add_issue("Todo", "ENG-1", "t", "");
        let e = f.poll_err();
        assert_eq!(
            e.to_string(),
            "linear resolve user: no user whose display name or email is 'ghost@acme.example'"
        );
        assert!(f.tracker.comments_on("ENG-1").is_empty(), "nothing posted");
    }

    /// `require_member "me", "none"` claims what nobody holds and what is the app's own, and
    /// passes over an issue a person or another app holds without a word on it.
    #[test]
    fn require_member_none_or_me_claims_what_nobody_holds_and_what_is_mine() {
        let f = fx(json!({"require_member": [["me", "none"]]}));
        f.tracker.seed_user("vesper@oauthapp.example", "u-vesper");
        f.tracker.add_issue("Todo", "ENG-1", "Phil's", "");
        f.tracker
            .edit_issue("ENG-1", |i| i.assignee = Some("u-phil".into()));
        f.tracker
            .add_issue("Todo", "ENG-2", "delegated to vesper 🌑", "");
        f.tracker
            .edit_issue("ENG-2", |i| i.delegate = Some("u-vesper".into()));
        f.tracker.add_issue("Todo", "ENG-3", "nobody's", "");
        f.tracker
            .add_issue("Todo", "ENG-4", "delegated to the app", "");
        f.tracker
            .edit_issue("ENG-4", |i| i.delegate = Some(SELF_ID.into()));
        assert_eq!(f.wire().id, "ENG-3");
        assert_eq!(f.wire().id, "ENG-4");
        assert!(f.poll().is_none(), "nothing else passes");
        for held in ["ENG-1", "ENG-2"] {
            assert!(f.tracker.comments_on(held).is_empty(), "{held} untouched");
        }

        // An unknown entry fails the poll even beside `me` and `none`.
        let f = fx(json!({"require_member": [["me", "none", "ghost@acme.example"]]}));
        f.tracker.add_issue("Todo", "ENG-1", "t", "");
        assert_eq!(
            f.poll_err().to_string(),
            "linear resolve user: no user whose display name or email is 'ghost@acme.example'"
        );
        assert!(f.tracker.comments_on("ENG-1").is_empty(), "nothing posted");
    }

    /// `require_label` claims only a labelled issue, `without_label` none carrying any of
    /// its labels, and deny wins over require.
    #[test]
    fn the_label_gates_hold_and_deny_wins() {
        let f = fx(json!({"require_label": "afkd/ready", "without_label": [["Hold", "WIP 🚧"]]}));
        f.tracker.add_issue("Todo", "ENG-1", "unlabelled", "");
        f.tracker.add_issue("Todo", "ENG-2", "ready but held", "");
        f.tracker.seed_label("ENG-2", "afkd/ready");
        f.tracker.seed_label("ENG-2", "WIP 🚧");
        f.tracker.add_issue("Todo", "ENG-3", "ready", "");
        f.tracker.seed_label("ENG-3", "afkd/ready");
        assert_eq!(f.wire().id, "ENG-3");
        for id in ["ENG-1", "ENG-2"] {
            assert!(f.tracker.comments_on(id).is_empty(), "{id} was written to");
        }
    }

    /// A blocked issue is passed over in silence and the next claimed; once its blocker is
    /// done — completed, or canceled — it is claimed in its turn.
    #[test]
    fn a_blocked_issue_waits_until_its_blocker_is_done() {
        for done in ["completed", "canceled"] {
            let f = fx(json!({}));
            f.tracker.add_issue("Todo", "ENG-1", "blocked", "");
            f.tracker.edit_issue("ENG-1", |i| {
                i.blockers = vec![Blocker {
                    identifier: "ENG-0".into(),
                    state_kind: "started".into(),
                }]
            });
            f.tracker.add_issue("Todo", "ENG-2", "free", "");
            assert_eq!(f.wire().id, "ENG-2");
            assert!(f.tracker.comments_on("ENG-1").is_empty());
            assert!(f.poll().is_none(), "ENG-2 is held, ENG-1 still blocked");
            f.tracker
                .edit_issue("ENG-1", |i| i.blockers[0].state_kind = done.into());
            assert_eq!(f.wire().id, "ENG-1", "{done}");
        }
    }

    /// An issue someone holds a live claim on is stepped over without a write; a claim past
    /// its lifetime holds nothing.
    #[test]
    fn a_live_claim_is_stepped_over_and_a_stale_one_is_not() {
        let f = fx(json!({}));
        f.tracker.add_issue("Todo", "ENG-1", "held", "");
        f.tracker.add_issue("Todo", "ENG-2", "stale", "");
        f.tracker.seed_comment_by(
            "ENG-1",
            "r1",
            r"\[afkd-claim\] owner=afkd\-17",
            "u-rival",
            now_secs() - 60,
        );
        f.tracker.seed_comment_by(
            "ENG-2",
            "r2",
            "[afkd-claim] owner=afkd-17",
            "u-rival",
            now_secs() - 2 * 3600,
        );
        assert_eq!(f.wire().id, "ENG-2");
        assert_eq!(
            f.texts("ENG-1"),
            [r"\[afkd-claim\] owner=afkd\-17"],
            "untouched"
        );
    }

    /// A rival's earlier live claim wins the race past the pre-scan: ours is deleted and the
    /// round lost.
    #[test]
    fn a_lost_race_takes_its_claim_back() {
        let f = fx(json!({}));
        f.tracker.add_issue("Todo", "ENG-1", "t", "");
        f.tracker.seed_comment_by(
            "ENG-1",
            "r1",
            "[afkd-claim] owner=afkd-17",
            "u-rival",
            now_secs() - 900,
        );
        let round = f
            .units
            .attempt_claim(
                TEAM_ID,
                f.tracker.issue("ENG-1"),
                None,
                &me(),
                &f.diag,
                &f.clock,
            )
            .unwrap();
        assert!(matches!(round, ClaimRound::Lost));
        assert_eq!(f.texts("ENG-1"), ["[afkd-claim] owner=afkd-17"]);
        assert_eq!(
            f.clock.sleeps(),
            [CLAIM_SETTLE],
            "it settled before it re-read"
        );
    }

    /// The scan reads a state page by page, and with two `pick_from` states moves to the
    /// second past every issue the first holds back; the key names the second state.
    #[test]
    fn pick_from_pages_its_states_in_order() {
        let f = fx(json!({"pick_from": [["Todo", "Backlog"]], "without_label": "Hold"}));
        f.tracker.set_page(2);
        for n in 1..=5 {
            f.tracker.add_issue("Todo", &format!("ENG-{n}"), "held", "");
            f.tracker.seed_label(&format!("ENG-{n}"), "Hold");
        }
        f.tracker
            .add_issue("Backlog", "ENG-9", "Bakgrund: stadsvy i skymning 🌆", "");
        let wire = f.wire();
        assert_eq!(wire.id, "ENG-9");
        assert!(wire.key.ends_with("#Backlog"), "{}", wire.key);
        let reads = f
            .tracker
            .calls()
            .iter()
            .filter(|c| **c == "read issues")
            .count();
        assert_eq!(reads, 4, "three pages of Todo, one of Backlog");
    }

    /// A `pick_from` naming a state the team lacks fails the poll naming it, before any
    /// state is read.
    #[test]
    fn a_state_the_team_lacks_fails_the_poll_naming_it() {
        let f = fx(json!({"pick_from": [["Todo", "Väntar ⏳"]]}));
        f.tracker.add_issue("Todo", "ENG-1", "t", "");
        let e = f.poll_err();
        assert_eq!(
            e.to_string(),
            "linear resolve state: no workflow state named 'Väntar ⏳' in team ENG"
        );
        assert!(!f.tracker.calls().contains(&"read issues"));
    }

    /// The scan stops at its budget, says so once, and claims nothing.
    #[test]
    fn the_scan_stops_at_its_budget() {
        let mut f = fx(json!({"without_label": "Hold"}));
        f.clock = FakeClock::ticking(Duration::from_secs(4));
        for n in 1..=10 {
            f.tracker.add_issue("Todo", &format!("ENG-{n}"), "held", "");
            f.tracker.seed_label(&format!("ENG-{n}"), "Hold");
        }
        f.tracker.add_issue("Todo", "ENG-11", "free", "");
        assert!(f.poll().is_none());
        assert_eq!(
            f.diag.errs(),
            [
                "linear poll: the scan ran past its 20s budget; the rest of it waits for the next \
              poll"
            ]
        );
        assert!(f.tracker.comments_on("ENG-11").is_empty());
    }

    // --- discuss_with ---

    /// `anyone`: an issue fires on first sight; once afkd has spoken it waits; a human's
    /// reply re-arms it; afkd's own reply — under its bot actor too — never does.
    #[test]
    fn discuss_with_anyone_waits_for_a_reply_after_afkd_speaks() {
        let f = fx_as(DISCUSS, json!({"discuss_with": "anyone"}));
        f.tracker
            .add_issue("Todo", "ENG-1", "Should we cap it?", "");
        let unit = f.poll().expect("first sight fires");
        f.tracker.push_comment(
            "ENG-1",
            "a1",
            "Yes — at 30s.",
            SELF_ID,
            SystemTime::now() - Duration::from_secs(5),
        );
        assert!(f.finish(&unit, UnitOutcome::Clean));
        assert!(f.poll().is_none(), "afkd spoke last");
        f.tracker.push_comment(
            "ENG-1",
            "b1",
            "And a bot note.",
            "bot-afkd",
            SystemTime::now() - Duration::from_secs(4),
        );
        assert!(
            f.poll().is_some(),
            "a word under an id the viewer is not reads as a human's"
        );

        let f = fx_as(DISCUSS, json!({"discuss_with": "anyone"}));
        f.tracker
            .add_issue("Todo", "ENG-1", "Should we cap it?", "");
        let unit = f.poll().unwrap();
        f.tracker.push_comment(
            "ENG-1",
            "a1",
            "Yes.",
            SELF_ID,
            SystemTime::now() - Duration::from_secs(5),
        );
        f.finish(&unit, UnitOutcome::Clean);
        f.tracker.push_comment(
            "ENG-1",
            "h1",
            "Why 30s?",
            "u-chen",
            SystemTime::now() - Duration::from_secs(3),
        );
        assert!(f.poll().is_some(), "a reply re-arms it");
    }

    /// A named list is armed only by the users it names, and never by afkd, however it is
    /// named.
    #[test]
    fn discuss_with_a_user_is_armed_only_by_that_user() {
        let f = fx_as(
            DISCUSS,
            json!({"discuss_with": [["chen@acme.example", "me"]]}),
        );
        f.tracker.seed_user("chen@acme.example", "u-chen");
        f.tracker.add_issue("Todo", "ENG-1", "t", "");
        let at = |ago: u64| SystemTime::now() - Duration::from_secs(ago);
        f.tracker
            .push_comment("ENG-1", "a1", "afkd asked", SELF_ID, at(60));
        f.tracker
            .push_comment("ENG-1", "h1", "Phil answered", "u-phil", at(50));
        assert!(f.poll().is_none(), "Phil is not on the list");
        f.tracker
            .push_comment("ENG-1", "a2", "afkd again", SELF_ID, at(40));
        assert!(f.poll().is_none(), "nor is afkd, named `me`");
        f.tracker
            .push_comment("ENG-1", "h2", "Chen answered", "u-chen", at(30));
        assert!(f.poll().is_some());
    }

    /// A silent discuss turn is backstopped, so afkd speaks last; one that spoke is not.
    #[test]
    fn a_silent_discuss_turn_is_backstopped() {
        let f = fx_as(DISCUSS, json!({"discuss_with": "anyone"}));
        f.tracker.add_issue("Todo", "ENG-1", "t", "");
        let unit = f.poll().unwrap();
        assert!(f.finish(&unit, UnitOutcome::Clean));
        assert_eq!(f.texts("ENG-1"), ["reviewed, nothing to add"]);
        assert!(
            f.texts("ENG-1").iter().all(|t| !is_ran(t)),
            "no watermark on this path"
        );
    }

    // --- finish, release, renew ---

    /// A clean finish posts one watermark carrying the claim read's boundary in
    /// milliseconds, prunes the prior one, and releases the claim last.
    #[test]
    fn a_clean_finish_leaves_one_watermark_and_releases() {
        let f = fx(json!({}));
        f.tracker.add_issue("Todo", "ENG-1", TITLE, "");
        let asked = SystemTime::now() - Duration::from_secs(3600);
        f.tracker.push_comment(
            "ENG-1",
            "ran0",
            "[afkd-ran] owner=afkd-17 upto=1000",
            SELF_ID,
            asked,
        );
        f.tracker.push_comment(
            "ENG-1",
            "h1",
            "Cap it.",
            "u-chen",
            asked + Duration::from_millis(87),
        );
        let unit = f.poll().unwrap();
        assert!(f.finish(&unit, UnitOutcome::Clean));
        let millis = (asked + Duration::from_millis(87))
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();
        assert_eq!(
            f.texts("ENG-1"),
            [
                "Cap it.".to_string(),
                format!("[afkd-ran] owner=afkd-4242 upto={millis}")
            ]
        );
        assert_eq!(
            f.diag.narrated().last().unwrap(),
            &format!("[linear] released claim on issue ENG-1 \"{TITLE}\"")
        );
    }

    /// A finish whose release does not land is held, and the release replays it; once the
    /// claim is gone the key is free. A finish owed past the retry window reverses the
    /// issue to the bottom of its state.
    #[test]
    fn an_undelivered_finish_is_held_and_release_delivers_it() {
        let f = fx(json!({}));
        f.tracker.add_issue("Todo", "ENG-1", "t", "");
        let unit = f.poll().unwrap();
        let key = unit.key();
        let down = LinearError::Transport {
            stage: "delete comment",
            reason: "mock failure".into(),
        };
        f.tracker.fail("delete comment", down.clone());
        assert!(!f.finish(&unit, UnitOutcome::Clean));
        assert_eq!(
            f.units.release_stale(&key, f.clock.now(), &f.diag),
            Some(false),
            "still owed"
        );
        f.tracker.clear_failure();
        assert_eq!(
            f.units.release_stale(&key, f.clock.now(), &f.diag),
            Some(true)
        );
        assert!(f.texts("ENG-1").iter().all(|t| !is_claim(t)));
        assert!(
            !f.tracker
                .actions()
                .iter()
                .any(|a| matches!(a, Action::Move { .. })),
            "a delivered finish moves nothing"
        );

        let f = fx(json!({}));
        f.tracker.add_issue("Todo", "ENG-1", "t", "");
        f.tracker.add_issue("Todo", "ENG-2", "queued", "");
        let unit = f.poll().unwrap();
        f.tracker.fail("delete comment", down);
        f.finish(&unit, UnitOutcome::Clean);
        f.clock.advance(RETRY_FOR);
        assert_eq!(
            f.units.release_stale(&unit.key(), f.clock.now(), &f.diag),
            Some(true)
        );
        assert!(
            f.diag.errs().contains(
                &"giving up on the finish for issue ENG-1 \"t\" after 1h: \
             the claim release still owed — linear delete comment: no response (mock failure); \
             returning it to \"Todo\""
                    .to_string()
            ),
            "{:?}",
            f.diag.errs()
        );
        assert_eq!(
            f.tracker.in_state("Todo"),
            ["ENG-2", "ENG-1"],
            "to the bottom"
        );
    }

    /// A crash victim's claim is pruned and its issue put back at the bottom of the state it
    /// came from — the key's own, else the first `pick_from` state; a key with no `#` names
    /// nothing.
    #[test]
    fn release_reverses_a_crashed_claim_to_the_bottom_of_its_state() {
        let f = fx(json!({"pick_from": [["Todo", "Backlog"]]}));
        f.tracker.add_issue("In Progress", "ENG-1", "crashed", "");
        f.tracker.add_issue("Todo", "ENG-2", "waiting", "");
        f.tracker.add_issue("Backlog", "ENG-3", "waiting too", "");
        f.tracker.seed_comment(
            "ENG-1",
            "claim1",
            "[afkd-claim] owner=afkd-17",
            now_secs() - 60,
        );
        assert_eq!(
            f.units
                .release_stale("ENG-1#claim1", f.clock.now(), &f.diag),
            Some(true)
        );
        assert_eq!(f.tracker.in_state("Todo"), ["ENG-2", "ENG-1"]);
        assert!(f.tracker.comments_on("ENG-1").is_empty());
        f.tracker.seed_comment(
            "ENG-1",
            "claim2",
            "[afkd-claim] owner=afkd-17",
            now_secs() - 60,
        );
        assert_eq!(
            f.units
                .release_stale("ENG-1#claim2#Backlog", f.clock.now(), &f.diag),
            Some(true)
        );
        assert_eq!(f.tracker.in_state("Backlog"), ["ENG-3", "ENG-1"]);
        assert_eq!(
            f.units.release_stale("garbage", f.clock.now(), &f.diag),
            None
        );
    }

    #[test]
    fn renewing_edits_the_claim_in_place() {
        let f = fx(json!({}));
        f.tracker.add_issue("Todo", "ENG-1", "t", "");
        let unit = f.poll().unwrap();
        f.units.renew(&unit, 3, &f.diag);
        assert_eq!(
            f.tracker.actions(),
            [Action::EditComment {
                comment: unit.claim_id.clone(),
                text: "[afkd-claim] owner=afkd-4242 renewal=3".into()
            }]
        );
    }

    /// A faulted attempt is noted on the issue; one Linear cannot take now is queued, and
    /// one with no sentence notes nothing.
    #[test]
    fn a_faulted_attempt_is_noted_or_queued() {
        let f = fx(json!({}));
        f.tracker.add_issue("Todo", "ENG-1", "t", "");
        let unit = f.poll().unwrap();
        f.units
            .attempt_failed(&unit, 1, 2, Some("cargo test: 3 failed"), &f.clock, &f.diag);
        f.units.attempt_failed(&unit, 2, 2, None, &f.clock, &f.diag);
        assert_eq!(
            f.texts("ENG-1")[1..],
            ["[afkd-attempt] 1/2: cargo test: 3 failed"]
        );
        f.tracker.fail(
            "post comment",
            LinearError::Refused {
                stage: "post comment",
                status: 429,
                message: "Rate limit exceeded".into(),
                kind: "ratelimited".into(),
                retry_after: Some(Duration::from_secs(7)),
            },
        );
        f.units
            .attempt_failed(&unit, 2, 2, Some("again"), &f.clock, &f.diag);
        assert!(f.units.outbox().owes("ENG-1"));
        assert_eq!(
            f.diag.errs(),
            [
                "linear could not take a write for issue ENG-1 \"t\": linear post comment: Linear \
              refused it (429): Rate limit exceeded; the comment \"[afkd-attempt] 2/2: again\" \
              is queued and retried for up to 1h"
            ]
        );
    }

    /// A write Linear rate limits is first retried no sooner than its `Retry-After` after its
    /// inline try failed, not after the call that asked for it began: on a clock that moves on
    /// one slow round trip per read, the wait starts from the read after the try.
    #[test]
    fn a_rate_limited_write_waits_its_retry_after_from_the_failure() {
        const TRY: Duration = Duration::from_secs(2);
        const RETRY_AFTER: Duration = Duration::from_secs(7);
        let mut f = fx(json!({}));
        f.tracker.add_issue("Todo", "ENG-1", TITLE, "");
        let unit = f.poll().unwrap();
        f.clock = FakeClock::ticking(TRY);
        f.tracker.fail(
            "post comment",
            LinearError::Refused {
                stage: "post comment",
                status: 429,
                message: "Rate limit exceeded".into(),
                kind: "ratelimited".into(),
                retry_after: Some(RETRY_AFTER),
            },
        );
        // Read the call's instant where `Plugin::answer` reads it, before the try goes out.
        let asked = f.clock.now();
        assert_eq!(
            f.units.deliver(
                &unit.issue,
                Write::Note("[afkd-attempt] 1/2: cargo test: 3 failed\n修复 🚨".into()),
                &f.clock,
                &f.diag,
            ),
            Ok(Delivery::Queued)
        );
        let due = f
            .units
            .outbox()
            .deliver_due(&*f.tracker, &f.clock, &f.diag)
            .expect("the note is still owed");
        assert!(
            due >= asked + TRY + RETRY_AFTER,
            "the retry is due {:?} after the call began: its {RETRY_AFTER:?} wait counts from \
             the call, not from the try that failed {TRY:?} into it",
            due - asked
        );
        assert_eq!(
            f.tracker.comments_on("ENG-1").len(),
            1,
            "only the claim landed"
        );
    }

    #[test]
    fn classify_parks_on_the_marker_and_otherwise_keeps_afkds_verdict() {
        let scratch = TempDir::new("classify");
        assert_eq!(
            LinearUnits::classify(scratch.path(), UnitOutcome::Clean),
            UnitOutcome::Clean
        );
        std::fs::write(scratch.path().join(PARK_FILE), b"").unwrap();
        assert_eq!(
            LinearUnits::classify(scratch.path(), UnitOutcome::Failed),
            UnitOutcome::Park
        );
    }

    // --- the park ---

    /// Run one issue to a park: claimed, the question asked, finished parked.
    fn park(f: &Fx, id: &str) -> Unit {
        let unit = f.poll().expect("a unit");
        assert_eq!(unit.identifier(), id);
        f.tracker.push_comment(
            id,
            "q1",
            "Ska videon visa appen eller webben? 📱",
            SELF_ID,
            SystemTime::now() - Duration::from_secs(120),
        );
        assert!(f.finish(&unit, UnitOutcome::Park));
        unit
    }

    /// A park badges the issue, posts the owner marker, and releases the claim, in that
    /// order; a reply re-arms it ahead of `pick_from`, the badge and marker coming off.
    #[test]
    fn a_parked_issue_answered_is_reclaimed_and_unbadged() {
        let f = fx(json!({}));
        f.tracker.add_issue("Todo", "ENG-1", TITLE, "");
        park(&f, "ENG-1");
        assert_eq!(f.tracker.issue("ENG-1").labels, [NEEDS_INFO_LABEL]);
        assert!(f
            .texts("ENG-1")
            .contains(&"[afkd-park] service=afkd::develop".to_string()));
        assert!(f.texts("ENG-1").iter().all(|t| !is_claim(t)));
        assert!(f.diag.narrated().contains(&format!(
            "[linear] parked issue ENG-1 \"{TITLE}\" awaiting a reply (label \"Needs Info\")"
        )));
        f.tracker.add_issue("Todo", "ENG-2", "queued", "");
        assert_eq!(
            f.poll().unwrap().identifier(),
            "ENG-2",
            "unanswered, so the scan goes on"
        );

        f.tracker.push_comment(
            "ENG-1",
            "h1",
            "Appen — webben får en egen video.",
            "u-phil",
            SystemTime::now() - Duration::from_secs(30),
        );
        let unit = f.poll().expect("the answer re-arms it");
        assert_eq!(unit.identifier(), "ENG-1");
        assert!(f.tracker.issue("ENG-1").labels.is_empty(), "unbadged");
        assert!(
            f.texts("ENG-1").iter().all(|t| !is_park(t)),
            "the marker is gone"
        );
        assert!(f.units.wire_unit(&unit).files[0]
            .text
            .ends_with("## New comments\n\n**u-phil:** Appen — webben får en egen video.\n"));
    }

    /// Another live service's parked issue is swept past in silence, and one naming a
    /// service the config no longer runs is taken over with one line.
    #[test]
    fn a_parked_issue_is_resumed_only_by_its_parker_or_as_an_orphan() {
        let parked = |service: &str| {
            let f = fx(json!({}));
            f.tracker.add_issue("In Progress", "ENG-1", "t", "");
            f.tracker.seed_label("ENG-1", NEEDS_INFO_LABEL);
            let at = |ago: u64| SystemTime::now() - Duration::from_secs(ago);
            f.tracker
                .push_comment("ENG-1", "q", "Which one?", SELF_ID, at(120));
            f.tracker.push_comment(
                "ENG-1",
                "p",
                &format!("[afkd-park] service={service}"),
                SELF_ID,
                at(110),
            );
            f.tracker
                .push_comment("ENG-1", "h", "This one.", "u-phil", at(60));
            f
        };
        let f = parked(DISCUSS);
        assert!(f.poll().is_none(), "the discusser's");
        assert!(f
            .tracker
            .comments_on("ENG-1")
            .iter()
            .all(|c| !is_claim(&c.text)));

        let f = parked("afkd::gone+2");
        assert_eq!(f.poll().unwrap().identifier(), "ENG-1");
        assert!(f.diag.narrated().contains(
            &"[linear] resuming issue ENG-1 \"t\", parked by \
             service \"afkd::gone\" which this config no longer runs"
                .to_string()
        ));
    }

    /// A blocker added while an issue was parked holds it back as one at the first claim
    /// would, and a denied label is a live veto.
    #[test]
    fn a_parked_issue_is_held_by_a_new_blocker_or_a_denied_label() {
        let f = fx(json!({"without_label": "Hold"}));
        f.tracker.add_issue("Todo", "ENG-1", "t", "");
        park(&f, "ENG-1");
        f.tracker.push_comment(
            "ENG-1",
            "h1",
            "Go ahead.",
            "u-phil",
            SystemTime::now() - Duration::from_secs(30),
        );
        f.tracker.edit_issue("ENG-1", |i| {
            i.blockers = vec![Blocker {
                identifier: "ENG-0".into(),
                state_kind: "started".into(),
            }]
        });
        assert!(f.poll().is_none(), "blocked");
        f.tracker.edit_issue("ENG-1", |i| i.blockers.clear());
        f.tracker.seed_label("ENG-1", "Hold");
        assert!(f.poll().is_none(), "held");
        f.tracker
            .edit_issue("ENG-1", |i| i.labels.retain(|l| l != "Hold"));
        assert!(f.poll().is_some());
    }

    /// An app whose comments come back under its bot actor reads its own question as its
    /// own: the parked issue waits for a human, and the claim's author joins the self set.
    #[test]
    fn an_app_whose_comments_carry_its_bot_actor_reads_its_own_words_as_its_own() {
        let f = fx(json!({}));
        f.tracker.post_as("bot-afkd");
        f.tracker.add_issue("Todo", "ENG-1", "t", "");
        let unit = f.poll().unwrap();
        assert_eq!(unit.me(), &me().with("bot-afkd"));
        f.tracker.push_comment(
            "ENG-1",
            "q1",
            "Which header?",
            "bot-afkd",
            SystemTime::now() - Duration::from_secs(120),
        );
        assert!(f.finish(&unit, UnitOutcome::Park));
        assert!(f.poll().is_none(), "its own question is no answer");
        f.tracker.push_comment(
            "ENG-1",
            "h1",
            "Retry-After.",
            "u-chen",
            SystemTime::now() - Duration::from_secs(60),
        );
        let unit = f.poll().unwrap();
        let brief = f.units.wire_unit(&unit).files[0].text.clone();
        assert!(
            brief.ends_with(
                "## Earlier conversation\n\n**afkd:** Which header?\n\n\n## New \
                             comments\n\n**u-chen:** Retry-After.\n"
            ),
            "{brief}"
        );
    }

    /// The badge that cannot land holds the claim; the release replays it and only then
    /// lets the claim go.
    #[test]
    fn a_park_whose_badge_cannot_land_is_held() {
        let f = fx(json!({}));
        f.tracker.add_issue("Todo", "ENG-1", "t", "");
        let unit = f.poll().unwrap();
        f.tracker.fail(
            "add label",
            LinearError::Status {
                stage: "add label",
                status: 503,
                retry_after: None,
            },
        );
        assert!(!f.finish(&unit, UnitOutcome::Park));
        assert!(
            f.texts("ENG-1").iter().any(|t| is_claim(t)),
            "the claim is held"
        );
        f.tracker.clear_failure();
        assert_eq!(
            f.units.release_stale(&unit.key(), f.clock.now(), &f.diag),
            Some(true)
        );
        assert_eq!(f.tracker.issue("ENG-1").labels, [NEEDS_INFO_LABEL]);
        assert!(f.texts("ENG-1").iter().all(|t| !is_claim(t)));
    }
}
