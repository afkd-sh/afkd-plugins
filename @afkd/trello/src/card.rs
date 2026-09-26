//! The `trello` kind's **vendor half**: the Trello-specific side of afkd's unit spine,
//! ported from the built-in `TrelloCardUnits` (afkd's `crates/trello/src/trigger.rs`)
//! and re-seated on the plugin wire.
//!
//! afkd keeps the spine — the cadence, the queue lane, the claim journal, the attempt
//! loop, the framing of `task.md`, the mid-run watch. What lives here is what only a board
//! can answer: the comment-lock claim, the eligibility gates, the parked-card sweep and
//! its ownership rule, the brief, the attempt markers, the three-moment terminal
//! lifecycle, and the owed-lifecycle retry a `held` finish leaves for `release`. It
//! reaches the board through the mockable [`BoardClient`] seam, so that logic is
//! unit-tested with no network against the in-memory `MockBoard`.
//!
//! Work is only ever *claimed from the source list*, or re-claimed by the parked sweep
//! from wherever a badged card sits; a card already in the in-progress list is never
//! picked up.
//!
//! Every text that crosses the board — the claim, watermark, attempt and park comments,
//! the backstop, the brief — and every narrated line is the built-in's, byte for byte, so
//! a card the built-in claimed, marked or parked reads identically here, and the other
//! way round.
//!
//! The wire changes three things. afkd's stop is invisible here, so a claim never
//! abandons mid-settle: a unit polled during a stop is handed straight back with
//! `release`, which takes the ordinary reversal. The poll runs under a scan budget, since
//! afkd ends a call at 60 seconds. And the lines go to [`Diag`], whose running form is
//! stderr.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::board::{BoardClient, BoardError, Card, Checklist, Comment};
use crate::claim::{
    claim_renewal_text, claim_text, is_claim, won_claim, ClaimMarker, CLAIM_LIFETIME, CLAIM_SETTLE,
};
use crate::common::{
    lock, Clock, Diag, ScanBudget, ENV_API_KEY, ENV_BOARD_ID, ENV_CARD_ID, ENV_TOKEN,
};
use crate::lifecycle::{LifecycleAction, ListPosition};
use crate::run_ref;
use crate::settings::{BoardConfig, DiscussWith, MemberRef};
use crate::wire::{Facts, UnitOutcome, WireFile, WireUnit};
use crate::{PARK_FILE, TASK_FILE};

/// Marker prefix on a failed-attempt comment.
const ATTEMPT_MARKER: &str = "[afkd-attempt]";

/// How many beats an owed terminal lifecycle is retried before the card is handed back
/// to `pick_from` for a human.
const PENDING_FINISH_TRIES: u32 = 5;

/// The bound on remembered undelivered lifecycles. Empty in the steady state — every
/// `release` drains its entry — so this only bites where afkd never asks.
const PENDING_MAX: usize = 64;

/// Marker prefix on the durable high-water-mark comment posted at run end.
const RAN_MARKER: &str = "[afkd-ran]";

/// Marker prefix on the **park owner** comment: posted when a run parks a card, naming
/// the configured service that parked it, so [`scan_parked`](TrelloUnits::scan_parked)
/// hands the card back to that service alone. Present only while the card is parked —
/// the re-claim deletes it ([`attempt_claim`](TrelloUnits::attempt_claim)).
///
/// A comment of its own rather than a field on the [`RAN_MARKER`] watermark: the
/// `discuss_with` grooming path posts no watermark at all, and a discuss service parks
/// just as a dev service does.
const PARK_MARKER: &str = "[afkd-park]";

/// The label the kind adds when a run **parks** a card awaiting a human reply, and takes
/// off again on the next claim. Spelled in title case because a board label is furniture
/// a person reads next to `Problem`.
///
/// It also does the finding: Trello claims strictly from `pick_from`, so a parked card
/// sitting anywhere else is somewhere nothing looks, and the badge rides the card record,
/// which makes "which cards are parked?" one small board read a beat.
const AWAITING_LABEL: &str = "Awaiting Reply";

/// How many badged cards one beat reads in full, from a rotating cursor. A bound, not a
/// limit on how many cards may be parked: the cursor still reaches every one of them
/// within a few beats.
const PARK_SCAN_MAX: usize = 16;

/// The tag every success line wears, naming the vendor family.
const ACTION_TAG: &str = "[trello]";

/// One card taken on as a unit of work.
pub(crate) struct Unit {
    card: Card,
    /// Our claim comment id, to delete on lease release.
    claim_id: String,
    /// The full claim-read comment set (the read used to judge the claim). The brief
    /// renders the whole thread from this; the new/prior split and the delivery boundary
    /// are derived from it.
    comments: Vec<Comment>,
    /// The delivery boundary stamped onto this run's `[afkd-ran]` watermark: the newest
    /// comment time this run has accounted for. Fixed at the claim read, so a comment
    /// that arrives mid-run falls strictly after it and reaches the next run.
    delivered_upto: SystemTime,
    /// afkd's own member id, taken from the posted claim's author: the self identity the
    /// brief and the feedback delta key on, and the backstop's last-speaker diff matches.
    self_author: String,
}

impl Unit {
    /// The card's short link — the unit's session thread, and the handle a log line
    /// names it by.
    pub(crate) fn thread(&self) -> &str {
        &self.card.short_link
    }

    /// afkd's own member id — the wire unit's `self`.
    pub(crate) fn self_author(&self) -> &str {
        &self.self_author
    }
}

/// One card's terminal lifecycle that never reached the board (ADR-0059): what is still
/// owed, so a later `release` of the key finishes it rather than re-running the agent.
struct PendingFinish {
    /// The journal key the `release` arrives with, `<card_id>#<claim_id>`.
    key: String,
    /// The card itself: its title for the log lines, its id for every call.
    card: Card,
    /// The moment's actions that never ran — the suffix `apply_actions` stopped at, so
    /// an action that already landed (a posted `comment`) is never repeated.
    actions: Vec<LifecycleAction>,
    /// The finishing run's facts, for a `comment`'s `@{run:…}` interpolation.
    facts: Facts,
    /// Whether the [`AWAITING_LABEL`] **badge** a park owes is still unwritten. The badge
    /// is what the next beat's scan re-finds the card by, so a park whose label write
    /// failed is *not* delivered.
    park: bool,
    /// Whether the [`PARK_MARKER`] **owner marker** a park owes is still unposted. A
    /// badged card naming no owner is anyone's, so a park that stopped there is not
    /// delivered either.
    park_marker: bool,
    /// The prior [`PARK_MARKER`] comments the owed marker's post still has to prune,
    /// trimmed to the ids the board has not already dropped — deleting an id twice is a
    /// 404.
    park_priors: Vec<String>,
    /// Whether the claim comment is still on the card, so the lease is still owed.
    release: bool,
    /// How many releases have tried this entry, bounded by [`PENDING_FINISH_TRIES`].
    tries: u32,
}

/// The `discuss_with` gate resolved for one poll: afkd's own member id (the tail
/// boundary), whether any author is allowed, and — when not — the allow-list of member
/// ids.
struct DiscussGate {
    self_id: String,
    anyone: bool,
    allowed: HashSet<String>,
}

/// What one claim attempt on one candidate decided.
enum ClaimRound {
    /// We hold the sole live claim, `on_claim` has run, and the unit is built from the
    /// very comment read that judged the race. Boxed because a bare `Unit` is several
    /// hundred bytes against an empty variant.
    Won(Box<Unit>),
    /// A rival's claim out-orders ours. Our claim comment is gone; the next candidate
    /// may be tried.
    Lost,
}

/// What the parked-card scan decided for the whole badged set.
enum ParkScan {
    /// A parked card was answered and re-claimed; this is the beat's unit and
    /// `pick_from` is not read at all.
    Claimed(Box<Unit>),
    /// The scan ran past its budget: the beat ends idle, and the rest waits.
    Spent,
    /// Nothing parked, or nothing answered: fall through to the `pick_from` scan.
    Nothing,
}

/// Who this kind is, as `hello` says: on the **board** (`owner`, the per-process string
/// the claim comment carries) and in the **config** (`service`, plus `roster` — every
/// service the daemon runs, so a [`PARK_MARKER`] naming a service that is gone reads as
/// orphaned rather than as someone else's live work).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Identity {
    /// This afkd process's claim owner, carried on the claim and watermark comments.
    pub(crate) owner: String,
    /// The configured service, instance suffix stripped.
    pub(crate) service: String,
    /// Every service the daemon runs.
    pub(crate) roster: Vec<String>,
}

/// The `trello` kind's vendor half: the board seam, the intake gates, the comment-lock
/// claim, the brief, and the four lifecycle action lists.
pub(crate) struct TrelloUnits {
    board: Box<dyn BoardClient>,
    board_id: String,
    /// Source list new work is claimed from.
    pick_from: String,
    require_member: Option<MemberRef>,
    require_label: Option<String>,
    without_label: Vec<String>,
    discuss_with: Option<DiscussWith>,
    min_age: Duration,
    settle: Duration,
    claim_lifetime: Duration,
    on_claim: Vec<LifecycleAction>,
    on_done: Vec<LifecycleAction>,
    on_fail: Vec<LifecycleAction>,
    on_park: Vec<LifecycleAction>,
    /// The credentials the run's environment carries.
    api_key: String,
    token: String,
    /// This process's claim owner identity.
    owner: String,
    /// The configured service: the name written onto a parked card's [`PARK_MARKER`],
    /// and the one the parked sweep compares a marker against.
    service: String,
    /// Every service the daemon runs: the sweep's test for "is the service that parked
    /// this card still here?".
    roster: Vec<String>,
    /// Where the next beat's badged-card sweep resumes, so a badged set larger than
    /// [`PARK_SCAN_MAX`] is covered across consecutive beats. Taken modulo the set's
    /// length each beat, so cards leaving or joining it can never index out of range.
    park_cursor: Mutex<usize>,
    /// Terminal lifecycles this process could not deliver, drained by
    /// [`release_stale`](Self::release_stale). A `Vec`, not a map: empty in the steady
    /// state, and its insertion order is what bounds it.
    pending: Mutex<Vec<PendingFinish>>,
}

impl TrelloUnits {
    /// The vendor half for `cfg`, talking to the board through `board`, as `id`.
    pub(crate) fn new(board: Box<dyn BoardClient>, cfg: &BoardConfig, id: Identity) -> Self {
        Self {
            board,
            board_id: cfg.board_id.clone(),
            pick_from: cfg.pick_from.clone(),
            require_member: cfg.require_member.clone(),
            require_label: cfg.require_label.clone(),
            without_label: cfg.without_label.clone(),
            discuss_with: cfg.discuss_with.clone(),
            min_age: cfg.min_age,
            settle: CLAIM_SETTLE,
            claim_lifetime: CLAIM_LIFETIME,
            on_claim: cfg.on_claim.clone(),
            on_done: cfg.on_done.clone(),
            on_fail: cfg.on_fail.clone(),
            on_park: cfg.on_park.clone(),
            api_key: cfg.api_key.clone(),
            token: cfg.token.clone(),
            owner: id.owner,
            service: id.service,
            roster: id.roster,
            park_cursor: Mutex::new(0),
            pending: Mutex::new(Vec::new()),
        }
    }

    /// Resolve the `discuss_with` gate's identities once per poll: afkd's own member id
    /// (the tail boundary), and — for a member allow-list — each named member resolved to
    /// an id, with self always removed. `None` when the gate is unset (no resolves, no
    /// comment reads). A member that does not resolve propagates as a `BoardError`.
    fn resolve_discuss_gate(&self) -> Result<Option<DiscussGate>, BoardError> {
        let Some(dw) = &self.discuss_with else {
            return Ok(None);
        };
        let self_id = self
            .board
            .resolve_member(&self.board_id, &MemberRef::SelfMember)?;
        let (anyone, mut allowed) = match dw {
            DiscussWith::Anyone => (true, HashSet::new()),
            DiscussWith::Members(refs) => {
                let mut set = HashSet::new();
                for who in refs {
                    set.insert(self.board.resolve_member(&self.board_id, who)?);
                }
                (false, set)
            }
        };
        // Self is never an allowed author, however it was named — afkd must not answer
        // itself.
        allowed.remove(&self_id);
        Ok(Some(DiscussGate {
            self_id,
            anyone,
            allowed,
        }))
    }

    /// The parked-card sweep, run **ahead of** the `pick_from` resolve on every beat: ask
    /// the board which of its open cards carry [`AWAITING_LABEL`], and re-claim the first
    /// one a human has answered — wherever on the board it sits.
    ///
    /// The reply test is the feedback delta ([`feedback_comments`] against
    /// [`watermark_of`]), so afkd's own tail comments cannot re-arm a card. Only the deny
    /// gate is re-applied: `require_label`, `require_member` and `min_age` were passed at
    /// the first claim, while `without_label` is a human's live veto.
    ///
    /// A card's [`PARK_MARKER`] names who parked it, and only that service resumes it
    /// ([`parked_elsewhere`](Self::parked_elsewhere)); a card naming a service off the
    /// roster is an orphan, taken over with one narrated line.
    fn scan_parked(
        &self,
        diag: &dyn Diag,
        clock: &dyn Clock,
        budget: &ScanBudget,
    ) -> Result<ParkScan, BoardError> {
        let badged: Vec<String> = self
            .board
            .board_cards(&self.board_id)?
            .into_iter()
            .filter(|c| c.labels.iter().any(|l| l == AWAITING_LABEL))
            .map(|c| c.id)
            .collect();
        if badged.is_empty() {
            return Ok(ParkScan::Nothing);
        }
        // afkd's own member id, resolved **lazily** — only once something is badged,
        // which is what keeps the idle beat at exactly one added request. Without it
        // `is_afkd` would degrade to control-markers-only, and the agent's own question
        // would read as a human reply and re-arm the card against itself.
        let me = self
            .board
            .resolve_member(&self.board_id, &MemberRef::SelfMember)?;
        // A rotating window over the badged set, so a hundred unanswered questions cost
        // a bounded number of card reads per beat and are still all reached across
        // consecutive beats.
        let take = badged.len().min(PARK_SCAN_MAX);
        let start = {
            let mut cursor = lock(&self.park_cursor);
            let start = *cursor % badged.len();
            *cursor = start + take;
            start
        };
        for step in 0..take {
            if budget.spent() {
                return Ok(ParkScan::Spent);
            }
            let card_id = &badged[(start + step) % badged.len()];
            let full = match self.board.read_card(card_id) {
                Ok(Some(card)) => card,
                // Archived or deleted between the board read and this one.
                Ok(None) => continue,
                // One card's read failing is not the poll failing: it stays badged and
                // is retried on a later beat.
                Err(e) => {
                    diag.err(&e);
                    continue;
                }
            };
            if self.without_label.iter().any(|l| full.labels.contains(l)) {
                continue;
            }
            {
                let comments = full.comments.as_deref().unwrap_or_default();
                // Someone already has this card in hand — including a park whose badge
                // outlived its own claim release.
                if has_live_claim(comments, self.claim_lifetime) {
                    continue;
                }
                // Nobody has answered yet: leave it alone.
                if feedback_comments(comments, &me).is_empty() {
                    continue;
                }
                // Theirs, and they are still here: sweep past it, silently.
                if self.parked_elsewhere(comments) {
                    continue;
                }
                // What is left is ours, unmarked, or orphaned — and the last of those
                // narrates, on the beat the card is actually taken over.
                if let Some(line) = self.orphan_takeover(&full, comments) {
                    diag.narrate(&line);
                }
            }
            // Routed through the ordinary claim, which re-runs `on_claim`, takes the
            // badge off, and hands back an ordinary fresh unit.
            match self.attempt_claim(full, diag, clock)? {
                ClaimRound::Won(unit) => return Ok(ParkScan::Claimed(unit)),
                ClaimRound::Lost => continue,
            }
        }
        Ok(ParkScan::Nothing)
    }

    /// Whether a parked card is **another live service's**: its newest [`PARK_MARKER`]
    /// names a configured service this daemon runs that is not us. `false` for a card
    /// that is anyone's: an **unmarked** one, **our own** (at the granularity of the
    /// configured service, so a pool copy resumes what its base asked), and one naming a
    /// service **absent from the roster** — orphaned, which the caller narrates through
    /// [`orphan_takeover`](Self::orphan_takeover).
    fn parked_elsewhere(&self, comments: &[Comment]) -> bool {
        let Some(owner) = park_owner(comments) else {
            return false;
        };
        let owner = instance_base(owner).unwrap_or(owner);
        owner != self.service && self.roster.iter().any(|s| s == owner)
    }

    /// The handover line owed for a parked card this service is about to take over from
    /// a parker the config no longer runs — `None` when nothing is owed. Called only
    /// where [`parked_elsewhere`](Self::parked_elsewhere) answered `false`.
    fn orphan_takeover(&self, card: &Card, comments: &[Comment]) -> Option<String> {
        let owner = park_owner(comments)?;
        let owner = instance_base(owner).unwrap_or(owner);
        (owner != self.service).then(|| orphan_line(card, owner))
    }

    /// Claim the next card via the comment lock: the parked sweep first, then the source
    /// list's first card that passes every gate and wins its claim race.
    ///
    /// Unset gates resolve nothing and read no comments, so the default path asks the
    /// board exactly what it always did. The gates run strictly before the claim comment
    /// is posted, so a skipped card is never commented on. A card carrying a live
    /// `[afkd-claim]` is someone's work in flight and is stepped over; a badged card
    /// another live service parked is stepped over too. A race lost despite that does
    /// not end the poll: the scan resumes at the next candidate.
    ///
    /// The whole scan runs under [`ScanBudget`]: past it, the beat ends idle and the rest
    /// of the list waits for the next poll. A claim already under way always finishes.
    pub(crate) fn try_claim_next(
        &self,
        diag: &dyn Diag,
        clock: &dyn Clock,
    ) -> Result<Option<Unit>, BoardError> {
        let budget = ScanBudget::start(clock, diag);
        // Parked work first: a card whose question a human has answered is more due than
        // anything queued behind it. Nothing badged falls straight through, one board
        // read the poorer.
        match self.scan_parked(diag, clock, &budget)? {
            ParkScan::Claimed(unit) => return Ok(Some(*unit)),
            ParkScan::Spent => return Ok(None),
            ParkScan::Nothing => {}
        }
        let list_id = self.board.resolve_list(&self.board_id, &self.pick_from)?;
        let cards = self.board.list_cards(&list_id)?;
        // Resolve each gate's identities once per poll, never per card.
        let required_member = match &self.require_member {
            None => None,
            Some(who) => Some(self.board.resolve_member(&self.board_id, who)?),
        };
        let discuss = self.resolve_discuss_gate()?;
        for card in cards {
            if budget.spent() {
                return Ok(None);
            }
            if let Some(member_id) = &required_member {
                if !card.members.contains(member_id) {
                    continue;
                }
            }
            // Deny first: a card carrying any excluded label is skipped regardless of the
            // other gates, so deny wins over `require_label`.
            if self.without_label.iter().any(|l| card.labels.contains(l)) {
                continue;
            }
            if let Some(label) = &self.require_label {
                if !card.labels.contains(label) {
                    continue;
                }
            }
            // Is this card parked? Read off the free `card.labels`, and the gate on
            // everything the ownership rule below costs.
            let badged = card.labels.iter().any(|l| l == AWAITING_LABEL);
            // The only gate that reads a clock: a card younger than `min_age` is skipped
            // this poll. Above the `discuss_with` arm, which costs a comment read.
            if !self.min_age.is_zero() {
                let created_at = match card.created_at {
                    Some(t) => t,
                    None => self.board.card_created_at(&card.id)?,
                };
                if age_of(created_at) < self.min_age {
                    continue;
                }
            }
            // The comments the remaining gates read: free when the board nested them,
            // and otherwise fetched only for a gate that needs the whole thread —
            // `discuss_with`, and the ownership rule on a *badged* card.
            let fetched;
            let comments: Option<&[Comment]> = match (&card.comments, discuss.is_some() || badged) {
                (Some(nested), _) => Some(nested.as_slice()),
                (None, true) => {
                    fetched = self.board.card_comments(&card.id)?;
                    Some(&fetched)
                }
                (None, false) => None,
            };
            // Someone holds this card's lease: step over it.
            if comments.is_some_and(|cs| has_live_claim(cs, self.claim_lifetime)) {
                continue;
            }
            // Parked, and somebody else's: leave the badge on it, silently.
            if badged && comments.is_some_and(|cs| self.parked_elsewhere(cs)) {
                continue;
            }
            if let (Some(gate), Some(cs)) = (&discuss, comments) {
                if !discuss_tail_passes(cs, gate) {
                    continue;
                }
            }
            // A parked card whose parker this config no longer runs falls free here too,
            // so the takeover narrates here too — the sweep's line.
            if badged {
                if let Some(line) = comments.and_then(|cs| self.orphan_takeover(&card, cs)) {
                    diag.narrate(&line);
                }
            }
            match self.attempt_claim(card, diag, clock)? {
                ClaimRound::Won(unit) => return Ok(Some(*unit)),
                // Someone else won this card; try the next candidate.
                ClaimRound::Lost => continue,
            }
        }
        Ok(None)
    }

    /// Race one candidate for the comment lock: post a claim, settle so a rival's earlier
    /// claim has time to be seen, re-read the thread, and win only as the single earliest
    /// still-live claim ([`won_claim`]).
    ///
    /// The win narrates and runs `on_claim` **here**, before returning, and takes the
    /// park badge and owner marker off a card that carried them. A failed re-read is a
    /// board fault, not a lost round: our claim is released and the error propagates.
    fn attempt_claim(
        &self,
        card: Card,
        diag: &dyn Diag,
        clock: &dyn Clock,
    ) -> Result<ClaimRound, BoardError> {
        let claim = self
            .board
            .post_comment(&card.id, &claim_text(&self.owner))?;
        // afkd's own member id: the claim is one afkd authored, so its author is exactly
        // the self identity, with no extra `/members/me` round-trip.
        let self_author = claim.author.clone();
        clock.sleep(self.settle);
        let mut comments = match self.board.card_comments(&card.id) {
            Ok(c) => c,
            Err(e) => {
                self.drop_claim(&card.id, &claim.id, diag);
                return Err(e);
            }
        };
        if !won_claim(&claim_markers(&comments), &claim.id, self.claim_lifetime) {
            self.drop_claim(&card.id, &claim.id, diag);
            return Ok(ClaimRound::Lost);
        }
        diag.narrate(&claimed_line(&card));
        // The card is being worked, so it is no longer waiting on anyone: take the badge
        // off, and the owner marker with it — a marker exists only while the badge does.
        // Best-effort: the claim is won either way.
        if card.labels.iter().any(|l| l == AWAITING_LABEL) {
            if let Err(e) = self
                .board
                .remove_label(&self.board_id, &card.id, AWAITING_LABEL)
            {
                diag.err(&e);
            }
            let mut removed = Vec::new();
            for prior in comments.iter().filter(|c| is_park(&c.text)) {
                match self.board.delete_comment(&card.id, &prior.id) {
                    Ok(()) => removed.push(prior.id.clone()),
                    Err(e) => diag.err(&e),
                }
            }
            // Keep the snapshot honest: `finish`'s park prune walks it, and a marker left
            // in it after the board dropped it would be deleted a second time — a 404.
            comments.retain(|c| !removed.contains(&c.id));
        }
        // on_claim runs before any fire, so neutral facts.
        self.apply_actions(&self.on_claim, &card, &Facts::none(), diag);
        // The feedback delta and the delivery boundary, computed once from the read that
        // judged the claim, so a comment arriving mid-run lands after the boundary.
        let feedback = feedback_comments(&comments, &self_author);
        let delivered = delivered_upto(&comments, &feedback);
        Ok(ClaimRound::Won(Box::new(Unit {
            card,
            claim_id: claim.id,
            comments,
            delivered_upto: delivered,
            self_author,
        })))
    }

    /// Release a claim comment this poll posted but will not keep. Best-effort but not
    /// silent.
    fn drop_claim(&self, card_id: &str, claim_id: &str, diag: &dyn Diag) {
        if let Err(e) = self.board.delete_comment(card_id, claim_id) {
            diag.err(&e);
        }
    }

    /// On the `discuss_with` path, guarantee afkd is the last speaker after a turn:
    /// re-read the card's comments and, if no comment authored by afkd this turn is new,
    /// post one backstop comment. The diff keys on comment id + author, so an attempt
    /// marker afkd posted *does* count as speaking and a human straggler does *not*. If
    /// the re-read fails, post anyway — a rare redundant comment beats the re-fire loop.
    fn post_backstop_if_silent(
        &self,
        unit: &Unit,
        outcome: UnitOutcome,
        facts: &Facts,
        diag: &dyn Diag,
    ) {
        let spoke = match self.board.card_comments(&unit.card.id) {
            Ok(after) => {
                let before_ids: HashSet<&str> =
                    unit.comments.iter().map(|c| c.id.as_str()).collect();
                after
                    .iter()
                    .any(|c| c.author == unit.self_author && !before_ids.contains(c.id.as_str()))
            }
            Err(e) => {
                diag.err(&e);
                false
            }
        };
        if !spoke {
            if let Err(e) = self
                .board
                .post_comment(&unit.card.id, &backstop_text(outcome, facts))
            {
                diag.err(&e);
            }
        }
    }

    /// Run a lifecycle moment's actions in order, surfacing and stopping on the first
    /// failure. Returns **how many** succeeded, so a caller retrying a partial moment
    /// finds the outstanding set at `actions[done..]`, with a posted `comment` never
    /// posted twice.
    pub(crate) fn apply_actions(
        &self,
        actions: &[LifecycleAction],
        card: &Card,
        facts: &Facts,
        diag: &dyn Diag,
    ) -> usize {
        for (done, action) in actions.iter().enumerate() {
            if let Err(e) = self.do_action(action, card, facts, diag) {
                diag.err(&e);
                return done;
            }
        }
        actions.len()
    }

    /// Remember a terminal lifecycle this process could not deliver, for
    /// [`release_stale`](Self::release_stale) to replay. Past [`PENDING_MAX`] the oldest
    /// goes.
    fn remember_pending(&self, owed: PendingFinish) {
        let mut pending = lock(&self.pending);
        pending.push(owed);
        if pending.len() > PENDING_MAX {
            pending.remove(0);
        }
    }

    /// Take the outstanding terminal lifecycle recorded for `key`, if any.
    fn take_pending(&self, key: &str) -> Option<PendingFinish> {
        let mut pending = lock(&self.pending);
        let at = pending.iter().position(|p| p.key == key)?;
        Some(pending.remove(at))
    }

    /// One beat's attempt at an owed terminal lifecycle: the park badge, then the actions
    /// that never ran, then the owner marker, then the lease release — `finish`'s order,
    /// so a partial delivery resumes rather than restarts. `Some(true)` when everything
    /// landed, `Some(false)` when something is still owed and kept for the next beat, and
    /// `None` once [`PENDING_FINISH_TRIES`] beats have failed — the entry is dropped and
    /// the caller falls through to the crash-victim reversal.
    fn retry_pending(
        &self,
        mut owed: PendingFinish,
        claim_id: &str,
        diag: &dyn Diag,
    ) -> Option<bool> {
        owed.tries += 1;
        if owed.park {
            match self
                .board
                .add_label(&self.board_id, &owed.card.id, AWAITING_LABEL)
            {
                Ok(()) => {
                    diag.narrate(&parked_line(&owed.card));
                    owed.park = false;
                }
                Err(e) => diag.err(&e),
            }
        }
        if !owed.park {
            let done = self.apply_actions(&owed.actions, &owed.card, &owed.facts, diag);
            owed.actions.drain(..done);
        }
        // The owner marker, before the release and not gated on the badge — `finish`'s
        // order exactly.
        if owed.park_marker {
            match self
                .board
                .post_comment(&owed.card.id, &park_text(&self.service))
            {
                Ok(_) => {
                    owed.park_marker = false;
                    self.prune_park_priors(&owed.card.id, &mut owed.park_priors, diag);
                }
                Err(e) => diag.err(&e),
            }
        }
        let owed_before_release = owed.park || !owed.actions.is_empty() || owed.park_marker;
        if !owed_before_release && owed.release {
            match self.board.delete_comment(&owed.card.id, claim_id) {
                Ok(()) => {
                    diag.narrate(&released_line(&owed.card));
                    owed.release = false;
                }
                Err(e) => diag.err(&e),
            }
        }
        if !owed_before_release && !owed.release {
            return Some(true);
        }
        if owed.tries >= PENDING_FINISH_TRIES {
            diag.err(&format_args!(
                "giving up on the terminal lifecycle for card \"{}\" after \
                 {PENDING_FINISH_TRIES} tries; returning it to \"{}\"",
                owed.card.title, self.pick_from
            ));
            return None;
        }
        self.remember_pending(owed);
        Some(false)
    }

    /// Delete the prior [`PARK_MARKER`] comments a just-posted marker supersedes, leaving
    /// in `priors` exactly the ids the board still carries. Best-effort: [`park_owner`]
    /// reads the newest, and the fresh one always is.
    fn prune_park_priors(&self, card_id: &str, priors: &mut Vec<String>, diag: &dyn Diag) {
        priors.retain(|id| match self.board.delete_comment(card_id, id) {
            Ok(()) => false,
            Err(e) => {
                diag.err(&e);
                true
            }
        });
    }

    /// Carry out one lifecycle action against a card, and on success narrate it. Each
    /// board call is `?`-propagated *before* the narration, so a failing action emits no
    /// success line.
    fn do_action(
        &self,
        action: &LifecycleAction,
        card: &Card,
        facts: &Facts,
        diag: &dyn Diag,
    ) -> Result<(), BoardError> {
        match action {
            LifecycleAction::MarkComplete => self.board.complete_card(&card.id)?,
            LifecycleAction::Archive => self.board.archive_card(&card.id)?,
            LifecycleAction::MoveTo { list, position } => {
                let list_id = self.board.resolve_list(&self.board_id, list)?;
                self.board.move_card(&card.id, &list_id, *position)?;
            }
            LifecycleAction::AddLabel { name } => {
                self.board.add_label(&self.board_id, &card.id, name)?;
            }
            LifecycleAction::RemoveLabel { name } => {
                self.board.remove_label(&self.board_id, &card.id, name)?;
            }
            LifecycleAction::AddMember(member) => {
                self.board.add_member(&self.board_id, &card.id, member)?;
            }
            LifecycleAction::RemoveMember(member) => {
                self.board.remove_member(&self.board_id, &card.id, member)?;
            }
            LifecycleAction::Comment(text) => {
                // `@{run:…}` interpolated against the run's facts (ADR-0064); the settings
                // reader proved every reference legal for this moment.
                self.board
                    .post_comment(&card.id, &run_ref::substitute(text, facts))?;
            }
        }
        diag.narrate(&action_line(action, card));
        Ok(())
    }

    /// Finish, or reverse, a claim named by a whole journal key — the `release` call.
    ///
    /// **The lifecycle is still owed** (a `held` finish): replay it — an owed park badge,
    /// the outstanding actions, the owner marker, the lease release. Full success is
    /// `Some(true)`; anything left is kept and `Some(false)`, up to
    /// [`PENDING_FINISH_TRIES`] — after which the card takes the reversal below.
    ///
    /// **Otherwise it is a crash victim**, or a unit afkd handed back unrun: prune the
    /// `[afkd-claim]` comment and move the card back to the **bottom** of `pick_from`, so
    /// a repeatedly-crashing card cannot starve the queue head. Best-effort: a board
    /// failure is diagnosed and the key still released, so a persistently-failing board
    /// cannot loop the reap.
    ///
    /// `None` for a key with no `#`, which names nothing releasable.
    pub(crate) fn release_stale(&self, key: &str, diag: &dyn Diag) -> Option<bool> {
        let (card_id, claim_id) = split_card_key(key)?;
        if let Some(owed) = self.take_pending(key) {
            if let Some(verdict) = self.retry_pending(owed, claim_id, diag) {
                return Some(verdict);
            }
        }
        if let Err(e) = self.board.delete_comment(card_id, claim_id) {
            diag.err(&e);
        }
        match self.board.resolve_list(&self.board_id, &self.pick_from) {
            Ok(list_id) => {
                if let Err(e) = self
                    .board
                    .move_card(card_id, &list_id, ListPosition::Bottom)
                {
                    diag.err(&e);
                }
            }
            Err(e) => diag.err(&e),
        }
        Some(true)
    }

    /// Keep this card's claim comment alive for as long as afkd's fire holds it.
    /// Best-effort: a failed edit is diagnosed, and the next renewal is minutes away.
    pub(crate) fn renew(&self, unit: &Unit, renewal: u64, diag: &dyn Diag) {
        if let Err(e) = self.board.edit_comment(
            &unit.card.id,
            &unit.claim_id,
            &claim_renewal_text(&self.owner, renewal),
        ) {
            diag.err(&e);
        }
    }

    /// The unit as it crosses the wire in a `poll` reply — every field the built-in's
    /// spine read off it:
    ///
    /// - `id`: the sanitized short link (the run-name component);
    /// - `key`: `<card_id>#<claim_id>` (the claim-journal key);
    /// - `thread`: the raw short link, stable across every claim (ADR-0067);
    /// - `seen`: the claim-read comment ids, which the brief already carries;
    /// - `self`: afkd's own member id;
    /// - `env`: the credentials and the card id the skill reads;
    /// - `files`: the brief, unframed — afkd frames `task.md` itself.
    pub(crate) fn wire_unit(&self, unit: &Unit) -> WireUnit {
        WireUnit {
            id: sanitize(&unit.card.short_link),
            key: card_key(&unit.card.id, &unit.claim_id),
            thread: unit.card.short_link.clone(),
            seen: unit.comments.iter().map(|c| c.id.clone()).collect(),
            me: unit.self_author.clone(),
            env: BTreeMap::from([
                (ENV_API_KEY.to_string(), self.api_key.clone()),
                (ENV_TOKEN.to_string(), self.token.clone()),
                (ENV_BOARD_ID.to_string(), self.board_id.clone()),
                (ENV_CARD_ID.to_string(), unit.card.id.clone()),
            ]),
            files: vec![WireFile {
                path: TASK_FILE.to_string(),
                text: brief_text(&unit.card, &unit.comments, &unit.self_author),
            }],
        }
    }

    /// The clarification gate's read: a [`PARK_FILE`] marker under the attempt's scratch
    /// root means the agent asked for human input (the skill's `ask.py` wrote it), so the
    /// attempt **parks** the card. The marker is read before, and overrides, afkd's own
    /// signal-only verdict.
    pub(crate) fn classify(scratch: &Path, verdict: UnitOutcome) -> UnitOutcome {
        if scratch.join(PARK_FILE).is_file() {
            return UnitOutcome::Park;
        }
        verdict
    }

    /// A faulting attempt leaves a marked comment on the card, so the budget spent on it
    /// is legible from the board itself. afkd sends the fault's sentence; with none there
    /// is nothing to mark.
    pub(crate) fn attempt_failed(
        &self,
        unit: &Unit,
        n: u32,
        max: u32,
        reason: Option<&str>,
        diag: &dyn Diag,
    ) {
        let Some(reason) = reason else {
            return;
        };
        if let Err(e) = self
            .board
            .post_comment(&unit.card.id, &attempt_text(n, max, reason))
        {
            diag.err(&e);
        }
    }

    /// Every comment on the unit's card, for afkd's mid-run watch.
    pub(crate) fn comments(&self, unit: &Unit) -> Result<Vec<Comment>, BoardError> {
        self.board.card_comments(&unit.card.id)
    }

    /// The terminal lifecycle: the moment's actions, then either the durable
    /// `[afkd-ran]` watermark (the default path) or the last-speaker backstop (the
    /// `discuss_with` path), and the lease release last of all. A park adds the
    /// [`AWAITING_LABEL`] badge before any `on_park` extras, and the [`PARK_MARKER`]
    /// naming this service after the branch and **before** the release, so the card is
    /// never badged, unclaimed and ownerless at once.
    ///
    /// Returns whether the moment was delivered. Anything short of "every action landed,
    /// the badge and owner marker are up, and the claim comment is gone" is remembered,
    /// and a later `release` of the key finishes it.
    pub(crate) fn finish(
        &self,
        unit: &Unit,
        outcome: UnitOutcome,
        facts: &Facts,
        diag: &dyn Diag,
    ) -> bool {
        let actions = match outcome {
            UnitOutcome::Clean => &self.on_done,
            UnitOutcome::Park => &self.on_park,
            UnitOutcome::Failed => &self.on_fail,
        };
        // The badge, before any user extras and owned by the kind itself, so the gate
        // holds with an empty `on_park`. It is the *finding* half, so it joins the
        // delivery verdict.
        let mut park_owed = matches!(outcome, UnitOutcome::Park);
        if park_owed {
            match self
                .board
                .add_label(&self.board_id, &unit.card.id, AWAITING_LABEL)
            {
                Ok(()) => {
                    diag.narrate(&parked_line(&unit.card));
                    park_owed = false;
                }
                Err(e) => diag.err(&e),
            }
        }
        // An owed badge stops the moment where a failed action would: the extras would
        // announce a state the board does not hold.
        let done = if park_owed {
            0
        } else {
            self.apply_actions(actions, &unit.card, facts, diag)
        };
        match &self.discuss_with {
            // Default path: post the durable high-water mark carrying `delivered_upto`
            // — the boundary fixed at the claim read — then prune the priors out of the
            // claim-read snapshot. Best-effort: a re-delivered comment is safe.
            None => {
                match self
                    .board
                    .post_comment(&unit.card.id, &ran_text(&self.owner, unit.delivered_upto))
                {
                    Ok(_) => {
                        for prior in unit.comments.iter().filter(|c| is_ran(&c.text)) {
                            if let Err(e) = self.board.delete_comment(&unit.card.id, &prior.id) {
                                diag.err(&e);
                            }
                        }
                    }
                    Err(e) => diag.err(&e),
                }
            }
            // Grooming path: afkd's own last comment IS the tail boundary, so the
            // backstop guarantees afkd is the last speaker.
            Some(_) => self.post_backstop_if_silent(unit, outcome, facts, diag),
        }
        // The park owner marker: AFTER the watermark/backstop branch (posted before it,
        // it would count as afkd speaking and suppress the backstop) and BEFORE the
        // release (a badged card naming no owner is anyone's). Post new, then prune
        // priors out of the claim-read snapshot.
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
                .board
                .post_comment(&unit.card.id, &park_text(&self.service))
            {
                Ok(_) => {
                    marker_owed = false;
                    self.prune_park_priors(&unit.card.id, &mut park_priors, diag);
                }
                Err(e) => diag.err(&e),
            }
        }
        let mut release_owed = true;
        if !park_owed && done == actions.len() && !marker_owed {
            match self.board.delete_comment(&unit.card.id, &unit.claim_id) {
                Ok(()) => {
                    diag.narrate(&released_line(&unit.card));
                    release_owed = false;
                }
                Err(e) => diag.err(&e),
            }
        }
        let delivered = !release_owed;
        if !delivered {
            self.remember_pending(PendingFinish {
                key: card_key(&unit.card.id, &unit.claim_id),
                card: unit.card.clone(),
                actions: actions[done..].to_vec(),
                facts: facts.clone(),
                park: park_owed,
                park_marker: marker_owed,
                park_priors,
                release: true,
                tries: 0,
            });
        }
        delivered
    }
}

/// Reduce a card id to a slug safe for a scratch dir / run token name.
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
        "card".to_string()
    } else {
        slug
    }
}

// --- Pure helpers (no seam): claim/marker logic, task text, the brief.

/// The base name of an instance copy — `afkd::develop+1` is `afkd::develop` — or `None`
/// for a name that is not instance-shaped. afkd's `afkd_config::instance_base`: the
/// suffix is read off the **last** `+`, both halves must be non-empty, and the tail must
/// be all ASCII digits.
fn instance_base(name: &str) -> Option<&str> {
    let (base, digits) = name.rsplit_once('+')?;
    let shaped =
        !base.is_empty() && !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit());
    shaped.then_some(base)
}

/// The claim-journal key for a card's in-flight claim (ADR-0059): the card id (the
/// move-back subject at reap) and the claim comment id (the prune target), joined by `#`.
fn card_key(card_id: &str, claim_id: &str) -> String {
    format!("{card_id}#{claim_id}")
}

/// Split a [`card_key`] back into its `(card_id, claim_id)`. `None` if the key carries no
/// `#`. Splits on the *first* `#` (a card id has none).
fn split_card_key(key: &str) -> Option<(&str, &str)> {
    key.split_once('#')
}

/// Map a card's comments into the view the claim decision reads: the **post** time
/// orders and the **last edit** judges liveness. The whole list is handed over,
/// unfiltered, because the marker predicate lives inside the decision.
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

/// The text of a failed-attempt marker comment (`n/max: reason`).
fn attempt_text(n: u32, max: u32, reason: &str) -> String {
    format!("{ATTEMPT_MARKER} {n}/{max}: {reason}")
}

/// The human-readable success line for a completed lifecycle `action` on `card`. Names
/// the card's title (never its id) and, for a move, the *configured* list name and the
/// effective placement.
fn action_line(action: &LifecycleAction, card: &Card) -> String {
    let title = &card.title;
    match action {
        LifecycleAction::MarkComplete => format!("{ACTION_TAG} marking card \"{title}\" complete"),
        LifecycleAction::Archive => format!("{ACTION_TAG} archiving card \"{title}\""),
        LifecycleAction::MoveTo { list, position } => {
            let pos = position_word(*position);
            format!("{ACTION_TAG} moving card \"{title}\" to list \"{list}\" (at {pos})")
        }
        LifecycleAction::AddLabel { name } => {
            format!("{ACTION_TAG} adding label \"{name}\" to card \"{title}\"")
        }
        LifecycleAction::RemoveLabel { name } => {
            format!("{ACTION_TAG} removing label \"{name}\" from card \"{title}\"")
        }
        LifecycleAction::AddMember(member) => {
            let who = member_word(member);
            format!("{ACTION_TAG} adding member \"{who}\" to card \"{title}\"")
        }
        LifecycleAction::RemoveMember(member) => {
            let who = member_word(member);
            format!("{ACTION_TAG} removing member \"{who}\" from card \"{title}\"")
        }
        LifecycleAction::Comment(_) => {
            // Name the card, never the comment body: bodies can be long and multi-line.
            format!("{ACTION_TAG} commenting on card \"{title}\"")
        }
    }
}

/// The placement word logged for a move's [`ListPosition`].
fn position_word(position: ListPosition) -> &'static str {
    match position {
        ListPosition::Top => "top",
        ListPosition::Bottom => "bottom",
    }
}

/// The member word logged for a member action: the **configured** operand, never the
/// board id or username it resolves to.
fn member_word(member: &MemberRef) -> &str {
    match member {
        MemberRef::SelfMember => "self",
        MemberRef::Username(username) => username,
    }
}

/// The success line for a won claim, naming the card's title.
fn claimed_line(card: &Card) -> String {
    format!("{ACTION_TAG} claimed card \"{}\"", card.title)
}

/// The success line for a released lease, naming the card's title.
fn released_line(card: &Card) -> String {
    format!("{ACTION_TAG} released claim on card \"{}\"", card.title)
}

/// The handover line for a parked card whose owner marker names a service this config no
/// longer runs. A handover is not a fault, so it is narrated, not diagnosed.
fn orphan_line(card: &Card, gone: &str) -> String {
    format!(
        "{ACTION_TAG} resuming card \"{}\", parked by service \"{gone}\" which this config no longer runs",
        card.title
    )
}

/// The success line for a parked card, naming the card's title and the badge a person
/// will see on the board.
fn parked_line(card: &Card) -> String {
    format!(
        "{ACTION_TAG} parked card \"{}\" awaiting a reply (label \"{AWAITING_LABEL}\")",
        card.title
    )
}

/// Build the task text: the title, or the title, a blank line, then the body.
fn task_text(card: &Card) -> String {
    if card.description.trim().is_empty() {
        card.title.clone()
    } else {
        format!("{}\n\n{}", card.title, card.description)
    }
}

/// Render the card's checklists as the brief's `## Checklists` section, or `""` when the
/// card carries none. Each checklist is a `### <name> (<done>/<total>)` header followed by
/// one `- [x] <text> · id: <id>` line per item; the `· id: <id>` is the handle the tick
/// skill needs.
fn render_checklists(card: &Card) -> String {
    if card.checklists.is_empty() {
        return String::new();
    }
    let mut s = String::from("## Checklists");
    for list in &card.checklists {
        s.push_str(&render_checklist(list));
    }
    s
}

/// One checklist rendered as its blank-line-separated header and item lines.
fn render_checklist(list: &Checklist) -> String {
    let done = list.items.iter().filter(|i| i.complete).count();
    let total = list.items.len();
    let mut s = format!("\n\n### {} ({done}/{total})", list.name);
    for item in &list.items {
        let box_glyph = if item.complete { "[x]" } else { "[ ]" };
        s.push_str(&format!("\n- {box_glyph} {} · id: {}", item.name, item.id));
    }
    s
}

/// Text of the run-end watermark comment: the marker, `owner`, and the delivery boundary
/// `upto=<epoch-seconds>` the next run measures its delta from.
fn ran_text(owner: &str, upto: SystemTime) -> String {
    let secs = upto.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    format!("{RAN_MARKER} owner={owner} upto={secs}")
}

/// The backstop comment posted at the end of a silent `discuss_with` turn. It carries no
/// marker: afkd posts it, so it is author=self, which is what both the tail boundary and
/// the brief key on.
fn backstop_text(outcome: UnitOutcome, facts: &Facts) -> String {
    match outcome {
        UnitOutcome::Clean => "reviewed, nothing to add".to_string(),
        // A parked turn that said nothing still has to leave afkd last-speaker.
        UnitOutcome::Park => "awaiting a human reply".to_string(),
        // The deciding attempt is the last failed one, so its fault reason is the
        // exhaustion reason; a `Failed` with no fault states the outcome with no reason.
        UnitOutcome::Failed => format!("run did not complete: {}", facts.fault().unwrap_or("")),
    }
}

/// Whether a comment is the run-end watermark marker.
fn is_ran(text: &str) -> bool {
    text.trim_start().starts_with(RAN_MARKER)
}

/// Text of the park owner comment: the marker and the **configured service** that parked
/// the card — never the claim owner, which changes on every restart.
fn park_text(service: &str) -> String {
    format!("{PARK_MARKER} service={service}")
}

/// Whether a comment is the park owner marker — the prefix test, so a marker whose
/// `service=` is missing or garbled is still pruned, deleted and hidden from the brief.
fn is_park(text: &str) -> bool {
    text.trim_start().starts_with(PARK_MARKER)
}

/// The service named on an `[afkd-park]` marker, if any. `None` for a non-marker, or a
/// marker carrying no (or an empty) `service=` — which reads as *no owner*.
fn park_service(text: &str) -> Option<&str> {
    let t = text.trim_start();
    if !t.starts_with(PARK_MARKER) {
        return None;
    }
    t.split_whitespace()
        .find_map(|w| w.strip_prefix("service="))
        .filter(|name| !name.is_empty())
}

/// The service named on the card's **newest** `[afkd-park]` marker, if any — newest,
/// since the prune that should leave exactly one is best-effort. Decided on
/// [`is_park`] first, so a newest marker whose value is garbled names *nobody* rather
/// than deferring to an older one.
fn park_owner(comments: &[Comment]) -> Option<&str> {
    comments
        .iter()
        .filter(|c| is_park(&c.text))
        .max_by_key(|c| c.posted_at)
        .and_then(|c| park_service(&c.text))
}

/// The delivery boundary stamped on an `[afkd-ran]` marker's text, if present and
/// parseable.
fn ran_upto(text: &str) -> Option<SystemTime> {
    let t = text.trim_start();
    if !t.starts_with(RAN_MARKER) {
        return None;
    }
    t.split_whitespace()
        .find_map(|w| w.strip_prefix("upto="))
        .and_then(|s| s.parse::<u64>().ok())
        .map(|secs| UNIX_EPOCH + Duration::from_secs(secs))
}

/// The high-water mark set by prior `[afkd-ran]` markers: the newest delivery boundary
/// they carry. A marker whose `upto=` cannot be parsed contributes nothing (ADR-0069).
fn watermark_of(comments: &[Comment]) -> Option<SystemTime> {
    comments.iter().filter_map(|c| ran_upto(&c.text)).max()
}

/// The delivery boundary to stamp on this run's watermark: the prior watermark or the
/// newest delivered comment, whichever is later — anchored to the board's post times,
/// never the local clock.
fn delivered_upto(comments: &[Comment], feedback: &[Comment]) -> SystemTime {
    let prior = watermark_of(comments);
    let newest_delivered = feedback.iter().map(|c| c.posted_at).max();
    [prior, newest_delivered]
        .into_iter()
        .flatten()
        .max()
        .unwrap_or(UNIX_EPOCH)
}

/// Whether a comment is one of afkd's bookkeeping **control markers** (claim / attempt /
/// ran / park), dropped from the brief entirely.
fn is_control_marker(text: &str) -> bool {
    let t = text.trim_start();
    is_claim(text) || t.starts_with(ATTEMPT_MARKER) || t.starts_with(RAN_MARKER) || is_park(text)
}

/// Whether a comment is afkd's own: authored by afkd's member id, or — the evidence path
/// for an action carrying no `idMemberCreator` — a control marker, which nothing else
/// writes.
pub(crate) fn is_afkd(c: &Comment, self_id: &str) -> bool {
    (!self_id.is_empty() && c.author == self_id) || is_control_marker(&c.text)
}

/// The human-feedback delta to deliver this run: comments posted strictly after the
/// run's boundary, with afkd's own dropped, oldest-first.
///
/// The boundary is two-tier: a prior `[afkd-ran]` watermark wins; without one (the
/// `discuss_with` path posts none) it falls back to afkd's newest **conversational**
/// comment — control markers excluded, so this run's own claim cannot mask every real
/// human comment. With neither, all human comments are kept.
fn feedback_comments(comments: &[Comment], self_id: &str) -> Vec<Comment> {
    let boundary = watermark_of(comments).or_else(|| {
        comments
            .iter()
            .filter(|c| is_afkd(c, self_id) && !is_control_marker(&c.text))
            .map(|c| c.posted_at)
            .max()
    });
    let mut kept: Vec<Comment> = comments
        .iter()
        .filter(|c| !is_afkd(c, self_id))
        .filter(|c| boundary.is_none_or(|b| c.posted_at > b))
        .cloned()
        .collect();
    kept.sort_by_key(|c| c.posted_at);
    kept
}

/// One comment rendered for the brief: `**<speaker>:** <body>`, the speaker the literal
/// `afkd` for afkd's own, else the commenter's display name.
fn render_comment(c: &Comment, self_id: &str) -> String {
    let speaker = if is_afkd(c, self_id) {
        "afkd"
    } else {
        &c.author_name
    };
    format!("**{speaker}:** {}", c.text.trim())
}

/// The brief afkd writes to `task.md`: the task text, the checklists, then the **full
/// comment thread** — prior conversation and the new, actionable comments, each under
/// its own heading, both oldest-first. Control markers are dropped.
fn brief_text(card: &Card, comments: &[Comment], self_id: &str) -> String {
    let mut s = task_text(card);
    let checklists = render_checklists(card);
    if !checklists.is_empty() {
        s.push_str("\n\n");
        s.push_str(&checklists);
    }
    let new = feedback_comments(comments, self_id);
    let new_ids: HashSet<&str> = new.iter().map(|c| c.id.as_str()).collect();
    let mut prior: Vec<&Comment> = comments
        .iter()
        .filter(|c| !is_control_marker(&c.text))
        .filter(|c| !new_ids.contains(c.id.as_str()))
        .collect();
    prior.sort_by_key(|c| c.posted_at);

    if !prior.is_empty() {
        s.push_str("\n\n## Earlier conversation\n");
        for c in &prior {
            s.push('\n');
            s.push_str(&render_comment(c, self_id));
            s.push('\n');
        }
    }
    if !new.is_empty() {
        s.push_str("\n\n## New comments\n");
        for c in &new {
            s.push('\n');
            s.push_str(&render_comment(c, self_id));
            s.push('\n');
        }
    }
    s
}

/// How old a card (or a claim) is right now, saturating at zero for a time in the
/// future. The one place this kind reads a wall clock.
fn age_of(created_at: SystemTime) -> Duration {
    SystemTime::now()
        .duration_since(created_at)
        .unwrap_or_default()
}

/// Whether `comments` carry a still-live `[afkd-claim]` — someone's lease on this card,
/// so the scan must step over it. Measured on the **renewal**, against the wall clock,
/// with the inclusive `<= lifetime` boundary [`won_claim`] uses.
fn has_live_claim(comments: &[Comment], lifetime: Duration) -> bool {
    comments
        .iter()
        .any(|c| is_claim(&c.text) && age_of(c.renewed_at) <= lifetime)
}

/// Whether a card's `comments` pass the `discuss_with` claim gate. The boundary is
/// afkd's own last comment, with two exemptions: a `[afkd-claim]` (the lease, not
/// speech) and a `[afkd-park]` (bookkeeping posted after the turn's last word). On first
/// sight — afkd never commented — the card fires unconditionally; after that, iff a
/// comment past the boundary is from an allowed, non-self author. The whole tail is
/// scanned, so a disallowed author cannot mask an allowed one.
fn discuss_tail_passes(comments: &[Comment], gate: &DiscussGate) -> bool {
    let spoke = |c: &&Comment| c.author == gate.self_id && !is_claim(&c.text) && !is_park(&c.text);
    let Some(boundary) = comments.iter().filter(spoke).map(|c| c.posted_at).max() else {
        return true;
    };
    comments.iter().any(|c| {
        c.posted_at > boundary
            && c.author != gate.self_id
            && (gate.anyone || gate.allowed.contains(&c.author))
    })
}

#[cfg(test)]
mod tests {
    //! No network: these drive the in-memory `MockBoard` on a fake clock, ported from the
    //! built-in's own suite (afkd's `crates/trello/src/trigger.rs`).
    //!
    //! What the built-in's suite drove through afkd's spine — `poll`, `run_unit`, `drive`
    //! and the journal reaper — is driven here through the calls afkd makes over the
    //! wire, in the spine's order: [`try_claim_next`](TrelloUnits::try_claim_next) for a
    //! poll; per attempt [`classify`](TrelloUnits::classify) and, on a failure,
    //! [`attempt_failed`](TrelloUnits::attempt_failed); then
    //! [`finish`](TrelloUnits::finish); and [`release_stale`](TrelloUnits::release_stale)
    //! for each key a `held` finish or a crash left behind.
    //!
    //! Not ported, because they exercise afkd's spine rather than this vendor half: the
    //! drive loop and its attempt count, the claim journal's `claim`/`release`/`abandon`
    //! call sequences, the stop token's abandons (a settle interrupt, a stop after the
    //! win, a stop mid-scan), the panic abandon, the `service_error`/fire events, the
    //! framing of `task.md`, the mid-run watch and its follow-up rounds, and the
    //! cross-claim "failed twice" cap. Where one of them also asserted a board effect,
    //! that half is kept — a crash victim's reversal through `release_stale`, say.

    use super::*;
    use crate::board::{Action, MockBoard, SELF_ID};
    use crate::claim::CLAIM_MARKER;
    use crate::common::{CaptureDiag, FakeClock, TempDir};
    use std::sync::Arc;

    fn move_to(list: &str) -> LifecycleAction {
        LifecycleAction::MoveTo {
            list: list.to_string(),
            position: ListPosition::Top,
        }
    }

    fn move_to_at(list: &str, position: ListPosition) -> LifecycleAction {
        LifecycleAction::MoveTo {
            list: list.to_string(),
            position,
        }
    }

    fn add_label(name: &str) -> LifecycleAction {
        LifecycleAction::AddLabel {
            name: name.to_string(),
        }
    }

    fn remove_label(name: &str) -> LifecycleAction {
        LifecycleAction::RemoveLabel {
            name: name.to_string(),
        }
    }

    fn comment_action(text: &str) -> LifecycleAction {
        LifecycleAction::Comment(text.to_string())
    }

    fn add_member_named(username: &str) -> LifecycleAction {
        LifecycleAction::AddMember(MemberRef::Username(username.to_string()))
    }

    fn remove_member_named(username: &str) -> LifecycleAction {
        LifecycleAction::RemoveMember(MemberRef::Username(username.to_string()))
    }

    /// The built-in suite's base config. The attempt bound is afkd's, so it is not here:
    /// [`Harness::run`] takes it.
    fn cfg(
        on_claim: Vec<LifecycleAction>,
        on_done: Vec<LifecycleAction>,
        on_fail: Vec<LifecycleAction>,
    ) -> BoardConfig {
        BoardConfig {
            board_address: "https://trello.com/b/BID/x".into(),
            board_id: "BID".into(),
            base_url: crate::client::TRELLO_BASE.into(),
            api_key: "k".into(),
            token: "t".into(),
            pick_from: "Up for Grabs".into(),
            on_claim,
            on_done,
            on_fail,
            ..BoardConfig::default()
        }
    }

    /// The facts afkd sends for an attempt that proceeded.
    fn proceed() -> Facts {
        Facts::none()
    }

    /// The facts afkd sends for an attempt that faulted with `reason`.
    fn fault(reason: &str) -> Facts {
        Facts {
            signal: "fault".into(),
            reason: Some(reason.into()),
            ..Facts::none()
        }
    }

    /// The facts afkd sends for an attempt whose workflow broke off.
    fn broke() -> Facts {
        Facts {
            signal: "break".into(),
            ..Facts::none()
        }
    }

    /// A run that proceeded after 5 s, $1.50 and 3 turns, as `run_name`.
    fn measured(run_name: &str) -> Facts {
        Facts {
            duration_ms: 5_000,
            cost: 1.5,
            turns: Some(3),
            run_name: Some(run_name.into()),
            ..Facts::none()
        }
    }

    /// afkd's own signal-only verdict (`UnitOutcome::from_facts`), which `classify`
    /// receives.
    fn verdict(facts: &Facts) -> UnitOutcome {
        match facts.fault() {
            Some(_) => UnitOutcome::Failed,
            None => UnitOutcome::Clean,
        }
    }

    /// The configured service every harness runs as unless a test names another.
    const TEST_SERVICE: &str = "afkd::develop";

    /// The other service on the board in the ownership tests.
    const OTHER_SERVICE: &str = "afkd::discuss";

    /// What one run of a unit came to, as afkd's spine drives it.
    struct Run {
        outcome: UnitOutcome,
        /// `finish`'s verdict: `false` is a `held` finish.
        delivered: bool,
        /// How many attempts ran.
        runs: u32,
    }

    /// The vendor half over a shared [`MockBoard`], with the fake clock and capturing
    /// diagnostic its calls are handed.
    struct Harness {
        board: Arc<MockBoard>,
        units: TrelloUnits,
        diag: CaptureDiag,
        clock: FakeClock,
    }

    impl Harness {
        /// The ordinary single-service harness: a fresh board, and an identity naming
        /// [`TEST_SERVICE`] on a roster holding only itself.
        fn new(cfg: BoardConfig, owner: &str) -> Self {
            Self::on_board(
                Arc::new(MockBoard::new()),
                cfg,
                owner,
                TEST_SERVICE,
                &[TEST_SERVICE],
            )
        }

        /// The vendor half over an **existing** board, under a named service and roster:
        /// the shape the park-ownership tests need, where two services poll one board.
        fn on_board(
            board: Arc<MockBoard>,
            cfg: BoardConfig,
            owner: &str,
            service: &str,
            roster: &[&str],
        ) -> Self {
            Self::as_service(
                board,
                cfg,
                Identity {
                    owner: owner.to_string(),
                    service: service.to_string(),
                    roster: roster.iter().map(|s| s.to_string()).collect(),
                },
            )
        }

        fn as_service(board: Arc<MockBoard>, cfg: BoardConfig, id: Identity) -> Self {
            let units = TrelloUnits::new(Box::new(Arc::clone(&board)), &cfg, id);
            Self {
                board,
                units,
                diag: CaptureDiag::default(),
                clock: FakeClock::new(),
            }
        }

        /// One `poll`, raw: the board fault is the caller's.
        fn try_claim(&self) -> Result<Option<Unit>, BoardError> {
            self.units.try_claim_next(&self.diag, &self.clock)
        }

        /// One `poll` as the plugin answers it: a board fault is diagnosed and the beat
        /// idle — the built-in's swallowed poll.
        fn poll(&self) -> Option<Unit> {
            self.try_claim().unwrap_or_else(|e| {
                self.diag.err(&e);
                None
            })
        }

        /// One poll, returning the claimed card's id (if any).
        fn poll_claim(&self) -> Option<String> {
            self.poll().map(|u| u.card.id)
        }

        /// Run a claimed unit as afkd's spine does: up to `max` attempts, each scripted by
        /// `attempt` over its own fresh scratch directory; `classify` after each, and
        /// `attempt_failed` after each one that failed; then `finish` with the last
        /// attempt's outcome and facts.
        fn run(&self, unit: &Unit, max: u32, mut attempt: impl FnMut(&Path) -> Facts) -> Run {
            let (mut outcome, mut facts, mut runs) = (UnitOutcome::Failed, Facts::none(), 0);
            for n in 1..=max {
                let scratch = TempDir::new("attempt");
                facts = attempt(scratch.path());
                runs += 1;
                outcome = TrelloUnits::classify(scratch.path(), verdict(&facts));
                if outcome != UnitOutcome::Failed {
                    break;
                }
                self.units
                    .attempt_failed(unit, n, max, facts.fault(), &self.diag);
            }
            let delivered = self.units.finish(unit, outcome, &facts, &self.diag);
            Run {
                outcome,
                delivered,
                runs,
            }
        }

        /// Run `unit` through attempts whose facts are `script`, in order.
        fn run_script(&self, unit: &Unit, script: &[Facts]) -> Run {
            let mut next = script.iter().cloned();
            self.run(unit, script.len() as u32, |_| {
                next.next().unwrap_or_else(Facts::none)
            })
        }

        /// One beat as afkd's drive runs it: a poll, and the claimed unit's run through
        /// `max` attempts. The journal key a `held` finish leaves behind, if any.
        fn beat(&self, max: u32, attempt: impl FnMut(&Path) -> Facts) -> Option<String> {
            let unit = self.poll()?;
            let run = self.run(&unit, max, attempt);
            (!run.delivered).then(|| card_key(&unit.card.id, &unit.claim_id))
        }

        /// afkd's `release` of one journal key.
        fn release(&self, key: &str) -> Option<bool> {
            self.units.release_stale(key, &self.diag)
        }
    }

    /// An attempt standing in for the skill's `ask.py`: it posts the question on the card
    /// and *then* writes the [`PARK_FILE`] marker into the attempt's own scratch dir —
    /// the helper's own order — and reports `facts`.
    fn ask(board: &Arc<MockBoard>, card: &str, facts: Facts) -> impl FnMut(&Path) -> Facts {
        let (board, card) = (Arc::clone(board), card.to_string());
        move |scratch| {
            board
                .post_comment(&card, PARK_QUESTION)
                .expect("the question posts before anything parks");
            std::fs::write(scratch.join(PARK_FILE), b"").unwrap();
            facts.clone()
        }
    }

    // --- Pure helpers ---

    fn card(id: &str, title: &str, description: &str) -> Card {
        Card {
            id: id.into(),
            short_link: format!("sl-{id}"),
            title: title.into(),
            description: description.into(),
            checklists: Vec::new(),
            members: Vec::new(),
            labels: Vec::new(),
            created_at: Some(UNIX_EPOCH),
            comments: None,
        }
    }

    #[test]
    fn task_text_is_title_or_title_blank_line_body() {
        assert_eq!(task_text(&card("1", "Fix", "  ")), "Fix");
        assert_eq!(task_text(&card("1", "Fix", "do x")), "Fix\n\ndo x");
    }

    #[test]
    fn sanitize_folds_unsafe_chars_and_falls_back_on_empty() {
        assert_eq!(sanitize("Card_9-x"), "Card_9-x");
        assert_eq!(sanitize("a/b c!"), "a-b-c-");
        assert_eq!(sanitize(""), "card");
    }

    /// afkd's own instance rule: the suffix is read off the last `+`, both halves must be
    /// non-empty, and the tail must be ASCII digits.
    #[test]
    fn instance_base_strips_only_an_instance_suffix() {
        assert_eq!(instance_base("afkd::develop+1"), Some("afkd::develop"));
        assert_eq!(instance_base("afkd::develop+12"), Some("afkd::develop"));
        assert_eq!(instance_base("a+b+0"), Some("a+b"));
        for name in [
            "afkd::develop",
            "worker+x",
            "worker+",
            "+0",
            "worker+1x",
            "監視+１",
        ] {
            assert_eq!(instance_base(name), None, "{name}");
        }
    }

    fn comment(id: &str, text: &str, secs: u64) -> Comment {
        Comment {
            id: id.into(),
            text: text.into(),
            // The two halves kept deliberately distinct: `author` is the member id the
            // claim/`discuss_with` paths key on, `author_name` the display name the brief
            // attributes with.
            author: "x".into(),
            author_name: "Dana Rivera".into(),
            posted_at: UNIX_EPOCH + Duration::from_secs(secs),
            renewed_at: UNIX_EPOCH + Duration::from_secs(secs),
        }
    }

    /// An `[afkd-ran]` watermark whose delivery boundary is `upto_secs` and which was
    /// itself posted at `posted_secs`.
    fn ran(id: &str, upto_secs: u64, posted_secs: u64) -> Comment {
        comment(
            id,
            &ran_text("me", UNIX_EPOCH + Duration::from_secs(upto_secs)),
            posted_secs,
        )
    }

    /// A comment afkd itself posted carrying NO marker, under a human-looking display
    /// name, so a test that passes can only have keyed on the member id.
    fn self_comment(id: &str, text: &str, secs: u64) -> Comment {
        Comment {
            author: SELF_ID.into(),
            author_name: "Robin Vale".into(),
            ..comment(id, text, secs)
        }
    }

    fn texts(comments: &[Comment]) -> Vec<&str> {
        comments.iter().map(|c| c.text.as_str()).collect()
    }

    // --- Feedback delta + brief (pure helpers, no seam) ---

    #[test]
    fn feedback_first_run_includes_all_human_comments() {
        let comments = [
            comment("h1", "please also fix the typo", 100),
            comment("h2", "and add a test", 200),
        ];
        assert_eq!(
            texts(&feedback_comments(&comments, SELF_ID)),
            ["please also fix the typo", "and add a test"]
        );
    }

    #[test]
    fn feedback_rerun_with_no_new_comment_is_empty() {
        let comments = [comment("h1", "do the thing", 100), ran("ran1", 100, 200)];
        assert!(feedback_comments(&comments, SELF_ID).is_empty());
    }

    #[test]
    fn feedback_keeps_only_comments_after_the_watermark() {
        let comments = [
            comment("h1", "old note", 100),
            ran("ran1", 100, 200),
            comment("h2", "new note", 300),
        ];
        assert_eq!(texts(&feedback_comments(&comments, SELF_ID)), ["new note"]);
    }

    #[test]
    fn feedback_drops_afkd_own_comments() {
        // The claim/attempt/ran control markers are afkd-originated by their TEXT alone,
        // and are never echoed back into the delta, even when newer than the watermark —
        // including a claim the fire holding the card has **renewed**.
        assert!(is_control_marker(&claim_text("me")));
        assert!(is_control_marker(&claim_renewal_text("me", 132)));
        assert!(is_control_marker(&attempt_text(1, 3, "boom")));
        assert!(is_control_marker(&ran_text("me", UNIX_EPOCH)));
        assert!(is_control_marker(&park_text(TEST_SERVICE)));
        assert!(!is_control_marker("a human comment"));
        assert!(is_ran(&ran_text("me", UNIX_EPOCH)));
        assert!(!is_ran(&claim_text("me")));

        let comments = [
            ran("ran0", 50, 100),
            comment("claim1", &claim_text("me"), 200),
            comment("att1", &attempt_text(1, 3, "boom"), 300),
            comment("h1", "real feedback", 400),
            comment("claim2", &claim_renewal_text("me", 132), 500),
        ];
        assert_eq!(
            texts(&feedback_comments(&comments, SELF_ID)),
            ["real feedback"]
        );
    }

    #[test]
    fn a_legacy_note_prefixed_comment_now_reads_as_human() {
        // The dead `[afkd-note]` prefix is inert text (ADR-0069): a comment still
        // carrying it is whoever authored it, and the brief renders the prefix verbatim.
        let card = card("1", "Fix", "do x");
        let body = "[afkd-note] here is my read:\n\n```\nboundary = max(watermark, last reply)\n```\n\n— 陳 prefers approach B 🙂  \n";
        let comments = [ran("ran0", 100, 100), comment("n1", body, 150)];
        assert_eq!(texts(&feedback_comments(&comments, SELF_ID)), [body]);

        let brief = brief_text(&card, &comments, SELF_ID);
        assert_eq!(
            brief,
            "Fix\n\ndo x\n\n## New comments\n\
             \n**Dana Rivera:** [afkd-note] here is my read:\
             \n\n```\nboundary = max(watermark, last reply)\n```\
             \n\n— 陳 prefers approach B 🙂\n"
        );
        assert!(!brief.contains("**afkd:**"), "not afkd's own: {brief:?}");
    }

    #[test]
    fn an_action_with_no_member_creator_keys_afkd_on_the_control_marker() {
        // A board action carrying no `idMemberCreator` leaves `self_author` empty, so the
        // control marker is the only thing that says "afkd wrote this".
        let claim = comment("claim1", &claim_text("me"), 100);
        let attempt = comment("att1", &attempt_text(1, 3, "boom"), 110);
        let watermark = ran("ran1", 120, 120);
        let human = comment("h1", "one more thing before you start", 300);
        for marked in [&claim, &attempt, &watermark] {
            assert_ne!(marked.author, SELF_ID, "the author is not evidence");
            assert!(is_afkd(marked, ""), "marker is the evidence: {marked:?}");
        }
        assert!(!is_afkd(&human, ""), "a human comment is not afkd's own");

        let comments = [claim, attempt, watermark, human];
        assert_eq!(
            texts(&feedback_comments(&comments, "")),
            ["one more thing before you start"]
        );
    }

    #[test]
    fn feedback_drops_an_unmarked_self_authored_comment() {
        let comments = [
            comment("h1", "two questions, then: what is the boundary?", 100),
            self_comment("self1", "q2 restated, plainly: the watermark wins", 150),
            comment("h2", "got it — go ahead", 200),
        ];
        assert_eq!(
            texts(&feedback_comments(&comments, SELF_ID)),
            ["got it — go ahead"]
        );
        let settled = [comments[0].clone(), comments[1].clone()];
        assert!(feedback_comments(&settled, SELF_ID).is_empty());
    }

    #[test]
    fn feedback_no_watermark_boundary_ignores_this_runs_own_control_markers() {
        // With no `[afkd-ran]` the boundary is afkd's newest CONVERSATIONAL comment; this
        // run's own claim and attempt marker must not move it.
        let comments = [
            comment("h1", "grooming question", 100),
            self_comment("self1", "grooming answer", 150),
            comment("h2", "promoted, one tweak", 200),
            Comment {
                author: SELF_ID.into(),
                ..comment("claim1", &claim_text("me"), 1000)
            },
            Comment {
                author: SELF_ID.into(),
                ..comment("att1", &attempt_text(1, 3, "boom"), 1001)
            },
        ];
        assert_eq!(
            texts(&feedback_comments(&comments, SELF_ID)),
            ["promoted, one tweak"]
        );
        let settled = [
            comments[0].clone(),
            comments[1].clone(),
            comments[3].clone(),
        ];
        assert!(feedback_comments(&settled, SELF_ID).is_empty());
    }

    #[test]
    fn feedback_uses_only_prior_ran_marker() {
        let comments = [
            ran("ran1", 100, 100),
            comment("h1", "added after the last run", 200),
        ];
        assert_eq!(
            texts(&feedback_comments(&comments, SELF_ID)),
            ["added after the last run"]
        );
    }

    #[test]
    fn feedback_orders_comments_oldest_first_regardless_of_input_order() {
        // The real board returns comment actions newest-first.
        let comments = [
            comment("h2", "second note", 200),
            comment("h1", "first note", 100),
        ];
        assert_eq!(
            texts(&feedback_comments(&comments, SELF_ID)),
            ["first note", "second note"]
        );
    }

    #[test]
    fn brief_no_watermark_puts_grooming_qa_in_earlier() {
        let card = card("1", "Fix", "do x");
        let comments = [
            comment("h1", "please use approach B", 100),
            self_comment("self1", "sounds good, going with B", 150),
            comment("claim1", &claim_text("me"), 180),
            comment("h2", "one more thing before you start", 200),
        ];
        let brief = brief_text(&card, &comments, SELF_ID);
        let earlier = brief
            .find("## Earlier conversation")
            .expect("prior heading");
        let new = brief.find("## New comments").expect("new heading");
        assert!(earlier < new, "earlier precedes new: {brief:?}");
        let ask_at = brief.find("please use approach B").expect("ask shown");
        let reply_at = brief
            .find("sounds good, going with B")
            .expect("reply shown");
        assert!(
            earlier < ask_at && ask_at < new && earlier < reply_at && reply_at < new,
            "grooming Q&A is earlier: {brief:?}"
        );
        let fresh_at = brief
            .find("one more thing before you start")
            .expect("post-reply human shown");
        assert!(new < fresh_at, "post-reply human is new: {brief:?}");
        assert!(!brief.contains(CLAIM_MARKER), "no claim marker: {brief:?}");
    }

    #[test]
    fn watermark_records_delivered_line_so_mid_run_comments_are_not_lost() {
        let read_set = [ran("ran0", 100, 100), comment("h1", "delivered now", 150)];
        let feedback = feedback_comments(&read_set, SELF_ID);
        let boundary = delivered_upto(&read_set, &feedback);
        assert_eq!(boundary, UNIX_EPOCH + Duration::from_secs(150));

        // Run A posts its watermark at run *end* (t=400) carrying upto=150; a human
        // comment that landed mid-run at t=300 must reach the next run.
        let next_read = [
            ran("ranA", 150, 400),
            comment("h2", "added while A was running", 300),
        ];
        assert_eq!(
            texts(&feedback_comments(&next_read, SELF_ID)),
            ["added while A was running"]
        );
    }

    #[test]
    fn an_unparseable_ran_marker_contributes_no_watermark() {
        let comments = [
            comment("h1", "old", 100),
            comment("ranX", &format!("{RAN_MARKER} owner=afkd-1 upto="), 200),
            comment("h2", "new", 300),
        ];
        assert_eq!(watermark_of(&comments), None, "no boundary to read");
        assert_eq!(
            texts(&feedback_comments(&comments, SELF_ID)),
            ["old", "new"]
        );

        let comments = [
            comment("h1", "old", 100),
            self_comment("self1", "reply", 150),
            comment("hmid", "mid", 180),
            comment("ranY", &format!("{RAN_MARKER} owner=afkd-1"), 200),
            comment("h2", "new", 300),
        ];
        assert_eq!(watermark_of(&comments), None, "no boundary to read");
        assert_eq!(
            texts(&feedback_comments(&comments, SELF_ID)),
            ["mid", "new"]
        );
    }

    #[test]
    fn brief_text_appends_feedback_after_description() {
        let card = card("1", "Fix", "do x");
        assert_eq!(brief_text(&card, &[], SELF_ID), task_text(&card));
        let comments = [
            comment("h1", "also handle empty input", 100),
            comment("h2", "rename the flag", 200),
        ];
        assert_eq!(
            brief_text(&card, &comments, SELF_ID),
            "Fix\n\ndo x\n\n## New comments\n\n**Dana Rivera:** also handle empty input\n\n**Dana Rivera:** rename the flag\n"
        );
    }

    #[test]
    fn brief_text_renders_checklists_section() {
        let board = MockBoard::new();
        board.add_list("src");
        board.add_card("src", "c1", "Fix", "do x");
        board.seed_checklist(
            "c1",
            "Acceptance",
            &[("i1", "done thing", true), ("i2", "todo thing", false)],
        );
        board.seed_checklist("c1", "Empty", &[]);
        let card = board
            .list_cards("src")
            .unwrap()
            .pop()
            .expect("the seeded card");

        let comments = [comment("h1", "a note", 100)];
        let brief = brief_text(&card, &comments, SELF_ID);

        let section = "## Checklists\n\n### Acceptance (1/2)\n- [x] done thing · id: i1\n- [ ] todo thing · id: i2\n\n### Empty (0/0)";
        assert!(brief.contains(section), "section shape: {brief:?}");
        let desc = brief.find("do x").expect("description shown");
        let checklists = brief.find("## Checklists").expect("checklist heading");
        let comments_at = brief.find("## New comments").expect("comments heading");
        assert!(
            desc < checklists && checklists < comments_at,
            "checklists sit between the description and the thread: {brief:?}"
        );
    }

    #[test]
    fn brief_text_without_checklists_is_unchanged() {
        let card = card("1", "Fix", "do x");
        let comments = [
            comment("h1", "also handle empty input", 100),
            comment("h2", "rename the flag", 200),
        ];
        let brief = brief_text(&card, &comments, SELF_ID);
        assert_eq!(
            brief,
            "Fix\n\ndo x\n\n## New comments\n\n**Dana Rivera:** also handle empty input\n\n**Dana Rivera:** rename the flag\n"
        );
        assert!(!brief.contains("## Checklists"), "{brief:?}");
        assert_eq!(brief_text(&card, &[], SELF_ID), task_text(&card));
    }

    #[test]
    fn brief_text_shows_prior_and_new() {
        let card = card("1", "Fix", "do x");
        let comments = [
            comment("h1", "old note", 100),
            ran("ran1", 100, 200),
            comment("h2", "new note", 300),
            self_comment("park1", &park_text("afkd::develop"), 310),
        ];
        let brief = brief_text(&card, &comments, SELF_ID);
        let earlier = brief
            .find("## Earlier conversation")
            .expect("prior heading");
        let new = brief.find("## New comments").expect("new heading");
        assert!(earlier < new, "earlier precedes new: {brief:?}");
        let old_at = brief.find("old note").expect("old note shown");
        let new_at = brief.find("new note").expect("new note shown");
        assert!(
            earlier < old_at && old_at < new,
            "old note is prior: {brief:?}"
        );
        assert!(new < new_at, "new note is under the new heading: {brief:?}");
        assert!(!brief.contains(RAN_MARKER), "no raw ran marker: {brief:?}");
        assert!(
            !brief.contains(PARK_MARKER),
            "no raw park marker: {brief:?}"
        );
    }

    #[test]
    fn brief_labels_an_unmarked_self_authored_comment_as_afkd() {
        let card = card("1", "Fix", "do x");
        let comments = [
            comment("h1", "please use approach B", 100),
            self_comment(
                "self1",
                "q2 restated, plainly:\n\n```\nboundary = max(watermark, last reply)\n```\n\n— 陳 asked the same 🙂  \n",
                150,
            ),
            comment("h2", "ship it, then", 200),
        ];
        let brief = brief_text(&card, &comments, SELF_ID);
        assert_eq!(
            brief,
            "Fix\n\ndo x\n\n## Earlier conversation\n\
             \n**Dana Rivera:** please use approach B\n\
             \n**afkd:** q2 restated, plainly:\
             \n\n```\nboundary = max(watermark, last reply)\n```\
             \n\n— 陳 asked the same 🙂\n\
             \n\n## New comments\n\
             \n**Dana Rivera:** ship it, then\n"
        );
        assert!(
            !brief.contains("Robin Vale"),
            "afkd is not named: {brief:?}"
        );
    }

    #[test]
    fn brief_drops_control_markers() {
        let card = card("1", "Fix", "do x");
        let comments = [
            comment("claim1", &claim_text("me"), 100),
            comment("att1", &attempt_text(1, 3, "boom"), 150),
            ran("ran1", 150, 200),
            comment("h1", "keep going", 300),
        ];
        let brief = brief_text(&card, &comments, SELF_ID);
        assert!(!brief.contains(CLAIM_MARKER), "no claim marker: {brief:?}");
        assert!(
            !brief.contains(ATTEMPT_MARKER),
            "no attempt marker: {brief:?}"
        );
        assert!(!brief.contains(RAN_MARKER), "no ran marker: {brief:?}");
        assert!(
            brief.contains("**Dana Rivera:** keep going"),
            "human shown: {brief:?}"
        );
    }

    #[test]
    fn new_set_stays_human_only_across_rounds() {
        let mut comments: Vec<Comment> = Vec::new();

        comments.push(comment("h1", "do the first thing", 100));
        assert_eq!(
            texts(&feedback_comments(&comments, SELF_ID)),
            ["do the first thing"]
        );
        comments.push(self_comment("n1", "first thing done", 150));
        comments.push(ran("ran1", 150, 160));

        comments.push(comment("h2", "now the second thing", 200));
        assert_eq!(
            texts(&feedback_comments(&comments, SELF_ID)),
            ["now the second thing"]
        );
        comments.push(self_comment("n2", "second thing done", 250));
        comments.push(ran("ran2", 250, 260));

        comments.push(comment("h3", "finally the third thing", 300));
        assert_eq!(
            texts(&feedback_comments(&comments, SELF_ID)),
            ["finally the third thing"]
        );

        let brief = brief_text(&card("1", "Fix", "do x"), &comments, SELF_ID);
        assert!(brief.contains("**afkd:** first thing done"), "{brief:?}");
        assert!(brief.contains("**afkd:** second thing done"), "{brief:?}");
        let new = brief.find("## New comments").expect("new heading");
        let third = brief.find("finally the third thing").expect("third shown");
        assert!(new < third, "the last human comment is new: {brief:?}");
        assert!(!brief.contains(RAN_MARKER), "no raw ran marker: {brief:?}");
        assert!(
            !brief.contains(CLAIM_MARKER),
            "no raw claim marker: {brief:?}"
        );
    }

    // --- Claim protocol ---

    fn seed_source_card(board: &MockBoard, id: &str) {
        board.add_list("Up for Grabs");
        board.add_list("In Progress");
        board.add_list("Review");
        board.add_card("Up for Grabs", id, "Title", "Body");
    }

    fn has_move_to(board: &MockBoard, to: &str) -> bool {
        board
            .actions()
            .iter()
            .any(|a| matches!(a, Action::Move { list, .. } if list == to))
    }

    #[test]
    fn claim_wins_runs_on_claim() {
        let h = Harness::new(cfg(vec![move_to("In Progress")], vec![], vec![]), "me");
        seed_source_card(&h.board, "card1");
        h.board.set_clock(1000);

        let unit = h.poll().expect("claimed");
        assert_eq!(unit.card.id, "card1");
        assert!(!unit.claim_id.is_empty());
        assert!(has_move_to(&h.board, "In Progress"));
        // The race is post → settle → re-read, on the clock it was handed.
        assert_eq!(h.clock.sleeps(), [CLAIM_SETTLE]);
    }

    // --- The `require_member` intake gate ---

    fn gated_cfg(who: MemberRef) -> BoardConfig {
        BoardConfig {
            require_member: Some(who),
            ..cfg(vec![], vec![], vec![])
        }
    }

    /// A source list whose head card carries no member and whose second card is
    /// `SELF_ID`'s — the head-of-line-blocking shape the gate has to scan past.
    fn seed_two_cards_second_is_ours(board: &MockBoard) {
        seed_source_card(board, "card1");
        board.add_card("Up for Grabs", "card2", "Second", "Body");
        board.seed_card_member("card2", SELF_ID);
    }

    #[test]
    fn require_member_skips_ineligible_head_and_claims_the_next() {
        let h = Harness::new(gated_cfg(MemberRef::SelfMember), "me");
        seed_two_cards_second_is_ours(&h.board);
        h.board.set_clock(1000);

        assert_eq!(h.poll_claim().as_deref(), Some("card2"));
        // The gate runs strictly before the claim is posted.
        assert!(h.board.comments_on("card1").is_empty());
    }

    #[test]
    fn require_member_with_no_eligible_card_claims_nothing() {
        let h = Harness::new(gated_cfg(MemberRef::SelfMember), "me");
        seed_source_card(&h.board, "card1");
        h.board.add_card("Up for Grabs", "card2", "Second", "Body");
        h.board.set_clock(1000);

        let claimed = h
            .try_claim()
            .expect("an empty eligible set is not an error");
        assert!(claimed.is_none());
        assert!(h.board.comments_on("card1").is_empty());
        assert!(h.board.comments_on("card2").is_empty());
    }

    #[test]
    fn without_require_member_the_head_card_is_claimed_and_no_member_resolved() {
        let h = Harness::new(cfg(vec![], vec![], vec![]), "me");
        seed_two_cards_second_is_ours(&h.board);
        h.board.set_clock(1000);

        assert_eq!(h.poll_claim().as_deref(), Some("card1"));
        assert_eq!(h.board.resolve_count(), 0);
    }

    #[test]
    fn require_member_resolves_once_per_poll() {
        let h = Harness::new(gated_cfg(MemberRef::SelfMember), "me");
        seed_two_cards_second_is_ours(&h.board);
        h.board.set_clock(1000);
        for _ in 0..3 {
            let _ = h.poll();
        }
        assert_eq!(h.board.resolve_count(), 3);
    }

    #[test]
    fn require_member_unknown_username_is_logged_and_claims_nothing() {
        let h = Harness::new(gated_cfg(MemberRef::Username("ghost".into())), "me");
        seed_source_card(&h.board, "card1");
        h.board.set_clock(1000);

        assert!(h.poll().is_none());
        assert!(h.poll().is_none());
        assert_eq!(
            h.diag.errs(),
            ["trello resolve member: no member named 'ghost'"; 2]
        );
        assert!(h.board.comments_on("card1").is_empty());
    }

    // --- The journal key and `release` (ADR-0059) ---

    #[test]
    fn card_key_round_trips_through_split() {
        assert_eq!(card_key("card1", "claim1"), "card1#claim1");
        assert_eq!(split_card_key("card1#claim1"), Some(("card1", "claim1")));
        assert_eq!(split_card_key("garbage"), None);
    }

    /// The key afkd journals is `card#claim`, both halves the reaper needs, and a clean
    /// finish is delivered — so afkd releases the entry rather than holding it.
    #[test]
    fn a_won_claim_is_keyed_by_card_and_claim_and_a_clean_finish_delivers() {
        let h = Harness::new(cfg(vec![], vec![], vec![]), "me");
        seed_source_card(&h.board, "card1");
        h.board.set_clock(1000);
        let unit = h.poll().expect("claimed");
        assert_eq!(h.units.wire_unit(&unit).key, "card1#c1000");
        assert!(h.run_script(&unit, &[proceed()]).delivered);
    }

    /// An exhausted run's `finish` is delivered too, so its entry is cleared rather than
    /// read as a crash victim on the next start.
    #[test]
    fn an_exhausted_run_is_delivered_too() {
        let h = Harness::new(cfg(vec![], vec![], vec![]), "me");
        seed_source_card(&h.board, "card1");
        h.board.set_clock(1000);
        let unit = h.poll().expect("claimed");
        let run = h.run_script(&unit, &[fault("boom")]);
        assert_eq!(run.outcome, UnitOutcome::Failed);
        assert!(run.delivered);
    }

    /// A unit that never reached `finish` — its run died, or afkd handed it straight back
    /// during a stop — has nothing recorded as owed, so its `release` takes the
    /// crash-victim arm: prune the claim, move the card back to the list the poll reads.
    #[test]
    fn a_unit_that_never_finished_is_reversed_by_release() {
        let h = Harness::new(cfg(vec![move_to("In Progress")], vec![], vec![]), "me");
        seed_source_card(&h.board, "card1");
        h.board.set_clock(1000);
        let unit = h.poll().expect("claimed");
        let before = h.board.actions().len();

        assert_eq!(
            h.release(&card_key(&unit.card.id, &unit.claim_id)),
            Some(true)
        );
        assert_eq!(
            h.board.actions()[before..],
            [
                Action::DeleteComment {
                    card: "card1".into(),
                    comment: "c1000".into(),
                },
                Action::Move {
                    card: "card1".into(),
                    list: "Up for Grabs".into(),
                    position: ListPosition::Bottom,
                },
            ],
        );
    }

    #[test]
    fn reaps_a_stranded_card_by_pruning_the_claim_and_moving_it_back() {
        // A previous run claimed `card1` (its `on_claim` moved it into "In Progress",
        // where this process never polls) and crashed.
        let h = Harness::new(cfg(vec![], vec![], vec![]), "me");
        h.board.add_list("Up for Grabs");
        h.board.add_list("In Progress");
        h.board.add_card("In Progress", "card1", "Title", "Body");
        h.board
            .seed_comment("card1", "claim1", &claim_text("me"), 500);

        assert_eq!(h.release("card1#claim1"), Some(true));

        assert_eq!(
            h.board.actions(),
            vec![
                Action::DeleteComment {
                    card: "card1".into(),
                    comment: "claim1".into(),
                },
                Action::Move {
                    card: "card1".into(),
                    list: "Up for Grabs".into(),
                    position: ListPosition::Bottom,
                },
            ],
        );
        assert_eq!(
            h.board
                .list_cards("Up for Grabs")
                .unwrap()
                .iter()
                .map(|c| c.id.clone())
                .collect::<Vec<_>>(),
            vec!["card1"],
        );
        assert!(h.board.comments_on("card1").is_empty());
    }

    /// The reversal is best-effort: a board failure is diagnosed and the key still
    /// released, so a persistently-failing board cannot loop the reap.
    #[test]
    fn a_reversal_the_board_refuses_is_logged_and_still_releases() {
        let h = Harness::new(cfg(vec![], vec![], vec![]), "me");
        seed_source_card(&h.board, "card1");
        h.board
            .seed_comment("card1", "claim1", &claim_text("me"), 500);
        h.board.fail("move card");

        assert_eq!(h.release("card1#claim1"), Some(true));
        assert_eq!(
            h.diag.errs(),
            ["trello move card: no response (mock failure)"]
        );
        assert!(
            h.board.comments_on("card1").is_empty(),
            "the claim still went"
        );
    }

    #[test]
    fn reap_drops_an_unparseable_victim_key_without_a_board_call() {
        let h = Harness::new(cfg(vec![], vec![], vec![]), "me");
        h.board.add_list("Up for Grabs");

        assert_eq!(h.release("garbage"), None);
        assert!(
            h.board.calls().is_empty(),
            "an unparseable key names no card"
        );
    }

    // --- An undelivered terminal lifecycle holds the claim (ADR-0059 amendment) ---

    /// The dead-board fixture: a card whose `on_fail` moment cannot reach the board — a
    /// move plus a label, both owed — driven through exactly one beat. The key the
    /// `held` finish leaves.
    fn a_run_whose_on_fail_cannot_land() -> (Harness, String) {
        let h = Harness::new(
            cfg(
                vec![],
                vec![],
                vec![move_to("Backlog"), add_label("Problem")],
            ),
            "me",
        );
        seed_source_card(&h.board, "card1");
        h.board.add_list("Backlog");
        h.board.set_clock(1000);
        h.board.fail("move card");
        let held = h
            .beat(1, |_| fault("cargo test: 3 failed"))
            .expect("the finish is held");
        (h, held)
    }

    #[test]
    fn an_undelivered_lifecycle_holds_the_claim_instead_of_releasing_it() {
        let (h, held) = a_run_whose_on_fail_cannot_land();
        assert_eq!(held, "card1#c1000");
        assert!(
            h.board
                .comments_on("card1")
                .iter()
                .any(|c| is_claim(&c.text)),
            "the lease is held while the lifecycle is owed: {:?}",
            h.board.comments_on("card1"),
        );
    }

    #[test]
    fn the_next_release_delivers_the_owed_on_fail_and_releases_the_key() {
        let (h, held) = a_run_whose_on_fail_cannot_land();
        h.board.clear_failure();

        assert_eq!(h.release(&held), Some(true));

        // `post_comment` records no `Action`, and a *failed* `move_card` records none
        // either. The `Move` is to "Backlog", the configured `on_fail` destination — not
        // the crash-victim reversal's move to "Up for Grabs".
        assert_eq!(
            h.board.actions(),
            vec![
                Action::Move {
                    card: "card1".into(),
                    list: "Backlog".into(),
                    position: ListPosition::Top,
                },
                Action::AddLabel {
                    card: "card1".into(),
                    label: "Problem".into(),
                },
                Action::DeleteComment {
                    card: "card1".into(),
                    comment: "c1000".into(),
                },
            ],
        );
        assert!(h
            .board
            .comments_on("card1")
            .iter()
            .all(|c| !is_claim(&c.text)));
    }

    #[test]
    fn a_release_only_failure_is_retried_with_no_card_move() {
        // The `afkd::discuss` shape: no lifecycle actions at all, so only the lease
        // release can fail — and the retry prunes the comment WITHOUT moving the card.
        let h = Harness::new(discuss_cfg(DiscussWith::Anyone), "me");
        seed_source_card(&h.board, "card1");
        h.board
            .seed_comment_by("card1", "h1", HUMAN_ASK, "human", 100);
        h.board.set_clock(1000);
        h.board.fail("delete comment");
        let held = h.beat(1, |_| proceed()).expect("held");
        assert_eq!(held, "card1#c1000");
        assert!(h
            .board
            .comments_on("card1")
            .iter()
            .any(|c| is_claim(&c.text)));

        h.board.clear_failure();
        assert_eq!(h.release(&held), Some(true));
        assert!(
            h.board
                .comments_on("card1")
                .iter()
                .all(|c| !is_claim(&c.text)),
            "the orphaned marker is pruned by the retry",
        );
        assert!(
            !h.board
                .actions()
                .iter()
                .any(|a| matches!(a, Action::Move { .. })),
            "a card with no lifecycle actions is never moved, saw {:?}",
            h.board.actions(),
        );
    }

    #[test]
    fn a_partial_lifecycle_replays_only_the_outstanding_actions() {
        // A three-action `on_done` whose middle action fails leaves the posted comment
        // alone and replays only the move and the label.
        let body = "shipped in @{run:duration} — @{run:cost}, @{run:turns} turns";
        let posted = "shipped in 5.00s — $1.50, 3 turns";
        let h = Harness::new(
            cfg(
                vec![],
                vec![
                    comment_action(body),
                    move_to("Review"),
                    add_label("Shipped"),
                ],
                vec![],
            ),
            "me",
        );
        seed_source_card(&h.board, "card1");
        h.board.set_clock(1000);
        h.board.fail("move card");
        let held = h
            .beat(1, |_| measured("260826-153328-card-nwYGQjnu-1"))
            .expect("held");

        let posted_count = |h: &Harness| {
            h.board
                .comments_on("card1")
                .iter()
                .filter(|c| c.text == posted)
                .count()
        };
        assert_eq!(
            posted_count(&h),
            1,
            "the first action landed during the run"
        );

        h.board.clear_failure();
        assert_eq!(h.release(&held), Some(true));

        assert_eq!(
            posted_count(&h),
            1,
            "a posted comment is never posted twice"
        );
        assert_eq!(
            h.board.actions(),
            vec![
                Action::Move {
                    card: "card1".into(),
                    list: "Review".into(),
                    position: ListPosition::Top,
                },
                Action::AddLabel {
                    card: "card1".into(),
                    label: "Shipped".into(),
                },
                Action::DeleteComment {
                    card: "card1".into(),
                    comment: "c1000".into(),
                },
            ],
            "only the outstanding suffix is replayed, then the lease",
        );
    }

    #[test]
    fn five_failed_deliveries_return_the_card_to_the_source_list() {
        // The fault is on `add label`, disjoint from the reversal's own calls, so the
        // give-up's promise ("returning it to …") is actually reachable.
        let h = Harness::new(cfg(vec![], vec![], vec![add_label("Problem")]), "me");
        seed_source_card(&h.board, "card1");
        h.board.set_clock(1000);
        h.board.fail("add label");
        let held = h.beat(1, |_| fault("boom")).expect("held");

        let verdicts: Vec<Option<bool>> = (0..PENDING_FINISH_TRIES)
            .map(|_| h.release(&held))
            .collect();
        assert_eq!(
            verdicts,
            [
                Some(false),
                Some(false),
                Some(false),
                Some(false),
                Some(true)
            ],
            "kept four beats, then handed back"
        );

        let give_ups: Vec<String> = h
            .diag
            .errs()
            .into_iter()
            .filter(|l| l.contains("giving up on the terminal lifecycle"))
            .collect();
        assert_eq!(
            give_ups,
            vec![
                "giving up on the terminal lifecycle for card \"Title\" after 5 tries; \
                 returning it to \"Up for Grabs\""
            ],
        );
        assert_eq!(
            h.board.actions().last(),
            Some(&Action::Move {
                card: "card1".into(),
                list: "Up for Grabs".into(),
                position: ListPosition::Bottom,
            }),
        );
        assert!(h
            .board
            .comments_on("card1")
            .iter()
            .all(|c| !is_claim(&c.text)));
    }

    /// `finish` reports an undelivered terminal state, and the board sees exactly the
    /// calls the failing lifecycle makes: the failed move, then the best-effort
    /// watermark — nothing else.
    #[test]
    fn an_undelivered_finish_adds_no_board_traffic() {
        let h = Harness::new(cfg(vec![], vec![move_to("Review")], vec![]), "me");
        seed_source_card(&h.board, "card1");
        h.board
            .seed_comment("card1", "myclaim", &claim_text("me"), 1);
        h.board.fail("move card");

        assert!(!h.units.finish(
            &claimed_unit("card1"),
            UnitOutcome::Clean,
            &Facts::none(),
            &h.diag,
        ));
        assert_eq!(
            h.board.calls(),
            vec!["resolve list", "move card", "post comment"]
        );
    }

    #[test]
    fn a_steady_state_poll_adds_no_board_traffic_per_beat() {
        // An idle poll costs three requests flat: the parked scan's whole-board read,
        // then the list resolve and the card read. Nothing is badged, so no card is read
        // in full and no member is resolved.
        let h = Harness::new(cfg(vec![], vec![], vec![]), "me");
        h.board.add_list("Up for Grabs");
        for _ in 0..4 {
            assert!(h.poll().is_none());
        }
        assert_eq!(
            h.board.calls(),
            ["board cards", "resolve list", "list cards"].repeat(4),
        );
        assert!(h.board.card_reads().is_empty());
    }

    // --- The `require_label` intake gate ---

    fn labelled_cfg(label: &str) -> BoardConfig {
        BoardConfig {
            require_label: Some(label.to_string()),
            ..cfg(vec![], vec![], vec![])
        }
    }

    #[test]
    fn require_label_claims_only_a_labelled_card() {
        let h = Harness::new(labelled_cfg("Redo"), "me");
        seed_source_card(&h.board, "card1");
        h.board.add_card("Up for Grabs", "card2", "Second", "Body");
        h.board.seed_card_label("card2", "Redo");
        h.board.set_clock(1000);

        assert_eq!(h.poll_claim().as_deref(), Some("card2"));
        assert!(h.board.comments_on("card1").is_empty());
    }

    #[test]
    fn require_label_with_no_labelled_card_claims_nothing() {
        let h = Harness::new(labelled_cfg("Redo"), "me");
        seed_source_card(&h.board, "card1");
        h.board.add_card("Up for Grabs", "card2", "Second", "Body");
        h.board.set_clock(1000);

        let claimed = h
            .try_claim()
            .expect("an empty eligible set is not an error");
        assert!(claimed.is_none());
        assert!(h.board.comments_on("card1").is_empty());
        assert!(h.board.comments_on("card2").is_empty());
    }

    #[test]
    fn require_label_and_require_member_both_must_hold() {
        let both_cfg = BoardConfig {
            require_member: Some(MemberRef::SelfMember),
            require_label: Some("Redo".into()),
            ..cfg(vec![], vec![], vec![])
        };
        let h = Harness::new(both_cfg, "me");
        seed_source_card(&h.board, "label-only");
        h.board.seed_card_label("label-only", "Redo");
        h.board.add_card("Up for Grabs", "member-only", "M", "Body");
        h.board.seed_card_member("member-only", SELF_ID);
        h.board.add_card("Up for Grabs", "both", "B", "Body");
        h.board.seed_card_member("both", SELF_ID);
        h.board.seed_card_label("both", "Redo");
        h.board.set_clock(1000);

        assert_eq!(h.poll_claim().as_deref(), Some("both"));
        assert!(h.board.comments_on("label-only").is_empty());
        assert!(h.board.comments_on("member-only").is_empty());
    }

    #[test]
    fn on_claim_remove_label_clears_the_gate_label() {
        let cfg = BoardConfig {
            require_label: Some("Redo".into()),
            ..cfg(vec![remove_label("Redo")], vec![], vec![])
        };
        let h = Harness::new(cfg, "me");
        seed_source_card(&h.board, "card1");
        h.board.seed_card_label("card1", "Redo");
        h.board.set_clock(1000);

        assert_eq!(h.poll_claim().as_deref(), Some("card1"));
        assert!(
            h.board.actions().iter().any(
                |a| matches!(a, Action::RemoveLabel { card, label } if card == "card1" && label == "Redo")
            ),
            "on_claim must remove the gate label, saw {:?}",
            h.board.actions(),
        );
        let cards = h.board.list_cards("Up for Grabs").unwrap();
        let card = cards
            .iter()
            .find(|c| c.id == "card1")
            .expect("card present");
        assert!(
            !card.labels.iter().any(|l| l == "Redo"),
            "{:?}",
            card.labels
        );
    }

    // --- The `without_label` negative intake gate ---

    #[test]
    fn without_label_skips_a_carrying_card() {
        let cfg = BoardConfig {
            without_label: vec!["Hold".into()],
            ..cfg(vec![], vec![], vec![])
        };
        let h = Harness::new(cfg, "me");
        seed_source_card(&h.board, "card1");
        h.board.seed_card_label("card1", "Hold");
        h.board.add_card("Up for Grabs", "card2", "Second", "Body");
        h.board.set_clock(1000);

        assert_eq!(h.poll_claim().as_deref(), Some("card2"));
        assert!(h.board.comments_on("card1").is_empty());
    }

    #[test]
    fn without_label_wins_over_require_label() {
        let cfg = BoardConfig {
            require_label: Some("Redo".into()),
            without_label: vec!["Hold".into()],
            ..cfg(vec![], vec![], vec![])
        };
        let h = Harness::new(cfg, "me");
        seed_source_card(&h.board, "both");
        h.board.seed_card_label("both", "Redo");
        h.board.seed_card_label("both", "Hold");
        h.board.add_card("Up for Grabs", "redo-only", "R", "Body");
        h.board.seed_card_label("redo-only", "Redo");
        h.board.set_clock(1000);

        assert_eq!(h.poll_claim().as_deref(), Some("redo-only"));
        assert!(h.board.comments_on("both").is_empty());
    }

    #[test]
    fn without_label_excludes_any_of_many() {
        let cfg = BoardConfig {
            without_label: vec!["Hold".into(), "WIP".into()],
            ..cfg(vec![], vec![], vec![])
        };
        let h = Harness::new(cfg, "me");
        seed_source_card(&h.board, "wip");
        h.board.seed_card_label("wip", "WIP");
        h.board.add_card("Up for Grabs", "clean", "C", "Body");
        h.board.set_clock(1000);

        assert_eq!(h.poll_claim().as_deref(), Some("clean"));
        assert!(h.board.comments_on("wip").is_empty());
    }

    // --- The `min_age` age intake gate ---

    fn min_age_cfg(min_age: Duration) -> BoardConfig {
        BoardConfig {
            min_age,
            ..cfg(vec![], vec![], vec![])
        }
    }

    /// A creation time `secs` seconds before real now. The gate reads the host clock
    /// directly, so the tests date their *cards* against that same clock.
    fn born_ago(secs: u64) -> Option<SystemTime> {
        Some(SystemTime::now() - Duration::from_secs(secs))
    }

    /// The epoch second `ago` seconds before real now — a *comment* post time dated
    /// against the host clock, since [`has_live_claim`] reads [`SystemTime::now`].
    fn live_secs(ago: u64) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - ago
    }

    const TEN_MINUTES: Duration = Duration::from_secs(600);

    #[test]
    fn min_age_unset_reads_no_creation_time() {
        let h = Harness::new(cfg(vec![], vec![], vec![]), "me");
        seed_source_card(&h.board, "card1");
        h.board.seed_card_created_at("card1", None);
        h.board.seed_card_created_action("card1", SystemTime::now());
        h.board.set_clock(1000);

        assert_eq!(h.poll_claim().as_deref(), Some("card1"));
        assert_eq!(h.board.created_at_calls(), 0);
    }

    #[test]
    fn a_card_younger_than_min_age_is_not_claimed_or_written_to() {
        let cfg = BoardConfig {
            min_age: TEN_MINUTES,
            ..cfg(vec![move_to("In Progress")], vec![], vec![])
        };
        let h = Harness::new(cfg, "me");
        seed_source_card(&h.board, "card1");
        h.board.seed_card_created_at("card1", born_ago(60));
        h.board.set_clock(1000);

        let claimed = h
            .try_claim()
            .expect("a card that is merely young is not an error");
        assert!(claimed.is_none());
        assert!(h.board.comments_on("card1").is_empty(), "no claim comment");
        assert_eq!(h.board.actions(), vec![], "no lifecycle action");
        assert_eq!(h.board.created_at_calls(), 0);

        h.board.seed_card_created_at("card1", born_ago(660));
        assert_eq!(h.poll_claim().as_deref(), Some("card1"));
        assert!(has_move_to(&h.board, "In Progress"));
    }

    #[test]
    fn a_card_dated_in_the_future_is_treated_as_newborn() {
        let h = Harness::new(min_age_cfg(TEN_MINUTES), "me");
        seed_source_card(&h.board, "card1");
        h.board
            .seed_card_created_at("card1", Some(SystemTime::now() + Duration::from_secs(3600)));
        h.board.set_clock(1000);

        assert!(h.try_claim().expect("not an error").is_none());
        assert!(h.board.comments_on("card1").is_empty());
    }

    #[test]
    fn a_young_head_card_does_not_block_an_older_one() {
        let h = Harness::new(min_age_cfg(TEN_MINUTES), "me");
        seed_source_card(&h.board, "fresh");
        h.board
            .add_card("Up for Grabs", "settled", "Second", "Body");
        h.board.seed_card_created_at("fresh", born_ago(5));
        h.board.seed_card_created_at("settled", born_ago(3600));
        h.board.set_clock(1000);

        assert_eq!(h.poll_claim().as_deref(), Some("settled"));
        assert!(h.board.comments_on("fresh").is_empty());
    }

    #[test]
    fn an_old_card_with_a_fresh_comment_is_still_claimed() {
        let h = Harness::new(
            BoardConfig {
                min_age: TEN_MINUTES,
                ..discuss_cfg(DiscussWith::Anyone)
            },
            "me",
        );
        seed_source_card(&h.board, "card1");
        h.board.seed_card_created_at("card1", born_ago(3600));
        let now_secs = live_secs(0);
        h.board
            .seed_comment_by("card1", "a1", "answered", SELF_ID, now_secs - 10);
        h.board
            .seed_comment_by("card1", "h1", "one more thing", "human", now_secs - 2);
        h.board.set_clock(now_secs);

        assert_eq!(h.poll_claim().as_deref(), Some("card1"));
    }

    #[test]
    fn min_age_composes_with_the_other_gates() {
        let cfg = BoardConfig {
            require_member: Some(MemberRef::SelfMember),
            require_label: Some("ready".into()),
            without_label: vec!["blocked".into()],
            min_age: TEN_MINUTES,
            ..cfg(vec![], vec![], vec![])
        };
        let h = Harness::new(cfg, "me");
        h.board.add_list("Up for Grabs");
        for (id, age, labels, assigned) in [
            ("young", 30, vec!["ready"], true),
            ("unlabelled", 3600, vec![], true),
            ("denied", 3600, vec!["ready", "blocked"], true),
            ("unassigned", 3600, vec!["ready"], false),
            ("eligible", 3600, vec!["ready"], true),
        ] {
            h.board.add_card("Up for Grabs", id, "Title", "Body");
            h.board.seed_card_created_at(id, born_ago(age));
            for label in labels {
                h.board.seed_card_label(id, label);
            }
            if assigned {
                h.board.seed_card_member(id, SELF_ID);
            }
        }
        h.board.set_clock(1000);

        assert_eq!(h.poll_claim().as_deref(), Some("eligible"));
        for skipped in ["young", "unlabelled", "denied", "unassigned"] {
            assert!(h.board.comments_on(skipped).is_empty(), "{skipped}");
        }
    }

    #[test]
    fn a_card_held_back_by_age_costs_no_comment_read() {
        let h = Harness::new(
            BoardConfig {
                min_age: TEN_MINUTES,
                ..discuss_cfg(DiscussWith::Anyone)
            },
            "me",
        );
        seed_source_card(&h.board, "card1");
        h.board.seed_card_created_at("card1", born_ago(60));
        h.board.set_clock(1000);
        h.board.fail("read comments");

        let claimed = h.try_claim().expect("the comment read must never happen");
        assert!(claimed.is_none());
    }

    #[test]
    fn an_undated_card_is_judged_by_asking_the_board() {
        for (age, claimed) in [(60, false), (660, true)] {
            let h = Harness::new(min_age_cfg(TEN_MINUTES), "me");
            seed_source_card(&h.board, "card1");
            h.board.seed_card_created_at("card1", None);
            h.board
                .seed_card_created_action("card1", SystemTime::now() - Duration::from_secs(age));
            h.board.set_clock(1000);

            let picked = h.try_claim().expect("not an error").is_some();
            assert_eq!(picked, claimed, "a {age}s-old card");
            assert_eq!(h.board.created_at_calls(), 1, "asked once, for that card");
        }
    }

    #[test]
    fn a_failed_creation_read_surfaces_as_a_board_error() {
        let h = Harness::new(min_age_cfg(TEN_MINUTES), "me");
        seed_source_card(&h.board, "card1");
        h.board.seed_card_created_at("card1", None);
        h.board.fail("read card created");

        let Err(err) = h.try_claim() else {
            panic!("a board fault is an error, not an eligible card");
        };
        assert_eq!(err.stage(), "read card created");
        assert!(h.poll().is_none());
        assert_eq!(
            h.diag.errs(),
            ["trello read card created: no response (mock failure)"]
        );
        assert!(h.board.comments_on("card1").is_empty());
    }

    // --- The `discuss_with` grooming gate ---

    fn discuss_cfg(dw: DiscussWith) -> BoardConfig {
        BoardConfig {
            discuss_with: Some(dw),
            ..cfg(vec![], vec![], vec![])
        }
    }

    fn alice_only() -> DiscussWith {
        DiscussWith::Members(vec![MemberRef::Username("alice".into())])
    }

    #[test]
    fn discuss_anyone_fires_on_first_sight() {
        let h = Harness::new(discuss_cfg(DiscussWith::Anyone), "me");
        seed_source_card(&h.board, "card1");
        h.board
            .seed_comment_by("card1", "h1", "please groom this", "human", 100);
        h.board.set_clock(1000);

        assert_eq!(h.poll_claim().as_deref(), Some("card1"));
    }

    #[test]
    fn discuss_anyone_stays_quiet_after_afkd_then_refires() {
        let h = Harness::new(discuss_cfg(DiscussWith::Anyone), "me");
        seed_source_card(&h.board, "card1");
        h.board
            .seed_comment_by("card1", "h1", "first ask", "human", 100);
        h.board
            .seed_comment_by("card1", "a1", "answered", SELF_ID, 200);
        h.board.set_clock(1000);

        assert!(h.poll().is_none(), "quiet poll 1");
        assert!(h.poll().is_none(), "quiet poll 2");
        h.board
            .seed_comment_by("card1", "h2", "one more thing", "human", 300);
        assert_eq!(h.poll_claim().as_deref(), Some("card1"), "refires");
    }

    #[test]
    fn discuss_anyone_collapses_multiple_human_comments() {
        let h = Harness::new(discuss_cfg(DiscussWith::Anyone), "me");
        seed_source_card(&h.board, "card1");
        h.board
            .seed_comment_by("card1", "h1", "point one", "human", 100);
        h.board
            .seed_comment_by("card1", "h2", "point two", "human", 200);
        h.board.set_clock(1000);

        assert_eq!(h.poll_claim().as_deref(), Some("card1"), "claims once");
        // The claim afkd just posted is not speech, and it is stale to the wall clock;
        // what holds the second poll off is losing the race to it.
        assert!(h.poll().is_none(), "both human comments were the one turn");
    }

    #[test]
    fn discuss_whitelist_only_named_author_fires_after_first_sight() {
        let harness = || {
            let h = Harness::new(discuss_cfg(alice_only()), "me");
            h.board.seed_board_member("alice", "ma");
            h.board.seed_board_member("bob", "mb");
            seed_source_card(&h.board, "card1");
            h.board
                .seed_comment_by("card1", "self1", "groomed", SELF_ID, 50);
            h.board.set_clock(1000);
            h
        };

        let bob = harness();
        bob.board
            .seed_comment_by("card1", "b1", "bob says", "mb", 100);
        assert!(bob.poll().is_none(), "a disallowed author does not fire");

        let alice = harness();
        alice
            .board
            .seed_comment_by("card1", "a1", "alice says", "ma", 100);
        assert_eq!(alice.poll_claim().as_deref(), Some("card1"));

        let both = harness();
        both.board
            .seed_comment_by("card1", "a1", "alice says", "ma", 100);
        both.board
            .seed_comment_by("card1", "b1", "bob says", "mb", 200);
        assert_eq!(
            both.poll_claim().as_deref(),
            Some("card1"),
            "bob-after-alice still fires"
        );
    }

    #[test]
    fn discuss_fires_on_first_sight_with_zero_comments_for_anyone_and_a_named_list() {
        for dw in [DiscussWith::Anyone, alice_only()] {
            let h = Harness::new(discuss_cfg(dw.clone()), "me");
            h.board.seed_board_member("alice", "ma");
            seed_source_card(&h.board, "card1");
            h.board.set_clock(1000);
            assert_eq!(h.poll_claim().as_deref(), Some("card1"), "{dw:?}");
        }
    }

    #[test]
    fn discuss_named_list_fires_on_first_sight_despite_disallowed_comment() {
        let h = Harness::new(discuss_cfg(alice_only()), "me");
        h.board.seed_board_member("alice", "ma");
        h.board.seed_board_member("bob", "mb");
        seed_source_card(&h.board, "card1");
        h.board
            .seed_comment_by("card1", "b1", "bob says", "mb", 100);
        h.board.set_clock(1000);

        assert_eq!(h.poll_claim().as_deref(), Some("card1"));
    }

    #[test]
    fn discuss_self_authored_comment_does_not_refire() {
        let h = Harness::new(discuss_cfg(DiscussWith::Anyone), "me");
        seed_source_card(&h.board, "card1");
        h.board
            .seed_comment_by("card1", "h1", "earlier ask", "human", 100);
        h.board
            .seed_comment_by("card1", "self1", "a bare self comment", SELF_ID, 200);
        h.board.set_clock(1000);

        assert!(h.poll().is_none(), "a self comment bounds the tail");
    }

    /// The grooming wedge's own comment shape (afkd card 794), with the tail comment left
    /// to the caller. `set_clock(5000)` puts the tail ~56 years in the past to the wall
    /// clock, so it is stale to [`has_live_claim`] and the claim path is the arm under
    /// test.
    fn wedged_grooming_card(tail: &str) -> Harness {
        let h = Harness::new(discuss_cfg(DiscussWith::Anyone), "me");
        seed_source_card(&h.board, "card1");
        h.board.seed_comment_by(
            "card1",
            "self1",
            "two questions, then:\n\n1. 看起来不对 — is §2's boundary the watermark?\n\
             2. or afkd's last reply? 🙂\n",
            SELF_ID,
            50,
        );
        h.board
            .seed_comment_by("card1", "h1", "ok finalize", "human", 195);
        h.board
            .seed_comment_by("card1", "orphan", tail, SELF_ID, 200);
        h.board.set_clock(5000);
        h
    }

    #[test]
    fn discuss_orphaned_claim_marker_does_not_bound_the_tail() {
        let h = wedged_grooming_card(&claim_text("me"));
        assert_eq!(
            h.poll_claim().as_deref(),
            Some("card1"),
            "an aged-out orphan bounds nothing"
        );
    }

    #[test]
    fn discuss_attempt_marker_still_bounds_the_tail() {
        let h = wedged_grooming_card(&attempt_text(1, 1, "cargo test: 3 failed"));
        assert!(h.poll().is_none(), "an attempt marker is afkd speech");
        assert!(h
            .board
            .comments_on("card1")
            .iter()
            .all(|c| !is_claim(&c.text)));
    }

    #[test]
    fn discuss_gate_reads_nothing_when_unset() {
        let h = Harness::new(cfg(vec![], vec![], vec![]), "me");
        seed_source_card(&h.board, "card1");
        h.board.set_clock(1000);

        assert_eq!(h.poll_claim().as_deref(), Some("card1"));
        assert_eq!(h.board.resolve_count(), 0);
    }

    // --- The nested-comment poll: `discuss_with` off the card list ---

    /// A groomed thread as it really reads: multi-line with a blank line and trailing
    /// spaces, CJK + an emoji + an em dash, and its own `**` markup.
    const HUMAN_ASK: &str =
        "please groom this\n\n  看起来不对 🚨 — the **bold** claim in §2 is wrong  ";

    /// One grooming board, seeded identically whether or not the board nests each card's
    /// comments into the poll's own answer, covering every `discuss_with` verdict.
    fn groomable_board(nest: bool) -> Harness {
        let h = Harness::new(discuss_cfg(alice_only()), "me");
        h.board.seed_board_member("alice", "ma");
        h.board.seed_board_member("bob", "mb");
        h.board.add_list("Up for Grabs");
        h.board.add_list("In Progress");
        h.board.add_list("Review");
        for id in ["quiet", "fresh", "answered", "stranger", "tail"] {
            h.board
                .add_card("Up for Grabs", id, "Fix \"the\" café — 修复", "Body");
            if nest {
                h.board.nest_comments(id);
            }
        }
        h.board
            .seed_comment_by("quiet", "q-h", HUMAN_ASK, "ma", 100);
        h.board
            .seed_comment_by("quiet", "q-a", "groomed, see above", SELF_ID, 200);
        h.board
            .seed_comment_by("answered", "an-a", "groomed once", SELF_ID, 100);
        h.board
            .seed_comment_by("answered", "an-h", HUMAN_ASK, "ma", 300);
        h.board
            .seed_comment_by("stranger", "st-a", "groomed once", SELF_ID, 100);
        h.board.seed_comment_by("stranger", "st-b", "", "mb", 300);
        h.board
            .seed_comment_by("tail", "t-a", "groomed once", SELF_ID, 100);
        h.board
            .seed_comment_by("tail", "t-alice", HUMAN_ASK, "ma", 200);
        h.board
            .seed_comment_by("tail", "t-bob", "+1 from me", "mb", 300);
        h.board.set_clock(1000);
        h
    }

    /// The cards `polls` successive polls claim, in order, each claimed card's turn
    /// ended the way `finish` ends one on the grooming path — the lease released and afkd
    /// left the last speaker.
    fn claim_sequence(h: &Harness, polls: usize) -> Vec<String> {
        (0..polls)
            .filter_map(|_| {
                let unit = h.poll()?;
                h.board
                    .delete_comment(&unit.card.id, &unit.claim_id)
                    .expect("the lease releases");
                h.board.seed_comment_by(
                    &unit.card.id,
                    &format!("bs-{}", unit.card.id),
                    "afkd: groomed, nothing further from me",
                    SELF_ID,
                    4000,
                );
                Some(unit.card.id)
            })
            .collect()
    }

    /// The board calls a poll made *before* it posted its claim.
    fn calls_before_the_claim(board: &MockBoard) -> Vec<&'static str> {
        let calls = board.calls();
        match calls.iter().position(|c| *c == "post comment") {
            Some(at) => calls[..at].to_vec(),
            None => calls,
        }
    }

    /// The live `afkd::discuss` shape over a five-card list with afkd last on every card.
    fn all_quiet_board(nest: bool) -> Harness {
        let h = Harness::new(discuss_cfg(DiscussWith::Anyone), "me");
        h.board.add_list("Up for Grabs");
        for n in 1..=5 {
            let id = format!("card{n}");
            h.board.add_card("Up for Grabs", &id, "Fix 修复", "Body");
            h.board
                .seed_comment_by(&id, &format!("h{n}"), HUMAN_ASK, "human", 100);
            h.board
                .seed_comment_by(&id, &format!("a{n}"), "groomed", SELF_ID, 200);
            if nest {
                h.board.nest_comments(&id);
            }
        }
        h.board.set_clock(1000);
        h
    }

    #[test]
    fn a_discuss_poll_over_a_nested_list_reads_no_card_comments() {
        let nested = all_quiet_board(true);
        let plain = all_quiet_board(false);
        for board in [&nested, &plain] {
            assert!(claim_sequence(board, 1).is_empty());
        }
        assert_eq!(
            nested.board.calls(),
            [
                "board cards",
                "resolve list",
                "list cards",
                "resolve member"
            ],
        );
        assert_eq!(
            plain.board.calls(),
            [
                "board cards",
                "resolve list",
                "list cards",
                "resolve member",
                "read comments",
                "read comments",
                "read comments",
                "read comments",
                "read comments",
            ],
            "without the nesting the tail gate pays one round trip per card"
        );
    }

    #[test]
    fn a_claim_gated_scan_over_a_nested_list_reads_no_card_comments() {
        let h = all_quiet_board(true);
        for n in 1..=5 {
            let id = format!("card{n}");
            h.board
                .seed_comment_by(&id, &format!("h{n}-late"), HUMAN_ASK, "human", 300);
            h.board.seed_comment_by(
                &id,
                &format!("held{n}"),
                &claim_text("afkd::discuss"),
                SELF_ID,
                live_secs(5),
            );
        }
        assert!(claim_sequence(&h, 1).is_empty(), "every card is leased");
        assert_eq!(
            h.board.calls(),
            [
                "board cards",
                "resolve list",
                "list cards",
                "resolve member"
            ],
        );
    }

    #[test]
    fn discuss_verdicts_are_identical_nested_and_unnested() {
        let nested = claim_sequence(&groomable_board(true), 6);
        let plain = claim_sequence(&groomable_board(false), 6);
        assert_eq!(nested, plain, "the nesting must not change a verdict");
        assert_eq!(nested, ["fresh", "answered", "tail"]);
    }

    #[test]
    fn a_card_without_nested_comments_still_pays_for_the_read() {
        let h = all_quiet_board(false);
        for id in ["card1", "card3", "card4", "card5"] {
            h.board.nest_comments(id);
        }
        h.board
            .seed_comment_by("card5", "h5-late", HUMAN_ASK, "human", 300);

        assert_eq!(claim_sequence(&h, 1), ["card5"]);
        assert_eq!(
            calls_before_the_claim(&h.board),
            [
                "board cards",
                "resolve list",
                "list cards",
                "resolve member",
                "read comments"
            ],
        );

        let unnested = all_quiet_board(false);
        unnested
            .board
            .seed_comment_by("card5", "h5-late", HUMAN_ASK, "human", 300);
        assert_eq!(claim_sequence(&unnested, 1), ["card5"]);
        assert_eq!(
            calls_before_the_claim(&unnested.board)
                .iter()
                .filter(|c| **c == "read comments")
                .count(),
            5
        );
    }

    #[test]
    fn an_empty_nested_array_reads_as_first_sight_with_no_read() {
        let h = groomable_board(true);
        assert_eq!(claim_sequence(&h, 1), ["fresh"]);
        assert!(
            !calls_before_the_claim(&h.board).contains(&"read comments"),
            "{:?}",
            h.board.calls()
        );
    }

    // --- The last-speaker backstop ---

    /// A claimed unit, as a claim over `card1` with no thread would build it.
    fn claimed_unit(id: &str) -> Unit {
        claimed_unit_with_claim(id, "myclaim")
    }

    fn claimed_unit_with_claim(id: &str, claim_id: &str) -> Unit {
        Unit {
            card: card(id, "T", "B"),
            claim_id: claim_id.into(),
            comments: vec![],
            delivered_upto: UNIX_EPOCH,
            self_author: SELF_ID.into(),
        }
    }

    fn unit_of(card: Card, comments: Vec<Comment>, self_id: &str) -> Unit {
        Unit {
            card,
            claim_id: "myclaim".into(),
            comments,
            delivered_upto: UNIX_EPOCH,
            self_author: self_id.into(),
        }
    }

    /// afkd's own conversational comments on a card.
    fn replies(board: &MockBoard, card: &str) -> Vec<String> {
        board
            .comments_on(card)
            .into_iter()
            .filter(|c| c.author == SELF_ID && !is_control_marker(&c.text))
            .map(|c| c.text)
            .collect()
    }

    #[test]
    fn backstop_posts_when_agent_is_silent() {
        let h = Harness::new(discuss_cfg(DiscussWith::Anyone), "me");
        seed_source_card(&h.board, "card1");
        h.board
            .seed_comment("card1", "myclaim", &claim_text("me"), 1);

        assert!(h.run_script(&claimed_unit("card1"), &[proceed()]).delivered);

        assert_eq!(replies(&h.board, "card1"), ["reviewed, nothing to add"]);
        assert!(
            !h.board.comments_on("card1").iter().any(|c| is_ran(&c.text)),
            "no watermark is posted on the discuss path"
        );
    }

    #[test]
    fn no_backstop_when_agent_posted_even_with_a_straggler() {
        let h = Harness::new(discuss_cfg(DiscussWith::Anyone), "me");
        seed_source_card(&h.board, "card1");
        h.board
            .seed_comment("card1", "myclaim", &claim_text("me"), 1);
        let board = Arc::clone(&h.board);

        h.run(&claimed_unit("card1"), 1, |_| {
            board.seed_comment_by("card1", "agent-note", "here is my reply", SELF_ID, 500);
            // A human comment lands mid-run — a straggler the diff must not count.
            board.seed_comment_by("card1", "straggler", "another thought", "human", 501);
            proceed()
        });

        assert_eq!(replies(&h.board, "card1"), ["here is my reply"]);
    }

    #[test]
    fn backstop_text_states_outcome() {
        let clean = backstop_text(UnitOutcome::Clean, &Facts::none());
        assert!(!is_control_marker(&clean), "{clean}");
        assert_eq!(clean, "reviewed, nothing to add");
        let exhausted = backstop_text(UnitOutcome::Failed, &fault("compile failed"));
        assert!(!is_control_marker(&exhausted), "{exhausted}");
        assert_eq!(exhausted, "run did not complete: compile failed");
        // A `Failed` with no fault states the outcome with no reason.
        assert_eq!(
            backstop_text(UnitOutcome::Failed, &Facts::none()),
            "run did not complete: "
        );
        assert_eq!(
            backstop_text(UnitOutcome::Park, &Facts::none()),
            "awaiting a human reply"
        );
    }

    /// A lost race's clean-up delete is best-effort but not silent.
    #[test]
    fn a_failing_claim_clean_up_delete_is_logged() {
        let h = Harness::new(cfg(vec![move_to("In Progress")], vec![], vec![]), "me");
        seed_source_card(&h.board, "card1");
        h.board
            .seed_comment("card1", "rival1", &claim_text("rival"), 500);
        h.board.set_clock(1000);
        h.board.fail("delete comment");

        assert!(h.poll().is_none());
        assert_eq!(
            h.diag.errs(),
            ["trello delete comment: no response (mock failure)"]
        );
        assert!(
            !has_move_to(&h.board, "In Progress"),
            "a lost race runs no on_claim"
        );
    }

    #[test]
    fn claim_captures_feedback_delta_on_the_unit() {
        let h = Harness::new(cfg(vec![], vec![], vec![]), "me");
        seed_source_card(&h.board, "card1");
        h.board
            .seed_comment("card1", "h1", "please also fix the typo", 500);
        h.board.set_clock(1000);

        let unit = h.poll().expect("claimed");
        let delta = feedback_comments(&unit.comments, &unit.self_author);
        assert_eq!(texts(&delta), ["please also fix the typo"]);
        assert!(unit.comments.iter().any(|c| is_claim(&c.text)));
        assert!(!delta.iter().any(|c| is_claim(&c.text)));
        assert_eq!(unit.delivered_upto, UNIX_EPOCH + Duration::from_secs(500));
    }

    #[test]
    fn claim_loses_to_earlier_live_claim_and_deletes_own() {
        let h = Harness::new(cfg(vec![], vec![], vec![]), "me");
        seed_source_card(&h.board, "card1");
        h.board
            .seed_comment("card1", "rival1", &claim_text("rival"), 500);
        h.board.set_clock(1000);

        assert!(h.poll().is_none());
        let ids: Vec<String> = h
            .board
            .comments_on("card1")
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(ids, vec!["rival1".to_string()]);
    }

    #[test]
    fn stale_rival_claim_lets_us_win() {
        let h = Harness::new(cfg(vec![], vec![], vec![]), "me");
        seed_source_card(&h.board, "card1");
        h.board
            .seed_comment("card1", "rival1", &claim_text("rival"), 10);
        h.board.set_clock(10 + 7200);

        assert!(h.poll().is_some());
    }

    /// A failed re-read deletes our claim before propagating: the poll ends rather than
    /// marching down the list on a board that is not answering.
    #[test]
    fn a_failed_reread_deletes_our_claim_and_propagates() {
        let h = Harness::new(cfg(vec![], vec![], vec![]), "me");
        seed_source_card(&h.board, "card1");
        h.board.set_clock(1000);
        h.board.fail("read comments");

        let Err(err) = h.try_claim() else {
            panic!("the re-read failure propagates");
        };
        assert_eq!(err.stage(), "read comments");
        h.board.clear_failure();
        assert!(h.board.comments_on("card1").is_empty());
    }

    // --- The live-claim intake gate ---

    /// Two source cards, both nested and both tail-open, with a live `[afkd-claim]` on
    /// the head card authored by `who`.
    fn two_cards_with_a_live_claim(cfg: BoardConfig, owner: &str, who: &str) -> Harness {
        let h = Harness::new(cfg, owner);
        h.board.add_list("Up for Grabs");
        h.board.add_list("In Progress");
        for id in ["card1", "card2"] {
            h.board
                .add_card("Up for Grabs", id, "Fix \"the\" café — 修复", "Body");
            h.board
                .seed_comment_by(id, &format!("h-{id}"), HUMAN_ASK, "human", live_secs(120));
            h.board.nest_comments(id);
        }
        h.board.seed_comment_by(
            "card1",
            "held",
            &claim_text("afkd::discuss"),
            who,
            live_secs(5),
        );
        h.board.set_clock(live_secs(0));
        h
    }

    #[test]
    fn a_live_claim_on_the_head_card_is_stepped_over() {
        let h = two_cards_with_a_live_claim(
            discuss_cfg(DiscussWith::Anyone),
            "afkd::discuss+1",
            SELF_ID,
        );
        assert_eq!(h.poll_claim().as_deref(), Some("card2"));
        let ids: Vec<String> = h
            .board
            .comments_on("card1")
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(ids, ["h-card1", "held"]);
        assert!(
            !h.board
                .actions()
                .iter()
                .any(|a| matches!(a, Action::DeleteComment { .. })),
            "{:?}",
            h.board.actions()
        );
        assert_eq!(
            h.board
                .calls()
                .iter()
                .filter(|c| **c == "post comment")
                .count(),
            1,
            "one claim posted, on card2 alone"
        );
    }

    #[test]
    fn a_live_claim_is_stepped_over_with_no_discuss_gate() {
        let h = two_cards_with_a_live_claim(
            cfg(vec![move_to("In Progress")], vec![], vec![]),
            "me",
            "rival-host",
        );
        assert_eq!(h.poll_claim().as_deref(), Some("card2"));
        let ids: Vec<String> = h
            .board
            .comments_on("card1")
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(ids, ["h-card1", "held"], "the busy card is untouched");
        assert!(!h
            .board
            .actions()
            .iter()
            .any(|a| matches!(a, Action::DeleteComment { .. })));
        assert_eq!(h.board.resolve_count(), 0);
    }

    #[test]
    fn a_claim_past_its_lifetime_does_not_skip_the_card() {
        let h = Harness::new(cfg(vec![], vec![], vec![]), "me");
        seed_source_card(&h.board, "card1");
        h.board.nest_comments("card1");
        h.board.seed_comment_by(
            "card1",
            "orphan",
            &claim_text("me"),
            SELF_ID,
            live_secs(7200),
        );
        h.board.set_clock(live_secs(0));

        assert_eq!(h.poll_claim().as_deref(), Some("card1"));
    }

    #[test]
    fn a_lost_claim_race_resumes_the_scan_at_the_next_card() {
        let h = Harness::new(cfg(vec![], vec![], vec![]), "me");
        seed_source_card(&h.board, "card1");
        h.board.add_card("Up for Grabs", "card2", "Second", "Body");
        h.board.nest_comments("card1");
        h.board.nest_comments("card2");
        h.board
            .seed_comment("card1", "rival1", &claim_text("rival"), 500);
        h.board.set_clock(1000);

        let unit = h.poll().expect("the scan resumes at the next candidate");
        assert_eq!(unit.card.id, "card2");
        let card1: Vec<String> = h
            .board
            .comments_on("card1")
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(card1, ["rival1"], "our lost claim was removed");
        assert!(h
            .board
            .comments_on("card2")
            .iter()
            .any(|c| c.id == unit.claim_id && is_claim(&c.text)));
    }

    #[test]
    fn a_card_only_in_the_in_progress_list_is_never_picked_up() {
        let h = Harness::new(cfg(vec![move_to("In Progress")], vec![], vec![]), "me");
        h.board.add_list("Up for Grabs");
        h.board.add_list("In Progress");
        h.board.add_card("In Progress", "card1", "T", "B");
        h.board.set_clock(1000);

        assert!(h.poll().is_none());
        assert!(h.board.comments_on("card1").is_empty());
    }

    // --- Attempts + the terminal lifecycle ---

    fn deleted(board: &MockBoard, comment: &str) -> bool {
        board
            .actions()
            .iter()
            .any(|a| matches!(a, Action::DeleteComment { comment: c, .. } if c == comment))
    }

    #[test]
    fn attempts_stop_on_first_clean_and_post_markers_on_fault() {
        let h = Harness::new(cfg(vec![], vec![move_to("Review")], vec![]), "me");
        seed_source_card(&h.board, "card1");
        h.board
            .seed_comment("card1", "myclaim", &claim_text("me"), 1);
        let mut script = [fault("boom"), proceed(), proceed()].into_iter();

        let run = h.run(&claimed_unit("card1"), 3, |_| script.next().unwrap());

        assert_eq!(run.runs, 2);
        let markers: Vec<String> = h
            .board
            .comments_on("card1")
            .into_iter()
            .filter(|c| c.text.starts_with(ATTEMPT_MARKER))
            .map(|c| c.text)
            .collect();
        assert_eq!(markers, ["[afkd-attempt] 1/3: boom"]);
        assert!(has_move_to(&h.board, "Review"));
        assert!(deleted(&h.board, "myclaim"));
    }

    #[test]
    fn all_attempts_failing_runs_on_fail_then_releases_lease() {
        let h = Harness::new(cfg(vec![], vec![], vec![move_to("Backlog")]), "me");
        seed_source_card(&h.board, "card1");
        h.board.add_list("Backlog");
        h.board
            .seed_comment("card1", "myclaim", &claim_text("me"), 1);

        let run = h.run_script(&claimed_unit("card1"), &[fault("a"), fault("b")]);

        assert_eq!((run.runs, run.outcome), (2, UnitOutcome::Failed));
        assert!(has_move_to(&h.board, "Backlog"));
        assert!(deleted(&h.board, "myclaim"));
        let markers: Vec<String> = h
            .board
            .comments_on("card1")
            .into_iter()
            .filter(|c| c.text.starts_with(ATTEMPT_MARKER))
            .map(|c| c.text)
            .collect();
        assert_eq!(markers, ["[afkd-attempt] 1/2: a", "[afkd-attempt] 2/2: b"]);
    }

    #[test]
    fn on_fail_add_label_attaches_the_label() {
        let h = Harness::new(cfg(vec![], vec![], vec![add_label("Problem")]), "me");
        seed_source_card(&h.board, "card1");
        h.board
            .seed_comment("card1", "myclaim", &claim_text("me"), 1);

        h.run_script(&claimed_unit("card1"), &[fault("a"), fault("b")]);

        assert!(h.board.actions().iter().any(
            |a| matches!(a, Action::AddLabel { card, label } if card == "card1" && label == "Problem")
        ));
        assert!(deleted(&h.board, "myclaim"));
    }

    #[test]
    fn on_done_move_to_applies_the_configured_list_position_to_the_board() {
        let h = Harness::new(
            cfg(
                vec![],
                vec![move_to_at("Review", ListPosition::Bottom)],
                vec![],
            ),
            "me",
        );
        seed_source_card(&h.board, "card1");
        h.board
            .seed_comment("card1", "myclaim", &claim_text("me"), 1);

        h.run_script(&claimed_unit("card1"), &[proceed()]);

        assert!(h.board.actions().iter().any(|a| matches!(
            a,
            Action::Move { list, position: ListPosition::Bottom, .. } if list == "Review"
        )));
    }

    #[test]
    fn on_done_comment_posts_the_literal_text_to_the_card() {
        let h = Harness::new(
            cfg(vec![], vec![comment_action("handled by afkd")], vec![]),
            "me",
        );
        seed_source_card(&h.board, "card1");
        h.board
            .seed_comment("card1", "myclaim", &claim_text("me"), 1);

        h.run_script(&claimed_unit("card1"), &[proceed()]);

        assert!(h
            .board
            .comments_on("card1")
            .iter()
            .any(|c| c.text == "handled by afkd"));
    }

    #[test]
    fn on_done_comment_substitutes_run_facts_onto_the_card() {
        let h = Harness::new(
            cfg(
                vec![],
                vec![comment_action(
                    "done in @{run:duration} — @{run:cost}, @{run:turns} turns — \
                     log: .afkd/runs/afkd::selfdev/@{run:name}/run.log",
                )],
                vec![],
            ),
            "me",
        );
        seed_source_card(&h.board, "card1");
        h.board
            .seed_comment("card1", "myclaim", &claim_text("me"), 1);

        h.run_script(
            &claimed_unit("card1"),
            &[measured("260722-141802-card-QBOL9KfN-1")],
        );

        assert!(
            h.board.comments_on("card1").iter().any(|c| c.text
                == "done in 5.00s — $1.50, 3 turns — \
                    log: .afkd/runs/afkd::selfdev/260722-141802-card-QBOL9KfN-1/run.log"),
            "{:?}",
            h.board.comments_on("card1"),
        );
    }

    #[test]
    fn failed_terminal_action_holds_the_lease_for_the_pending_retry() {
        let h = Harness::new(cfg(vec![], vec![move_to("Review")], vec![]), "me");
        seed_source_card(&h.board, "card1");
        h.board
            .seed_comment("card1", "myclaim", &claim_text("me"), 1);
        h.board.fail("move card");

        let run = h.run_script(&claimed_unit("card1"), &[proceed()]);

        assert!(!run.delivered);
        assert!(h
            .board
            .comments_on("card1")
            .iter()
            .any(|c| c.id == "myclaim"));
        assert!(!h
            .board
            .actions()
            .iter()
            .any(|a| matches!(a, Action::DeleteComment { .. })));
    }

    #[test]
    fn run_unit_posts_ran_marker_on_release() {
        let h = Harness::new(cfg(vec![], vec![move_to("Review")], vec![]), "me");
        seed_source_card(&h.board, "card1");
        h.board
            .seed_comment("card1", "myclaim", &claim_text("me"), 1);

        h.run_script(&claimed_unit("card1"), &[proceed()]);

        let comments = h.board.comments_on("card1");
        assert!(comments.iter().any(|c| is_ran(&c.text)));
        assert!(!comments.iter().any(|c| c.id == "myclaim"));
        assert!(comments.iter().any(|c| ran_upto(&c.text).is_some()));
    }

    fn prior_ran(id: &str, upto: u64) -> Comment {
        Comment {
            id: id.into(),
            text: ran_text("me", UNIX_EPOCH + Duration::from_secs(upto)),
            author: SELF_ID.into(),
            author_name: SELF_ID.into(),
            posted_at: UNIX_EPOCH + Duration::from_secs(upto),
            renewed_at: UNIX_EPOCH + Duration::from_secs(upto),
        }
    }

    #[test]
    fn run_unit_keeps_single_ran_marker() {
        let h = Harness::new(cfg(vec![], vec![move_to("Review")], vec![]), "me");
        seed_source_card(&h.board, "card1");
        h.board
            .seed_comment("card1", "myclaim", &claim_text("me"), 1);
        let (prior0, prior1) = (prior_ran("ran-old0", 10), prior_ran("ran-old1", 20));
        h.board.seed_comment("card1", &prior0.id, &prior0.text, 10);
        h.board.seed_comment("card1", &prior1.id, &prior1.text, 20);
        let mut unit = claimed_unit("card1");
        unit.comments = vec![prior0, prior1];
        unit.delivered_upto = UNIX_EPOCH + Duration::from_secs(20);

        h.run_script(&unit, &[proceed()]);

        let comments = h.board.comments_on("card1");
        let survivors: Vec<Comment> = comments
            .iter()
            .filter(|c| is_ran(&c.text))
            .cloned()
            .collect();
        assert_eq!(survivors.len(), 1);
        assert!(!comments
            .iter()
            .any(|c| c.id == "ran-old0" || c.id == "ran-old1"));
        assert_eq!(ran_upto(&survivors[0].text), Some(unit.delivered_upto));
        assert_eq!(watermark_of(&survivors), Some(unit.delivered_upto));
    }

    #[test]
    fn ran_marker_prune_failure_leaves_boundary_correct() {
        let h = Harness::new(cfg(vec![], vec![move_to("Review")], vec![]), "me");
        seed_source_card(&h.board, "card1");
        h.board
            .seed_comment("card1", "myclaim", &claim_text("me"), 1);
        let prior = prior_ran("ran-old0", 10);
        h.board.seed_comment("card1", &prior.id, &prior.text, 10);
        let mut unit = claimed_unit("card1");
        unit.comments = vec![prior];
        unit.delivered_upto = UNIX_EPOCH + Duration::from_secs(20);
        h.board.fail("delete comment");

        h.run_script(&unit, &[proceed()]);

        let comments = h.board.comments_on("card1");
        assert!(comments.iter().filter(|c| is_ran(&c.text)).count() >= 2);
        assert_eq!(watermark_of(&comments), Some(unit.delivered_upto));
    }

    // --- The unit as it crosses the wire ---

    /// The unit the built-in's spine would have run, field for field: the name, the
    /// journal key and the thread are three distinct reads of one card, the env carries
    /// the credentials and the card id the skill reads, `seen` is the claim-read thread,
    /// and the brief is the only file, unframed.
    #[test]
    fn a_units_name_key_and_thread_are_three_distinct_reads_of_one_card() {
        let h = Harness::new(cfg(vec![], vec![], vec![]), "me");
        let card = Card {
            id: "6a4dd5de1234abcd5678ef90".into(),
            short_link: "1Rk/el ydw 修 🚨".into(),
            ..card("ignored", "Fix", "do x")
        };
        let unit = unit_of(
            card,
            vec![comment("h1", "also handle empty input", 100)],
            SELF_ID,
        );
        let wire = h.units.wire_unit(&unit);

        // One `-` per unsafe CHAR, so the wide glyph and the emoji each fold to one.
        assert_eq!(wire.id, "1Rk-el-ydw----");
        assert_eq!(wire.key, "6a4dd5de1234abcd5678ef90#myclaim");
        assert_eq!(wire.thread, "1Rk/el ydw 修 🚨");
        assert_ne!(wire.id, wire.thread);
        assert_ne!(wire.key, wire.thread);
        assert!(!wire.id.contains("6a4dd5de"));
        assert_eq!(wire.seen, ["h1"]);
        assert_eq!(wire.me, SELF_ID);
        assert_eq!(
            wire.env,
            BTreeMap::from([
                ("TRELLO_API_KEY".to_string(), "k".to_string()),
                ("TRELLO_BOARD_ID".to_string(), "BID".to_string()),
                (
                    "TRELLO_CARD_ID".to_string(),
                    "6a4dd5de1234abcd5678ef90".to_string()
                ),
                ("TRELLO_TOKEN".to_string(), "t".to_string()),
            ])
        );
        assert_eq!(
            wire.files,
            [WireFile {
                path: "task.md".into(),
                text:
                    "Fix\n\ndo x\n\n## New comments\n\n**Dana Rivera:** also handle empty input\n"
                        .into(),
            }]
        );
    }

    /// The thread keys one conversation across every claim of a card — two claims, two
    /// journal keys, one thread — and separates one card from another.
    #[test]
    fn the_thread_is_stable_per_card_and_distinct_across_cards() {
        let h = Harness::new(cfg(vec![], vec![], vec![]), "me");
        let first = h.units.wire_unit(&claimed_unit_with_claim("card1", "c1"));
        let again = h.units.wire_unit(&claimed_unit_with_claim("card1", "c2"));
        let other = h.units.wire_unit(&claimed_unit_with_claim("card2", "c1"));
        assert_eq!(first.thread, "sl-card1");
        assert_eq!(again.thread, first.thread);
        assert_ne!(again.key, first.key);
        assert_eq!(other.thread, "sl-card2");
    }

    /// Driven through the real claim path: two board members with distinct member ids
    /// and display names comment on a card, and the brief names each of them — never the
    /// raw member id, and never afkd's own display name.
    #[test]
    fn the_brief_names_each_commenter_and_leaks_no_member_id() {
        let h = Harness::new(cfg(vec![], vec![], vec![]), "me");
        seed_source_card(&h.board, "card1");
        h.board.seed_comment_named(
            "card1",
            "h1",
            "this breaks the retry path when the token expires:\n\n```\nGET /v1 401\n```\n  ",
            "5f4d1c2b9a0000000000aa01",
            "Alice Smith",
            400,
        );
        h.board.seed_comment_named(
            "card1",
            "self1",
            "q2 restated, plainly: which of the two boundaries wins?\n\n- the watermark\n- the last reply\n",
            SELF_ID,
            "Robin Vale",
            450,
        );
        h.board.seed_comment_named(
            "card1",
            "h2",
            "disagree — 陳's patch already handles that 🙂",
            "5f4d1c2b9a0000000000bb02",
            "Bob Jones",
            500,
        );
        h.board.set_clock(1000);

        let unit = h.poll().expect("claimed");
        let brief = &h.units.wire_unit(&unit).files[0].text;
        assert_eq!(
            brief,
            "Title\n\nBody\n\n## Earlier conversation\n\
             \n**Alice Smith:** this breaks the retry path when the token expires:\
             \n\n```\nGET /v1 401\n```\n\
             \n**afkd:** q2 restated, plainly: which of the two boundaries wins?\
             \n\n- the watermark\n- the last reply\n\
             \n\n## New comments\n\
             \n**Bob Jones:** disagree — 陳's patch already handles that 🙂\n"
        );
        assert!(!brief.contains("**human:**"), "{brief}");
        assert!(!brief.contains("5f4d1c2b9a"), "no raw member id: {brief}");
        assert!(!brief.contains("Robin Vale"), "afkd is not named: {brief}");
        assert!(unit
            .comments
            .iter()
            .any(|c| c.author == "5f4d1c2b9a0000000000aa01"));
    }

    #[test]
    fn classify_parks_on_the_marker_and_otherwise_keeps_afkds_verdict() {
        let scratch = TempDir::new("classify");
        for verdict in [UnitOutcome::Clean, UnitOutcome::Failed, UnitOutcome::Park] {
            assert_eq!(TrelloUnits::classify(scratch.path(), verdict), verdict);
        }
        std::fs::write(scratch.path().join(PARK_FILE), b"").unwrap();
        for verdict in [UnitOutcome::Clean, UnitOutcome::Failed] {
            assert_eq!(
                TrelloUnits::classify(scratch.path(), verdict),
                UnitOutcome::Park
            );
        }
        // A directory of that name is no marker.
        let dir = TempDir::new("classify-dir");
        std::fs::create_dir(dir.path().join(PARK_FILE)).unwrap();
        assert_eq!(
            TrelloUnits::classify(dir.path(), UnitOutcome::Clean),
            UnitOutcome::Clean
        );
    }

    /// afkd sends `attempt_failed` with the fault's sentence; with none there is nothing
    /// to mark, as the built-in returned early on a non-fault signal.
    #[test]
    fn attempt_failed_without_a_reason_posts_nothing() {
        let h = Harness::new(cfg(vec![], vec![], vec![]), "me");
        seed_source_card(&h.board, "card1");
        h.units
            .attempt_failed(&claimed_unit("card1"), 1, 2, None, &h.diag);
        assert!(h.board.calls().is_empty());
        h.units.attempt_failed(
            &claimed_unit("card1"),
            2,
            2,
            Some("run_cmd `make test` failed: 2\n\n修复 🚨"),
            &h.diag,
        );
        assert_eq!(
            texts(&h.board.comments_on("card1")),
            ["[afkd-attempt] 2/2: run_cmd `make test` failed: 2\n\n修复 🚨"]
        );
    }

    // --- Error swallowing + the poll loop ---

    #[test]
    fn a_poll_board_error_is_swallowed_and_diagnosed_once() {
        let h = Harness::new(cfg(vec![], vec![], vec![]), "me");
        seed_source_card(&h.board, "card1");
        h.board.set_clock(1000);
        h.board.fail("list cards");
        assert!(h.poll().is_none());
        assert_eq!(
            h.diag.errs(),
            ["trello list cards: no response (mock failure)"]
        );
        assert!(h.diag.narrated().is_empty());
        h.board.clear_failure();
        assert!(h.poll().is_some());
    }

    #[test]
    fn a_clean_completion_lets_the_next_card_be_claimed() {
        let h = Harness::new(cfg(vec![move_to("In Progress")], vec![], vec![]), "me");
        seed_source_card(&h.board, "card1");
        h.board.add_card("Up for Grabs", "card2", "Title", "Body");
        h.board.set_clock(1000);

        let unit = h.poll().expect("claimed card1");
        assert_eq!(unit.card.id, "card1");
        h.run_script(&unit, &[proceed()]);
        assert_eq!(h.poll_claim().as_deref(), Some("card2"));
        assert!(h.diag.errs().is_empty(), "{:?}", h.diag.errs());
    }

    #[test]
    fn a_wind_down_board_error_is_diagnosed_and_the_next_card_is_claimed() {
        let h = Harness::new(cfg(vec![move_to("In Progress")], vec![], vec![]), "me");
        seed_source_card(&h.board, "card1");
        h.board.add_card("Up for Grabs", "card2", "Title", "Body");
        h.board.set_clock(1000);

        let unit = h.poll().expect("claimed card1");
        h.board.fail("delete comment");
        h.run_script(&unit, &[proceed()]);
        h.board.clear_failure();
        assert_eq!(h.poll_claim().as_deref(), Some("card2"));
        assert_eq!(
            h.diag.errs(),
            ["trello delete comment: no response (mock failure)"]
        );
    }

    #[test]
    fn one_beat_claims_runs_and_finishes_one_card_end_to_end() {
        let h = Harness::new(
            cfg(
                vec![move_to("In Progress")],
                vec![move_to("Review")],
                vec![],
            ),
            "me",
        );
        seed_source_card(&h.board, "card1");
        h.board.set_clock(1000);

        assert_eq!(h.beat(1, |_| proceed()), None, "delivered, nothing held");
        assert!(has_move_to(&h.board, "In Progress"));
        assert!(has_move_to(&h.board, "Review"));
        assert!(deleted(&h.board, "c1000"));
    }

    /// A sole claim always wins, and adding strictly-later claims never turns a win into
    /// a loss — driven through [`claim_markers`], the exact mapping the claim feeds the
    /// decision. The built-in's proptest, as a deterministic sweep over every vector of
    /// up to five later offsets drawn from a spread that covers both edges of the
    /// lifetime.
    #[test]
    fn later_claims_never_unseat_us() {
        const OFFSETS: [u64; 6] = [1, 2, 59, 1800, 3599, 3600];
        let base = 10_000u64;
        let mut vectors: Vec<Vec<u64>> = vec![vec![]];
        for _ in 0..5 {
            let longer: Vec<Vec<u64>> = vectors
                .iter()
                .filter(|v| v.len() == vectors.last().map_or(0, Vec::len))
                .flat_map(|v| {
                    OFFSETS.iter().map(move |dt| {
                        let mut next = v.clone();
                        next.push(*dt);
                        next
                    })
                })
                .collect();
            vectors.extend(longer);
        }
        assert_eq!(vectors.len(), 1 + 6 + 36 + 216 + 1296 + 7776);
        for laters in &vectors {
            let mut comments = vec![comment("mine", &claim_text("me"), base)];
            for (i, dt) in laters.iter().enumerate() {
                comments.push(comment(&format!("r{i}"), &claim_text("rival"), base + dt));
            }
            assert!(
                won_claim(&claim_markers(&comments), "mine", CLAIM_LIFETIME),
                "{laters:?}"
            );
        }
    }

    // --- Successful lifecycle actions narrate themselves ---

    /// The card the narration tests render: the delimiter a success line wraps a title
    /// in, wide CJK, an emoji and an em dash.
    const NARRATED_TITLE: &str = "Fix \"the\" café — 修复 🚨 déjà vu";

    fn narrated_card(title: &str) -> Card {
        card("card1", title, "B")
    }

    /// Each [`LifecycleAction`] variant's narration key: the exhaustiveness guard.
    fn narration_key(action: &LifecycleAction) -> &'static str {
        match action {
            LifecycleAction::MarkComplete => "mark_complete",
            LifecycleAction::Archive => "archive",
            LifecycleAction::MoveTo { .. } => "move_to",
            LifecycleAction::AddLabel { .. } => "add_label",
            LifecycleAction::RemoveLabel { .. } => "remove_label",
            LifecycleAction::AddMember(_) => "add_member",
            LifecycleAction::RemoveMember(_) => "remove_member",
            LifecycleAction::Comment(_) => "comment",
        }
    }

    /// Every narratable action paired with the exact line it must produce for `title`.
    fn narration_table(title: &str) -> Vec<(LifecycleAction, String)> {
        vec![
            (
                LifecycleAction::MarkComplete,
                format!("{ACTION_TAG} marking card \"{title}\" complete"),
            ),
            (
                LifecycleAction::Archive,
                format!("{ACTION_TAG} archiving card \"{title}\""),
            ),
            (
                move_to_at("In Progress", ListPosition::Top),
                format!("{ACTION_TAG} moving card \"{title}\" to list \"In Progress\" (at top)"),
            ),
            (
                move_to_at("Review", ListPosition::Bottom),
                format!("{ACTION_TAG} moving card \"{title}\" to list \"Review\" (at bottom)"),
            ),
            (
                add_label("Problem"),
                format!("{ACTION_TAG} adding label \"Problem\" to card \"{title}\""),
            ),
            (
                remove_label("Redo"),
                format!("{ACTION_TAG} removing label \"Redo\" from card \"{title}\""),
            ),
            (
                LifecycleAction::AddMember(MemberRef::SelfMember),
                format!("{ACTION_TAG} adding member \"self\" to card \"{title}\""),
            ),
            (
                add_member_named("marisa"),
                format!("{ACTION_TAG} adding member \"marisa\" to card \"{title}\""),
            ),
            (
                remove_member_named("marisa"),
                format!("{ACTION_TAG} removing member \"marisa\" from card \"{title}\""),
            ),
            (
                comment_action(
                    "done in 5.00s — $1.50\n\nthe rest of this body is long, multi-line, and\n\
                     has no business in a service log line",
                ),
                format!("{ACTION_TAG} commenting on card \"{title}\""),
            ),
        ]
    }

    #[test]
    fn action_line_names_the_card_and_its_operand_for_every_action() {
        let card = narrated_card(NARRATED_TITLE);
        let table = narration_table(NARRATED_TITLE);
        for (action, expected) in &table {
            assert_eq!(&action_line(action, &card), expected);
        }
        let mut covered: Vec<&str> = table.iter().map(|(a, _)| narration_key(a)).collect();
        covered.sort_unstable();
        covered.dedup();
        assert_eq!(
            covered,
            [
                "add_label",
                "add_member",
                "archive",
                "comment",
                "mark_complete",
                "move_to",
                "remove_label",
                "remove_member",
            ]
        );
        let commented = &table[table.len() - 1].1;
        assert!(!commented.contains('\n'), "{commented}");
        assert_eq!(
            action_line(&LifecycleAction::Archive, &narrated_card("")),
            format!("{ACTION_TAG} archiving card \"\"")
        );
    }

    #[test]
    fn applying_every_action_narrates_one_tagged_line_each_and_raises_no_error() {
        let h = Harness::new(cfg(vec![], vec![], vec![]), "me");
        h.board.add_list("Up for Grabs");
        h.board.add_list("In Progress");
        h.board.add_list("Review");
        h.board
            .add_card("Up for Grabs", "card1", NARRATED_TITLE, "B");
        h.board.seed_card_label("card1", "Redo");
        let card = narrated_card(NARRATED_TITLE);
        let actions: Vec<LifecycleAction> = narration_table(NARRATED_TITLE)
            .into_iter()
            .map(|(action, _)| action)
            .collect();

        assert_eq!(
            h.units
                .apply_actions(&actions, &card, &Facts::none(), &h.diag),
            actions.len(),
        );
        // Parity against the pure renderer: the narrated lines are its own output.
        assert_eq!(
            h.diag.narrated(),
            actions
                .iter()
                .map(|a| action_line(a, &card))
                .collect::<Vec<_>>(),
        );
        assert!(h.diag.errs().is_empty(), "a success is not a problem");

        let recorded = h.board.actions();
        let saw = |pred: fn(&Action) -> bool| recorded.iter().any(pred);
        assert!(saw(|a| matches!(a, Action::Complete(c) if c == "card1")));
        assert!(saw(|a| matches!(a, Action::Archive(c) if c == "card1")));
        assert_eq!(
            recorded
                .iter()
                .filter(|a| matches!(a, Action::Move { .. }))
                .count(),
            2
        );
        assert!(saw(
            |a| matches!(a, Action::AddLabel { label, .. } if label == "Problem")
        ));
        assert!(saw(
            |a| matches!(a, Action::RemoveLabel { label, .. } if label == "Redo")
        ));
        assert!(saw(
            |a| matches!(a, Action::AddMember { member, .. } if *member == MemberRef::SelfMember)
        ));
        assert!(saw(|a| matches!(
            a,
            Action::AddMember { member, .. } if matches!(member, MemberRef::Username(u) if u == "marisa")
        )));
        assert!(saw(|a| matches!(
            a,
            Action::RemoveMember { member, .. } if matches!(member, MemberRef::Username(u) if u == "marisa")
        )));
        assert!(h
            .board
            .comments_on("card1")
            .iter()
            .any(|c| c.text.contains("no business in a service log line")));
    }

    #[test]
    fn a_failed_add_member_emits_no_success_line_and_stops_the_moment() {
        let h = Harness::new(cfg(vec![], vec![], vec![]), "me");
        h.board.add_list("In Progress");
        h.board.fail("add member");

        assert_eq!(
            h.units.apply_actions(
                &[add_member_named("marisa"), move_to("In Progress")],
                &card("card1", "T", "B"),
                &Facts::none(),
                &h.diag
            ),
            0,
        );
        assert!(h.diag.narrated().is_empty());
        assert_eq!(
            h.diag.errs(),
            ["trello add member: no response (mock failure)"]
        );
        assert!(!h
            .board
            .actions()
            .iter()
            .any(|a| matches!(a, Action::Move { .. })));
    }

    #[test]
    fn a_failed_action_emits_no_success_line_and_keeps_the_error_path() {
        let h = Harness::new(cfg(vec![], vec![], vec![]), "me");
        h.board.add_list("In Progress");
        h.board.fail("move card");

        assert_eq!(
            h.units.apply_actions(
                &[move_to("In Progress")],
                &card("card1", "T", "B"),
                &Facts::none(),
                &h.diag
            ),
            0,
        );
        assert!(h.diag.narrated().is_empty());
        assert_eq!(
            h.diag.errs(),
            ["trello move card: no response (mock failure)"]
        );
    }

    #[test]
    fn a_won_claim_logs_a_friendly_line() {
        let h = Harness::new(cfg(vec![], vec![], vec![]), "me");
        seed_source_card(&h.board, "card1");
        h.board.set_clock(1000);

        h.poll().expect("claimed");
        assert_eq!(
            h.diag.narrated(),
            [format!("{ACTION_TAG} claimed card \"Title\"")]
        );
        assert!(h.diag.errs().is_empty());
    }

    #[test]
    fn releasing_the_lease_logs_a_friendly_line() {
        let h = Harness::new(cfg(vec![], vec![], vec![]), "me");
        seed_source_card(&h.board, "card1");
        h.board
            .seed_comment("card1", "myclaim", &claim_text("me"), 1);
        let unit = unit_of(card("card1", "Ship it", "B"), vec![], SELF_ID);

        h.run_script(&unit, &[proceed()]);

        assert_eq!(
            h.diag.narrated(),
            [format!("{ACTION_TAG} released claim on card \"Ship it\"")]
        );
        assert!(h.diag.errs().is_empty());
    }

    // --- The clarification gate: park the card, badge it, re-arm on a reply ---

    /// The agent's question, as the skill's `ask.py` posts it — multi-paragraph, fenced,
    /// non-ASCII, with the trailing whitespace the brief trims.
    const PARK_QUESTION: &str = "I can't build this confidently — the card reads two ways:\n\n\
         1. `pick_from` names the **list** a card is claimed from, or\n\
         2. it names the label 名前 the card must carry\n\n\
         ```conf\ntrigger trello { pick_from \"Up for Grabs\" }\n```\n\n\
         which did you mean? 🙏   ";

    /// The human's answer, posted after afkd's last word — the comment that re-arms a
    /// badged card.
    const PARK_REPLY: &str = "reading 1 — the **list**. 看 gallery/06:\n\n\
         ```conf\npick_from \"Up for Grabs\"\n```\n\n\
         drop the label idea entirely.  ";

    /// The realistic service shape: `on_claim` moves the card into `In Progress`,
    /// `on_done` moves it to `Review`, and `on_fail` is the dead end the park avoids —
    /// `Backlog` under a red `Problem` label. Run with two attempts.
    fn park_cfg() -> BoardConfig {
        cfg(
            vec![move_to("In Progress")],
            vec![move_to("Review")],
            vec![move_to("Backlog"), add_label("Problem")],
        )
    }

    /// One card in `pick_from` with a human's opening comment, a second card queued
    /// behind it, and the `Backlog` list `on_fail` would use.
    fn seed_park_board(board: &MockBoard) {
        seed_source_card(board, "card1");
        board.add_list("Backlog");
        board.add_card("Up for Grabs", "queued", "Next up", "Body");
        board.seed_comment_named(
            "card1",
            "h0",
            "the retry backoff is wrong — see the header",
            "mem-phil",
            "Phil Ek",
            100,
        );
        board.set_clock(1000);
    }

    /// One beat in which `card1`'s agent asks and parks.
    fn park_beat(h: &Harness) -> Option<String> {
        h.beat(2, ask(&h.board, "card1", fault("parked")))
    }

    fn is_badged(board: &MockBoard, card: &str) -> bool {
        board
            .read_card(card)
            .unwrap()
            .is_some_and(|c| c.labels.iter().any(|l| l == AWAITING_LABEL))
    }

    #[test]
    fn a_park_marker_parks_the_card_whatever_signal_ended_the_run() {
        // `classify` reads the marker BEFORE the signal, so an ask that ends in `fail`,
        // in `break`, or cleanly parks alike.
        for facts in [fault("parked: awaiting a human reply"), broke(), proceed()] {
            let h = Harness::new(park_cfg(), "me");
            seed_park_board(&h.board);
            let unit = h.poll().expect("claimed");
            let run = h.run(&unit, 2, ask(&h.board, "card1", facts.clone()));

            assert_eq!(run.runs, 1, "a park is terminal: {facts:?}");
            assert_eq!(run.outcome, UnitOutcome::Park);
            assert!(run.delivered);
            let comments = h.board.comments_on("card1");
            assert!(
                !comments.iter().any(|c| c.text.starts_with(ATTEMPT_MARKER)),
                "a park is not a failed attempt: {comments:?}"
            );
            assert!(comments.iter().all(|c| !is_claim(&c.text)), "released");
            assert!(comments.iter().any(|c| c.text == PARK_QUESTION));
            let acts = h.board.actions();
            assert!(acts.iter().any(
                |a| matches!(a, Action::AddLabel { card, label } if card == "card1" && label == AWAITING_LABEL)
            ));
            assert!(!acts
                .iter()
                .any(|a| matches!(a, Action::AddLabel { label, .. } if label == "Problem")));
            let moves: Vec<&str> = acts
                .iter()
                .filter_map(|a| match a {
                    Action::Move { list, .. } => Some(list.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(moves, ["In Progress"], "the card does not move on a park");
            assert!(is_badged(&h.board, "card1"));
            assert!(h.diag.narrated().contains(&format!(
                "{ACTION_TAG} parked card \"Title\" awaiting a reply (label \"{AWAITING_LABEL}\")"
            )));
        }
    }

    #[test]
    fn a_parked_card_is_re_armed_by_a_reply_ahead_of_pick_from() {
        let h = Harness::new(park_cfg(), "me");
        seed_park_board(&h.board);
        assert_eq!(park_beat(&h), None);
        assert!(is_badged(&h.board, "card1"));

        h.board
            .seed_comment_named("card1", "reply", PARK_REPLY, "mem-phil", "Phil Ek", 2000);

        let unit = h.poll().expect("the answered card is claimed");
        assert_eq!(unit.card.id, "card1", "ahead of the head of pick_from");
        assert!(h.board.actions().iter().any(
            |a| matches!(a, Action::RemoveLabel { card, label } if card == "card1" && label == AWAITING_LABEL)
        ));
        assert!(!is_badged(&h.board, "card1"));

        let brief = &h.units.wire_unit(&unit).files[0].text;
        let (earlier, new) = brief
            .split_once("## New comments")
            .expect("the brief has a new-comments section");
        assert!(
            new.contains("**Phil Ek:** reading 1 — the **list**. 看 gallery/06:"),
            "{new}"
        );
        assert!(new.contains("drop the label idea entirely."), "{new}");
        assert!(!new.contains("which did you mean?"), "{new}");
        assert!(
            earlier.contains("**afkd:** I can't build this confidently"),
            "{earlier}"
        );
    }

    #[test]
    fn a_resumed_parked_card_is_an_ordinary_fresh_claim() {
        // A re-armed card starts over: both attempts of its budget run, each marked.
        let h = Harness::new(park_cfg(), "me");
        seed_park_board(&h.board);
        park_beat(&h);
        h.board
            .seed_comment_named("card1", "reply", PARK_REPLY, "mem-phil", "Phil Ek", 2000);
        let unit = h.poll().expect("claimed");

        let run = h.run_script(
            &unit,
            &[fault("cargo test: 3 failed"), fault("cargo test: 1 failed")],
        );
        assert_eq!(run.runs, 2, "the resumed card gets both attempts");
        let marks: Vec<String> = h
            .board
            .comments_on("card1")
            .into_iter()
            .filter(|c| c.text.starts_with(ATTEMPT_MARKER))
            .map(|c| c.text)
            .collect();
        assert_eq!(
            marks,
            [
                "[afkd-attempt] 1/2: cargo test: 3 failed",
                "[afkd-attempt] 2/2: cargo test: 1 failed"
            ]
        );
    }

    #[test]
    fn a_discuss_service_re_arms_a_parked_card_from_its_own_last_word() {
        let h = Harness::new(
            BoardConfig {
                on_claim: vec![move_to("In Progress")],
                ..discuss_cfg(DiscussWith::Anyone)
            },
            "me",
        );
        seed_park_board(&h.board);
        park_beat(&h);
        assert!(!h.board.comments_on("card1").iter().any(|c| is_ran(&c.text)));
        assert!(is_badged(&h.board, "card1"));

        for _ in 0..2 {
            assert!(
                h.poll().is_none_or(|u| u.card.id != "card1"),
                "an unanswered parked card is not re-claimed"
            );
        }

        h.board
            .seed_comment_named("card1", "reply", PARK_REPLY, "mem-phil", "Phil Ek", 3000);
        assert_eq!(h.poll_claim().as_deref(), Some("card1"));
    }

    #[test]
    fn a_silent_parked_discuss_turn_backstops_awaiting_a_reply() {
        let h = Harness::new(discuss_cfg(DiscussWith::Anyone), "me");
        seed_source_card(&h.board, "card1");
        h.board
            .seed_comment("card1", "myclaim", &claim_text("me"), 1);

        // Parks without posting anything: the marker alone.
        h.run(&claimed_unit("card1"), 1, |scratch| {
            std::fs::write(scratch.join(PARK_FILE), b"").unwrap();
            fault("parked")
        });

        let comments = h.board.comments_on("card1");
        assert_eq!(replies(&h.board, "card1"), ["awaiting a human reply"]);
        assert_eq!(
            comments
                .iter()
                .filter_map(|c| park_service(&c.text))
                .collect::<Vec<_>>(),
            [TEST_SERVICE],
        );
        let backstop_at = comments
            .iter()
            .position(|c| c.text == "awaiting a human reply")
            .expect("the backstop is on the card");
        let marker_at = comments
            .iter()
            .position(|c| is_park(&c.text))
            .expect("the marker is on the card");
        assert!(
            backstop_at < marker_at,
            "the marker is posted after the backstop"
        );
    }

    // --- The parked scan: what it reads, what it skips, what it costs ---

    /// A board with one badged card parked in `In Progress` and one ordinary card at the
    /// head of `pick_from`; `marker` is posted as afkd's own comment between the
    /// watermark and the human's reply, and `reply` adds the answer.
    fn parked_board_owned_by(h: &Harness, reply: bool, marker: Option<&str>) {
        h.board.add_list("Up for Grabs");
        h.board.add_list("In Progress");
        h.board
            .add_card("In Progress", "parked", "Fix the retry backoff", "Body");
        h.board.seed_card_label("parked", AWAITING_LABEL);
        h.board
            .add_card("Up for Grabs", "queued", "Next up", "Body");
        h.board.seed_comment_named(
            "parked",
            "h0",
            "the retry backoff is wrong",
            "mem-phil",
            "Phil Ek",
            100,
        );
        h.board
            .seed_comment_by("parked", "ask", PARK_QUESTION, SELF_ID, 200);
        h.board.seed_comment(
            "parked",
            "ran",
            &ran_text("me", UNIX_EPOCH + Duration::from_secs(200)),
            210,
        );
        if let Some(marker) = marker {
            h.board
                .seed_comment_by("parked", "park", marker, SELF_ID, 220);
        }
        if reply {
            h.board
                .seed_comment_named("parked", "reply", PARK_REPLY, "mem-phil", "Phil Ek", 300);
        }
        h.board.set_clock(1000);
    }

    fn parked_board(h: &Harness, reply: bool) {
        parked_board_owned_by(h, reply, None);
    }

    #[test]
    fn a_badged_card_with_no_reply_is_left_alone_and_the_beat_claims_on() {
        let h = Harness::new(cfg(vec![move_to("In Progress")], vec![], vec![]), "me");
        parked_board(&h, false);

        assert_eq!(h.poll_claim().as_deref(), Some("queued"));
        assert_eq!(h.board.card_reads(), ["parked"], "one full read, no more");
        assert!(is_badged(&h.board, "parked"));
        assert!(!h
            .board
            .actions()
            .iter()
            .any(|a| matches!(a, Action::RemoveLabel { card, .. } if card == "parked")));
        assert!(h
            .board
            .comments_on("parked")
            .iter()
            .all(|c| !is_claim(&c.text)));
    }

    #[test]
    fn an_archived_badged_card_is_never_scanned() {
        let h = Harness::new(cfg(vec![], vec![], vec![]), "me");
        parked_board(&h, true);
        h.board.archive_card("parked").unwrap();

        assert_eq!(h.poll_claim().as_deref(), Some("queued"));
        assert!(h.board.card_reads().is_empty());
    }

    #[test]
    fn a_card_gone_between_the_two_reads_is_skipped() {
        let h = Harness::new(cfg(vec![], vec![], vec![]), "me");
        parked_board(&h, true);
        h.board.vanish_on_read("parked");

        assert_eq!(h.poll_claim().as_deref(), Some("queued"));
        assert_eq!(h.board.card_reads(), ["parked"]);
        assert!(h
            .board
            .comments_on("parked")
            .iter()
            .all(|c| !is_claim(&c.text)));
    }

    #[test]
    fn a_badged_card_whose_read_fails_stays_badged_and_the_beat_polls_on() {
        let h = Harness::new(cfg(vec![], vec![], vec![]), "me");
        h.board.add_list("Up for Grabs");
        h.board.add_list("In Progress");
        h.board
            .add_card("In Progress", "parked", "Fix the retry backoff", "Body");
        h.board.seed_card_label("parked", AWAITING_LABEL);
        h.board
            .add_card("Up for Grabs", "queued", "Next up", "Body");
        h.board.set_clock(1000);
        h.board.fail("read card");

        assert_eq!(
            h.poll_claim().as_deref(),
            Some("queued"),
            "the beat polls on"
        );
        assert_eq!(
            h.diag.errs(),
            ["trello read card: no response (mock failure)"]
        );
        h.board.clear_failure();
        assert!(
            is_badged(&h.board, "parked"),
            "it stays badged, to be retried"
        );
    }

    #[test]
    fn a_badged_card_carrying_a_denied_label_is_skipped() {
        let h = Harness::new(
            BoardConfig {
                without_label: vec!["Hold".into()],
                require_label: Some("Ready".into()),
                ..cfg(vec![], vec![], vec![])
            },
            "me",
        );
        parked_board(&h, true);
        h.board.seed_card_label("parked", "Hold");
        h.board.seed_card_label("queued", "Ready");

        assert_eq!(h.poll_claim().as_deref(), Some("queued"));
        assert_eq!(h.board.card_reads(), ["parked"], "read, then vetoed");
        assert!(h
            .board
            .comments_on("parked")
            .iter()
            .all(|c| !is_claim(&c.text)));

        // The gate it does not re-apply: drop the veto and the parked card claims,
        // though it carries no `Ready`.
        let h2 = Harness::new(
            BoardConfig {
                require_label: Some("Ready".into()),
                ..cfg(vec![], vec![], vec![])
            },
            "me",
        );
        parked_board(&h2, true);
        h2.board.seed_card_label("queued", "Ready");
        assert_eq!(h2.poll_claim().as_deref(), Some("parked"));
    }

    #[test]
    fn a_badged_card_under_a_live_claim_is_stepped_over() {
        let h = Harness::new(cfg(vec![], vec![], vec![]), "me");
        parked_board(&h, true);
        h.board.seed_comment_by(
            "parked",
            "rival",
            &claim_text("other-host"),
            "mem-other",
            live_secs(5),
        );

        assert_eq!(h.poll_claim().as_deref(), Some("queued"));
        assert_eq!(h.board.card_reads(), ["parked"]);
    }

    #[test]
    fn a_badged_card_dragged_into_pick_from_is_claimed_once_and_unbadged() {
        let h = Harness::new(cfg(vec![], vec![], vec![]), "me");
        h.board.add_list("Up for Grabs");
        h.board
            .add_card("Up for Grabs", "parked", "Fix the retry backoff", "Body");
        h.board.seed_card_label("parked", AWAITING_LABEL);
        h.board
            .seed_comment_by("parked", "ask", PARK_QUESTION, SELF_ID, 200);
        h.board.set_clock(1000);

        assert_eq!(h.poll_claim().as_deref(), Some("parked"));
        assert_eq!(
            h.board
                .comments_on("parked")
                .iter()
                .filter(|c| is_claim(&c.text))
                .count(),
            1,
            "claimed once, by one scan or the other — never both"
        );
        assert_eq!(
            h.board
                .actions()
                .iter()
                .filter(
                    |a| matches!(a, Action::RemoveLabel { label, .. } if label == AWAITING_LABEL)
                )
                .count(),
            1,
        );
        assert!(!is_badged(&h.board, "parked"));
    }

    // --- Park ownership: a parked card goes back to the service that parked it ---

    #[test]
    fn a_parked_card_is_resumed_only_by_the_service_that_parked_it() {
        let board = Arc::new(MockBoard::new());
        let roster = [TEST_SERVICE, OTHER_SERVICE];
        let dev = Harness::on_board(Arc::clone(&board), park_cfg(), "me", TEST_SERVICE, &roster);
        seed_park_board(&board);
        board.add_list("Discussion");
        let discuss = Harness::on_board(
            Arc::clone(&board),
            BoardConfig {
                pick_from: "Discussion".into(),
                ..park_cfg()
            },
            "me",
            OTHER_SERVICE,
            &roster,
        );

        park_beat(&dev);
        assert_eq!(
            board
                .comments_on("card1")
                .iter()
                .filter_map(|c| park_service(&c.text).map(str::to_string))
                .collect::<Vec<_>>(),
            [TEST_SERVICE],
        );
        board.seed_comment_named("card1", "reply", PARK_REPLY, "mem-phil", "Phil Ek", 2000);

        let before_comments = board.comments_on("card1");
        let before_actions = board.actions();
        assert_eq!(discuss.poll_claim(), None, "the discusser claims nothing");
        assert_eq!(
            board.comments_on("card1"),
            before_comments,
            "writes nothing"
        );
        assert_eq!(board.actions(), before_actions);
        assert!(is_badged(&board, "card1"));

        assert_eq!(dev.poll_claim().as_deref(), Some("card1"));
    }

    #[test]
    fn without_the_owner_marker_the_rival_service_takes_the_card() {
        let board = Arc::new(MockBoard::new());
        let roster = [TEST_SERVICE, OTHER_SERVICE];
        let dev = Harness::on_board(Arc::clone(&board), park_cfg(), "me", TEST_SERVICE, &roster);
        seed_park_board(&board);
        board.add_list("Discussion");
        let discuss = Harness::on_board(
            Arc::clone(&board),
            BoardConfig {
                pick_from: "Discussion".into(),
                ..park_cfg()
            },
            "me",
            OTHER_SERVICE,
            &roster,
        );
        park_beat(&dev);
        board.seed_comment_named("card1", "reply", PARK_REPLY, "mem-phil", "Phil Ek", 2000);
        let marker = board
            .comments_on("card1")
            .into_iter()
            .find(|c| is_park(&c.text))
            .expect("the park wrote a marker");
        board.delete_comment("card1", &marker.id).unwrap();

        assert_eq!(discuss.poll_claim().as_deref(), Some("card1"));
    }

    #[test]
    fn instance_copies_of_one_service_are_one_park_owner() {
        for (parked_by, resumer) in [
            ("afkd::develop", "afkd::develop+1"),
            ("afkd::develop+1", "afkd::develop"),
            ("afkd::develop+1", "afkd::develop+2"),
        ] {
            let h = Harness::on_board(
                Arc::new(MockBoard::new()),
                cfg(vec![], vec![], vec![]),
                "me",
                // afkd strips the resumer's own name before `hello`; the marker's is
                // stripped here, on the read side.
                instance_base(resumer).unwrap_or(resumer),
                &[TEST_SERVICE, OTHER_SERVICE],
            );
            parked_board_owned_by(&h, true, Some(&park_text(parked_by)));
            assert_eq!(
                h.poll_claim().as_deref(),
                Some("parked"),
                "{resumer} must resume what {parked_by} parked"
            );
        }
    }

    #[test]
    fn a_parked_card_naming_no_owner_is_claimed_by_any_service() {
        for marker in [
            None,
            Some(PARK_MARKER.to_string()),
            Some(format!("{PARK_MARKER} service=")),
        ] {
            let h = Harness::on_board(
                Arc::new(MockBoard::new()),
                cfg(vec![], vec![], vec![]),
                "me",
                OTHER_SERVICE,
                &[TEST_SERVICE, OTHER_SERVICE],
            );
            parked_board_owned_by(&h, true, marker.as_deref());
            assert_eq!(h.poll_claim().as_deref(), Some("parked"), "{marker:?}");
        }
    }

    fn two_service_identity(service: &str) -> Identity {
        Identity {
            owner: "me".into(),
            service: service.into(),
            roster: vec![TEST_SERVICE.into(), OTHER_SERVICE.into()],
        }
    }

    #[test]
    fn a_park_marker_naming_a_vanished_service_is_taken_over_with_one_line() {
        let h = Harness::as_service(
            Arc::new(MockBoard::new()),
            cfg(vec![], vec![], vec![]),
            two_service_identity(TEST_SERVICE),
        );
        h.board.add_list("Up for Grabs");
        h.board.add_list("In Progress");
        h.board
            .add_card("In Progress", "parked", NARRATED_TITLE, "Body");
        h.board.seed_card_label("parked", AWAITING_LABEL);
        h.board
            .seed_comment_by("parked", "ask", PARK_QUESTION, SELF_ID, 200);
        h.board.seed_comment(
            "parked",
            "ran",
            &ran_text("me", UNIX_EPOCH + Duration::from_secs(200)),
            210,
        );
        h.board
            .seed_comment_by("parked", "park", &park_text("afkd::groom"), SELF_ID, 220);
        h.board
            .seed_comment_named("parked", "reply", PARK_REPLY, "mem-phil", "Phil Ek", 300);
        h.board.set_clock(1000);

        assert_eq!(h.poll_claim().as_deref(), Some("parked"));
        let handovers: Vec<String> = h
            .diag
            .narrated()
            .into_iter()
            .filter(|l| l.contains("no longer runs"))
            .collect();
        assert_eq!(
            handovers,
            [orphan_line(&narrated_card(NARRATED_TITLE), "afkd::groom")]
        );
        assert!(
            handovers[0].contains(NARRATED_TITLE) && handovers[0].contains("afkd::groom"),
            "{}",
            handovers[0]
        );
        assert!(h.diag.errs().is_empty(), "a handover is not a problem");
    }

    #[test]
    fn a_card_owned_by_a_live_sibling_is_swept_past_in_silence() {
        let h = Harness::as_service(
            Arc::new(MockBoard::new()),
            cfg(vec![], vec![], vec![]),
            two_service_identity(OTHER_SERVICE),
        );
        parked_board_owned_by(&h, true, Some(&park_text(TEST_SERVICE)));
        h.board.archive_card("queued").unwrap();

        for _ in 0..3 {
            assert!(h.poll().is_none());
        }
        assert!(h.diag.narrated().is_empty(), "{:?}", h.diag.narrated());
        assert!(h.diag.errs().is_empty(), "{:?}", h.diag.errs());
    }

    #[test]
    fn a_park_leaves_exactly_one_owner_marker_and_the_reclaim_removes_it() {
        let h = Harness::new(park_cfg(), "me");
        seed_park_board(&h.board);
        h.board
            .seed_comment_by("card1", "stale-park", &park_text("afkd::old"), SELF_ID, 150);

        park_beat(&h);

        let markers: Vec<Comment> = h
            .board
            .comments_on("card1")
            .into_iter()
            .filter(|c| is_park(&c.text))
            .collect();
        assert_eq!(
            texts(&markers),
            [park_text(TEST_SERVICE)],
            "exactly one marker, and it is this service's"
        );
        assert!(h.board.actions().iter().any(|a| matches!(
            a,
            Action::DeleteComment { card, comment } if card == "card1" && comment == "stale-park"
        )));

        let live = markers[0].id.clone();
        h.board
            .seed_comment_named("card1", "reply", PARK_REPLY, "mem-phil", "Phil Ek", 2000);
        assert_eq!(h.poll_claim().as_deref(), Some("card1"));
        assert!(!is_badged(&h.board, "card1"));
        assert!(h.board.actions().iter().any(|a| matches!(
            a,
            Action::DeleteComment { card, comment } if card == "card1" && *comment == live
        )));
        assert!(h
            .board
            .comments_on("card1")
            .iter()
            .all(|c| !is_park(&c.text)));
    }

    #[test]
    fn a_re_park_deletes_each_marker_exactly_once() {
        let h = Harness::new(park_cfg(), "me");
        seed_park_board(&h.board);

        park_beat(&h);
        let first = h
            .board
            .comments_on("card1")
            .into_iter()
            .find(|c| is_park(&c.text))
            .expect("the park wrote a marker");
        h.board
            .seed_comment_named("card1", "reply", PARK_REPLY, "mem-phil", "Phil Ek", 2000);
        assert_eq!(park_beat(&h), None);

        let markers: Vec<Comment> = h
            .board
            .comments_on("card1")
            .into_iter()
            .filter(|c| is_park(&c.text))
            .collect();
        assert_eq!(texts(&markers), [park_text(TEST_SERVICE)]);
        assert_ne!(markers[0].id, first.id, "the second park posts its own");

        let deletes: Vec<String> = h
            .board
            .actions()
            .into_iter()
            .filter_map(|a| match a {
                Action::DeleteComment { card, comment } if card == "card1" => Some(comment),
                _ => None,
            })
            .collect();
        assert_eq!(deletes.iter().filter(|c| **c == first.id).count(), 1);
        let mut once = deletes.clone();
        once.sort();
        once.dedup();
        assert_eq!(
            once.len(),
            deletes.len(),
            "no id deleted twice: {deletes:?}"
        );
        assert!(h.diag.errs().is_empty(), "{:?}", h.diag.errs());
    }

    #[test]
    fn the_park_marker_never_reaches_a_brief_or_the_feedback_delta() {
        let thread = vec![
            self_comment("ask", PARK_QUESTION, 200),
            comment("reply", PARK_REPLY, 300),
            self_comment("park", &park_text(TEST_SERVICE), 310),
        ];
        let delta = feedback_comments(&thread, SELF_ID);
        assert_eq!(
            delta.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            ["reply"]
        );
        let brief = brief_text(&narrated_card(NARRATED_TITLE), &thread, SELF_ID);
        assert!(!brief.contains(PARK_MARKER), "{brief}");
        assert!(brief.contains("drop the label idea entirely."), "{brief}");
    }

    #[test]
    fn the_park_marker_is_not_the_discuss_tail_boundary() {
        let gate = DiscussGate {
            self_id: SELF_ID.to_string(),
            anyone: true,
            allowed: HashSet::new(),
        };
        let answered = vec![
            self_comment("ask", PARK_QUESTION, 200),
            comment("reply", PARK_REPLY, 300),
            self_comment("park", &park_text(TEST_SERVICE), 310),
        ];
        assert!(discuss_tail_passes(&answered, &gate));
        // Were the marker counted as speech, nothing would ever be newer than it.
        let spoke_incl_marker = answered
            .iter()
            .filter(|c| c.author == SELF_ID)
            .map(|c| c.posted_at)
            .max()
            .unwrap();
        assert!(!answered
            .iter()
            .any(|c| c.posted_at > spoke_incl_marker && c.author != SELF_ID));
        let unanswered = vec![
            self_comment("ask", PARK_QUESTION, 200),
            self_comment("park", &park_text(TEST_SERVICE), 210),
        ];
        assert!(!discuss_tail_passes(&unanswered, &gate));
    }

    /// A badged, unanswered card at the head of `pick_from`, with `marker` under it: the
    /// sweep stops at its feedback gate, so only the list scan can reach it.
    fn badged_card_in_pick_from(board: &MockBoard, marker: Option<&str>) {
        board.add_list("Up for Grabs");
        board.add_list("In Progress");
        board.add_card("Up for Grabs", "parked", NARRATED_TITLE, "Body");
        board.seed_card_label("parked", AWAITING_LABEL);
        board.seed_comment_by("parked", "ask", PARK_QUESTION, SELF_ID, 200);
        if let Some(marker) = marker {
            board.seed_comment_by("parked", "park", marker, SELF_ID, 220);
        }
        board.set_clock(1000);
    }

    #[test]
    fn a_badged_card_in_another_services_pick_from_is_left_for_its_owner() {
        let h = Harness::on_board(
            Arc::new(MockBoard::new()),
            cfg(vec![], vec![], vec![]),
            "me",
            OTHER_SERVICE,
            &[TEST_SERVICE, OTHER_SERVICE],
        );
        badged_card_in_pick_from(&h.board, Some(&park_text(TEST_SERVICE)));
        let before = h.board.comments_on("parked");

        assert_eq!(h.poll_claim(), None);
        assert_eq!(h.board.comments_on("parked"), before);
        assert!(!h.board.actions().iter().any(|a| matches!(
            a,
            Action::RemoveLabel { label, .. } if label == AWAITING_LABEL
        )));
        assert!(!h
            .board
            .actions()
            .iter()
            .any(|a| matches!(a, Action::DeleteComment { .. })));
        assert!(is_badged(&h.board, "parked"));
        assert_eq!(
            park_owner(&h.board.comments_on("parked")),
            Some(TEST_SERVICE)
        );
    }

    #[test]
    fn the_parking_service_claims_its_own_badged_card_from_its_own_pick_from() {
        for parked_by in [
            TEST_SERVICE,
            &format!("{TEST_SERVICE}+1"),
            &format!("{TEST_SERVICE}+12"),
        ] {
            let h = Harness::on_board(
                Arc::new(MockBoard::new()),
                cfg(vec![], vec![], vec![]),
                "me",
                TEST_SERVICE,
                &[TEST_SERVICE, OTHER_SERVICE],
            );
            badged_card_in_pick_from(&h.board, Some(&park_text(parked_by)));

            assert_eq!(h.poll_claim().as_deref(), Some("parked"), "{parked_by}");
            assert!(!is_badged(&h.board, "parked"), "{parked_by}");
            assert!(h
                .board
                .comments_on("parked")
                .iter()
                .all(|c| !is_park(&c.text)));
        }
    }

    #[test]
    fn an_unmarked_badged_card_in_another_services_pick_from_is_still_claimed() {
        let h = Harness::on_board(
            Arc::new(MockBoard::new()),
            cfg(vec![], vec![], vec![]),
            "me",
            OTHER_SERVICE,
            &[TEST_SERVICE, OTHER_SERVICE],
        );
        badged_card_in_pick_from(&h.board, None);

        assert_eq!(h.poll_claim().as_deref(), Some("parked"));
        assert!(!is_badged(&h.board, "parked"));
    }

    #[test]
    fn the_newest_park_marker_decides_ownership_whatever_order_the_board_returns() {
        for (newest, claimed) in [(TEST_SERVICE, false), ("afkd::gone", true)] {
            for newest_first in [false, true] {
                let stale = if newest == TEST_SERVICE {
                    "afkd::gone"
                } else {
                    TEST_SERVICE
                };
                let h = Harness::on_board(
                    Arc::new(MockBoard::new()),
                    cfg(vec![], vec![], vec![]),
                    "me",
                    OTHER_SERVICE,
                    &[TEST_SERVICE, OTHER_SERVICE],
                );
                badged_card_in_pick_from(&h.board, None);
                let (first, second) = if newest_first {
                    (("new", newest, 300), ("old", stale, 220))
                } else {
                    (("old", stale, 220), ("new", newest, 300))
                };
                for (id, service, at) in [first, second] {
                    h.board
                        .seed_comment_by("parked", id, &park_text(service), SELF_ID, at);
                }

                let label = format!("newest {newest}, board order first={}", first.1);
                assert_eq!(
                    park_owner(&h.board.comments_on("parked")),
                    Some(newest),
                    "{label}"
                );
                assert_eq!(h.poll_claim().is_some(), claimed, "{label}");
            }
        }
    }

    #[test]
    fn an_orphaned_parked_card_narrates_the_same_takeover_from_either_route() {
        let gone = "afkd::groom";

        // Route 1 — the parked sweep: answered, and sitting in `In Progress`.
        let swept = Harness::as_service(
            Arc::new(MockBoard::new()),
            cfg(vec![], vec![], vec![]),
            two_service_identity(TEST_SERVICE),
        );
        badged_card_in_pick_from(&swept.board, Some(&park_text(gone)));
        swept
            .board
            .move_card("parked", "In Progress", ListPosition::Top)
            .unwrap();
        swept.board.seed_comment(
            "parked",
            "ran",
            &ran_text("me", UNIX_EPOCH + Duration::from_secs(200)),
            210,
        );
        swept
            .board
            .seed_comment_named("parked", "reply", PARK_REPLY, "mem-phil", "Phil Ek", 300);

        // Route 2 — the ordinary list scan: unanswered, sitting in `pick_from`.
        let scanned = Harness::as_service(
            Arc::new(MockBoard::new()),
            cfg(vec![], vec![], vec![]),
            two_service_identity(TEST_SERVICE),
        );
        badged_card_in_pick_from(&scanned.board, Some(&park_text(gone)));

        let mut lines = Vec::new();
        for h in [&swept, &scanned] {
            assert_eq!(h.poll_claim().as_deref(), Some("parked"));
            let handovers: Vec<String> = h
                .diag
                .narrated()
                .into_iter()
                .filter(|l| l.contains("no longer runs"))
                .collect();
            assert_eq!(handovers.len(), 1, "{:?}", h.diag.narrated());
            assert!(h.diag.errs().is_empty(), "{:?}", h.diag.errs());
            lines.push(handovers.into_iter().next().unwrap());
        }
        assert_eq!(lines[0], lines[1], "one line, not two spellings");
        assert_eq!(lines[0], orphan_line(&narrated_card(NARRATED_TITLE), gone));
    }

    #[test]
    fn an_unbadged_poll_asks_the_board_exactly_what_it_asked_before() {
        let plain = Harness::new(cfg(vec![], vec![], vec![]), "me");
        seed_source_card(&plain.board, "card1");
        plain
            .board
            .add_card("Up for Grabs", "card2", "Next up", "Body");
        plain.board.set_clock(1000);
        assert_eq!(plain.poll_claim().as_deref(), Some("card1"));
        assert_eq!(
            plain.board.calls(),
            [
                "board cards",
                "resolve list",
                "list cards",
                "post comment",
                "read comments",
            ],
        );

        let badged = Harness::new(cfg(vec![], vec![], vec![]), "me");
        seed_source_card(&badged.board, "card1");
        badged
            .board
            .add_card("Up for Grabs", "card2", "Next up", "Body");
        badged.board.seed_card_label("card1", AWAITING_LABEL);
        badged
            .board
            .seed_comment_by("card1", "park", &park_text(TEST_SERVICE), SELF_ID, 200);
        badged.board.set_clock(1000);
        assert_eq!(badged.poll_claim().as_deref(), Some("card1"));
        assert_eq!(
            badged
                .board
                .calls()
                .iter()
                .filter(|c| **c == "read comments")
                .count(),
            2,
            "the ownership read, then the claim's re-read: {:?}",
            badged.board.calls()
        );
    }

    #[test]
    fn forty_badged_cards_are_read_sixteen_a_beat_and_all_within_three() {
        let h = Harness::new(cfg(vec![], vec![], vec![]), "me");
        h.board.add_list("Up for Grabs");
        h.board.add_list("In Progress");
        for n in 0..40 {
            let id = format!("p{n:02}");
            h.board.add_card("In Progress", &id, "Parked", "Body");
            h.board.seed_card_label(&id, AWAITING_LABEL);
            h.board
                .seed_comment_by(&id, &format!("ask-{id}"), PARK_QUESTION, SELF_ID, 200);
        }
        h.board.set_clock(1000);

        assert_eq!(h.poll_claim(), None);
        assert_eq!(h.board.card_reads().len(), PARK_SCAN_MAX);
        assert_eq!(
            h.board.card_reads(),
            (0..16).map(|n| format!("p{n:02}")).collect::<Vec<_>>(),
        );
        assert_eq!(h.poll_claim(), None);
        assert_eq!(h.poll_claim(), None);
        let reached: HashSet<String> = h.board.card_reads().into_iter().collect();
        assert_eq!(reached.len(), 40);
        assert_eq!(h.board.card_reads().len(), 3 * PARK_SCAN_MAX);
    }

    // --- `on_park` extras, and the park's own delivery verdict ---

    #[test]
    fn on_park_actions_run_after_the_badge() {
        let h = Harness::new(
            BoardConfig {
                on_park: vec![
                    move_to("Discussion"),
                    comment_action("parked after @{run:duration} — @{run:turns} turns"),
                ],
                ..park_cfg()
            },
            "me",
        );
        seed_park_board(&h.board);
        h.board.add_list("Discussion");
        h.beat(
            2,
            ask(
                &h.board,
                "card1",
                Facts {
                    signal: "fault".into(),
                    reason: Some("parked: awaiting a human reply".into()),
                    ..measured("260906-085008-card-U9VjXLga-1")
                },
            ),
        );

        let ordered: Vec<String> = h
            .board
            .actions()
            .iter()
            .filter_map(|a| match a {
                Action::Move { list, .. } => Some(format!("move {list}")),
                Action::AddLabel { label, .. } => Some(format!("label {label}")),
                _ => None,
            })
            .collect();
        assert_eq!(
            ordered,
            [
                "move In Progress".to_string(),
                format!("label {AWAITING_LABEL}"),
                "move Discussion".to_string()
            ]
        );
        assert!(h
            .board
            .comments_on("card1")
            .iter()
            .any(|c| c.text == "parked after 5.00s — 3 turns"));
    }

    /// The park whose badge cannot reach the board, driven through one beat; `extras`
    /// are the `on_park` block. The key the `held` finish leaves.
    fn a_park_whose_badge_cannot_land_with(extras: Vec<LifecycleAction>) -> (Harness, String) {
        let h = Harness::new(
            BoardConfig {
                on_park: extras,
                ..park_cfg()
            },
            "me",
        );
        seed_park_board(&h.board);
        h.board.add_list("Discussion");
        h.board.fail("add label");
        let held = park_beat(&h).expect("the park is undelivered");
        (h, held)
    }

    fn a_park_whose_badge_cannot_land() -> (Harness, String) {
        a_park_whose_badge_cannot_land_with(vec![move_to("Discussion")])
    }

    #[test]
    fn a_park_with_no_extras_whose_badge_fails_still_holds_the_claim() {
        let (h, held) = a_park_whose_badge_cannot_land_with(Vec::new());
        assert_eq!(held, "card1#c1000");
        assert!(h
            .board
            .comments_on("card1")
            .iter()
            .any(|c| is_claim(&c.text)));

        h.board.clear_failure();
        assert_eq!(h.release(&held), Some(true));
        assert_eq!(
            h.board
                .actions()
                .iter()
                .filter_map(|a| match a {
                    Action::AddLabel { label, .. } => Some(format!("label {label}")),
                    Action::DeleteComment { .. } => Some("release".to_string()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            [format!("label {AWAITING_LABEL}"), "release".to_string()],
        );
        assert!(is_badged(&h.board, "card1"));
    }

    #[test]
    fn a_park_whose_badge_fails_is_undelivered_and_holds_the_claim() {
        let (h, _) = a_park_whose_badge_cannot_land();
        assert!(h
            .board
            .comments_on("card1")
            .iter()
            .any(|c| is_claim(&c.text)));
        assert!(
            !has_move_to(&h.board, "Discussion"),
            "the extras wait for the badge"
        );
        assert!(h
            .board
            .comments_on("card1")
            .iter()
            .any(|c| c.text == PARK_QUESTION));
    }

    #[test]
    fn a_recovered_board_delivers_the_owed_badge_then_the_extras() {
        let (h, held) = a_park_whose_badge_cannot_land();
        h.board.clear_failure();

        assert_eq!(h.release(&held), Some(true));
        assert_eq!(
            h.board
                .actions()
                .iter()
                .filter_map(|a| match a {
                    Action::Move { list, .. } => Some(format!("move {list}")),
                    Action::AddLabel { label, .. } => Some(format!("label {label}")),
                    Action::DeleteComment { .. } => Some("release".to_string()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            [
                "move In Progress".to_string(),
                format!("label {AWAITING_LABEL}"),
                "move Discussion".to_string(),
                "release".to_string(),
            ],
        );
    }

    #[test]
    fn a_park_posts_the_owner_marker_before_it_releases_the_claim() {
        let h = Harness::new(park_cfg(), "me");
        seed_park_board(&h.board);

        park_beat(&h);

        assert_eq!(
            h.board
                .calls()
                .into_iter()
                .filter(|c| matches!(*c, "post comment" | "delete comment"))
                .collect::<Vec<_>>(),
            [
                "post comment",   // the claim
                "post comment",   // the agent's question
                "post comment",   // the `[afkd-ran]` watermark
                "post comment",   // the `[afkd-park]` owner marker
                "delete comment"  // …and only then the lease
            ],
        );
        let markers: Vec<String> = h
            .board
            .comments_on("card1")
            .into_iter()
            .filter(|c| is_ran(&c.text) || is_park(&c.text))
            .map(|c| c.text)
            .collect();
        assert_eq!(
            markers,
            [
                ran_text("me", UNIX_EPOCH + Duration::from_secs(100)),
                park_text(TEST_SERVICE)
            ],
        );
        assert!(h
            .board
            .comments_on("card1")
            .iter()
            .all(|c| !is_claim(&c.text)));
        assert!(is_badged(&h.board, "card1"));
    }

    #[test]
    fn a_park_whose_marker_cannot_land_holds_the_claim_until_the_release_posts_it() {
        let h = Harness::new(park_cfg(), "me");
        seed_park_board(&h.board);
        h.board.fail_post_matching(PARK_MARKER);

        let held = park_beat(&h).expect("the park is undelivered");
        assert!(h
            .board
            .comments_on("card1")
            .iter()
            .any(|c| is_claim(&c.text)));
        assert!(h
            .board
            .comments_on("card1")
            .iter()
            .all(|c| !is_park(&c.text)));
        assert!(is_badged(&h.board, "card1"));
        let posted = h.board.comments_on("card1");
        assert!(
            posted.iter().any(|c| c.text == PARK_QUESTION)
                && posted.iter().any(|c| is_ran(&c.text))
        );

        let before = h.board.calls().len();
        h.board.clear_failure();
        assert_eq!(h.release(&held), Some(true));
        assert_eq!(
            h.board.calls()[before..]
                .iter()
                .copied()
                .filter(|c| matches!(*c, "post comment" | "delete comment"))
                .collect::<Vec<_>>(),
            ["post comment", "delete comment"],
            "the marker, then the lease"
        );
        assert_eq!(
            h.board
                .comments_on("card1")
                .into_iter()
                .filter(|c| is_park(&c.text))
                .map(|c| c.text)
                .collect::<Vec<_>>(),
            [park_text(TEST_SERVICE)],
        );
        assert!(h
            .board
            .comments_on("card1")
            .iter()
            .all(|c| !is_claim(&c.text)));
    }

    #[test]
    fn a_park_owed_five_beats_returns_the_card_to_pick_from_with_the_question_on_it() {
        let (h, held) = a_park_whose_badge_cannot_land();
        for _ in 0..PENDING_FINISH_TRIES {
            h.release(&held);
        }
        assert!(h
            .diag
            .errs()
            .iter()
            .any(|l| l.contains("giving up on the terminal lifecycle")));
        assert!(h.board.actions().iter().any(|a| matches!(
            a,
            Action::Move { card, list, position }
                if card == "card1" && list == "Up for Grabs" && *position == ListPosition::Bottom
        )));
        let comments = h.board.comments_on("card1");
        assert!(comments.iter().all(|c| !is_claim(&c.text)));
        assert!(comments.iter().any(|c| c.text == PARK_QUESTION));
    }

    // --- Keeping the claim alive for the life of the run ---

    #[test]
    fn a_card_with_an_edited_claim_is_stepped_over() {
        let h = Harness::new(cfg(vec![], vec![], vec![]), "me");
        seed_source_card(&h.board, "card1");
        h.board.nest_comments("card1");
        h.board.seed_comment_renewed(
            "card1",
            "rival1",
            &claim_text("other"),
            live_secs(36_000),
            live_secs(60),
        );
        h.board.set_clock(live_secs(0));

        assert!(
            h.poll().is_none(),
            "we claimed a card a live run is holding"
        );
        assert_eq!(h.board.actions(), vec![]);
        let ids: Vec<String> = h
            .board
            .comments_on("card1")
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(ids, ["rival1"]);
    }

    #[test]
    fn an_edited_claim_loses_us_the_race_past_the_pre_scan() {
        let h = Harness::new(cfg(vec![], vec![], vec![]), "me");
        seed_source_card(&h.board, "card1");
        h.board.nest_comments("card1");
        h.board
            .seed_comment_renewed("card1", "rival1", &claim_text("other"), 500, 39_940);
        h.board.set_clock(40_000);

        assert!(h.poll().is_none());
        let ids: Vec<String> = h
            .board
            .comments_on("card1")
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(ids, ["rival1"]);
        assert!(deleted(&h.board, "c40000"));
    }

    #[test]
    fn renewing_a_claim_edits_the_marker_in_place() {
        let owner = "afkd-4242";
        let h = Harness::new(cfg(vec![], vec![], vec![]), owner);
        seed_source_card(&h.board, "card1");
        h.board
            .seed_comment("card1", "myclaim", &claim_text(owner), 1);
        h.board.set_clock(50_000);

        h.units.renew(&claimed_unit("card1"), 3, &h.diag);

        let renewed = claim_renewal_text(owner, 3);
        assert_eq!(
            h.board.actions(),
            vec![Action::EditComment {
                card: "card1".into(),
                comment: "myclaim".into(),
                text: renewed.clone(),
            }],
        );
        let comments = h.board.comments_on("card1");
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].id, "myclaim");
        assert_eq!(comments[0].text, renewed);
        assert!(is_claim(&comments[0].text));
        assert!(comments[0].renewed_at > comments[0].posted_at);
    }

    #[test]
    fn a_failing_renewal_raises_one_diagnostic_and_nothing_else() {
        let h = Harness::new(cfg(vec![], vec![], vec![]), "me");
        seed_source_card(&h.board, "card1");
        let claim = claim_text("me");
        h.board.seed_comment("card1", "myclaim", &claim, 1);
        h.board.fail("edit comment");

        h.units.renew(&claimed_unit("card1"), 1, &h.diag);

        assert_eq!(
            h.diag.errs(),
            ["trello edit comment: no response (mock failure)"]
        );
        assert_eq!(h.board.comments_on("card1")[0].text, claim);
    }

    // --- The poll's scan budget ---

    /// A list of cards each held by an earlier live rival, so every candidate costs one
    /// lost race — one settle — and a scan over twenty-five of them outruns the budget.
    #[test]
    fn the_list_scan_stops_at_its_budget_and_says_so_once() {
        let h = Harness::new(cfg(vec![], vec![], vec![]), "me");
        h.board.add_list("Up for Grabs");
        for n in 0..25 {
            let id = format!("card{n:02}");
            h.board.add_card("Up for Grabs", &id, "Title", "Body");
            h.board
                .seed_comment(&id, &format!("rival{n}"), &claim_text("rival"), 500);
        }
        h.board.set_clock(1000);

        assert!(h.poll().is_none());
        assert_eq!(
            h.clock.sleeps().len(),
            20,
            "twenty settles fill the 20s budget, and the scan stops there"
        );
        assert_eq!(
            h.diag.errs(),
            [
                "trello poll: the scan ran past its 20s budget; the rest of it waits for the \
              next poll"
            ]
        );
        assert_eq!(
            h.board
                .calls()
                .iter()
                .filter(|c| **c == "post comment")
                .count(),
            20,
            "the twenty-first card is never posted to"
        );
    }

    /// The sweep checks the budget before each card read, so a slow board stops it too,
    /// and the beat ends idle rather than reaching `pick_from`. Every clock read here
    /// costs ten seconds: the first card is read, the second check finds the budget
    /// spent.
    #[test]
    fn the_parked_sweep_stops_at_its_budget() {
        let h = Harness {
            clock: FakeClock::ticking(Duration::from_secs(10)),
            ..Harness::new(cfg(vec![], vec![], vec![]), "me")
        };
        h.board.add_list("Up for Grabs");
        h.board.add_list("In Progress");
        for n in 0..3 {
            let id = format!("p{n}");
            h.board.add_card("In Progress", &id, "Parked", "Body");
            h.board.seed_card_label(&id, AWAITING_LABEL);
            h.board
                .seed_comment_by(&id, &format!("ask-{id}"), PARK_QUESTION, SELF_ID, 200);
        }
        h.board
            .add_card("Up for Grabs", "queued", "Next up", "Body");

        assert!(h.poll().is_none(), "the beat ends idle");
        assert_eq!(h.board.card_reads(), ["p0"], "no card read past the budget");
        assert!(
            !h.board.calls().contains(&"resolve list"),
            "pick_from is not reached: {:?}",
            h.board.calls()
        );
        assert_eq!(h.diag.errs().len(), 1, "{:?}", h.diag.errs());
    }
}
