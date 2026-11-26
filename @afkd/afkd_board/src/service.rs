//! The `board` kind, `service(afkd_board)` in a config: a board whose cards afkd claims one per
//! run, and the calls afkd makes of it: `poll`, `finish` and `release`.
//!
//! **A beat claims at most one card.** `poll` walks `claim_from` in the order written, each list
//! top to bottom, and races for each card [`claimable`] lets through: it posts a claim marker on
//! the card, waits [`CLAIM_SETTLE`], re-reads the card's comments and the card, and wins iff
//! [`won_claim`] names its marker and the card still lacks `afkd/claimed`. The winner puts the
//! label on and hands afkd a unit, keyed by the card handle's key, which afkd journals (ADR-0059)
//! and which `==` compares with a read card.
//!
//! **The label comes off once the card's post-run slot has run.** afkd's beat is `poll`, the
//! pre-run slot, the attempts, `finish`, the post-run slot, the next beat, so `finish` only
//! writes the card down in the owed-release ledger ([`crate::owed`]) and the child's next `poll`
//! takes the label off before it claims anything. A crashed run's label comes off through
//! afkd's `release` of its journalled key at the next start.
//!
//! **Every call answers within [`CALL_BUDGET`].** afkd ends the service on a missed reply, so
//! `poll` starts no new race with too little of it left, and every request is clipped to it.

use std::collections::BTreeMap;
use std::path::Path;
use std::rc::Rc;
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use crate::board::{parse_key, Board};
use crate::claim::{
    claim_text, claimable, is_claim, markers, won_claim, CLAIMED_LABEL, CLAIM_SETTLE, CLAIM_WINDOW,
};
use crate::client::{BoardAnswer, BoardClient, CardAnswer, CommentAnswer, ItemAnswer, CALL_BUDGET};
use crate::common::{Clock, Diag};
use crate::owed::{Entry, Ledger};
use crate::reads::{card_handle, no_list};
use crate::settings::ServiceConfig;
use crate::wire::{fit_poll, idle_line, Fitted, WireFile, WireUnit};

/// A race starts only with this much of the call's [`CALL_BUDGET`] left: ten seconds hold the
/// settle and leave seven for the marker before it and the re-read and the label after it.
pub(crate) const CLAIM_RESERVE: Duration = Duration::from_secs(10);

/// The env the run carries the board's address in.
pub(crate) const ENV_URL: &str = "AFKD_BOARD_URL";
/// The env the run carries its `Authorization` header's value in.
pub(crate) const ENV_AUTHORIZATION: &str = "AFKD_BOARD_AUTHORIZATION";
/// The env the run carries what the plugin knows of the run it will be in: JSON.
pub(crate) const ENV_RUN: &str = "AFKD_BOARD_RUN";

/// Who the service is, as `hello` names it: the configured service, and this afkd process's
/// claim owner, which names the claim marker and tells one afkd process's ledger entries from
/// another's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Identity {
    pub(crate) service: String,
    pub(crate) owner: String,
}

/// The board's API as the service uses it, so the service is tested against a board that is a
/// mock and run against the real one.
pub(crate) trait Api {
    /// The board: its id, its lists in order and its labels.
    fn board(&self) -> Result<BoardAnswer, String>;
    /// The cards of the list `list` that are neither archived nor done, in the list's order.
    fn open_cards(&self, list: &str) -> Result<Vec<CardAnswer>, String>;
    /// Every card carrying `label`, archived ones too. A label the board lacks is refused by
    /// the board, so the caller asks only for one it has.
    fn cards_labelled(&self, label: &str) -> Result<Vec<CardAnswer>, String>;
    /// The card `number`.
    fn card(&self, number: i64) -> Result<CardAnswer, String>;
    /// The card's comments, oldest first.
    fn comments(&self, number: i64) -> Result<Vec<CommentAnswer>, String>;
    /// A comment of `text` by the key, made in no run.
    fn comment(&self, number: i64, text: &str) -> Result<CommentAnswer, String>;
    /// The card with `label` put on, in no run.
    fn add_label(&self, number: i64, label: &str) -> Result<CardAnswer, String>;
    /// The card with `label` taken off, in no run.
    fn remove_label(&self, number: i64, label: &str) -> Result<CardAnswer, String>;
}

/// The real board: a [`BoardClient`] for one call, whose refusals name `action`.
pub(crate) struct Remote {
    client: BoardClient,
    action: &'static str,
}

impl Remote {
    pub(crate) fn new(board: &Board, deadline: Instant, action: &'static str) -> Self {
        Self {
            client: BoardClient::new(board, deadline),
            action,
        }
    }
}

impl Api for Remote {
    fn board(&self) -> Result<BoardAnswer, String> {
        self.client.board(self.action)
    }

    fn open_cards(&self, list: &str) -> Result<Vec<CardAnswer>, String> {
        let query = [
            ("list", list.to_string()),
            ("archived", "false".to_string()),
            ("done", "false".to_string()),
            ("limit", "250".to_string()),
        ];
        self.client.cards(self.action, &query)
    }

    fn cards_labelled(&self, label: &str) -> Result<Vec<CardAnswer>, String> {
        let mut cards = Vec::new();
        for archived in ["false", "true"] {
            let query = [
                ("labels", label.to_string()),
                ("archived", archived.to_string()),
                ("limit", "250".to_string()),
            ];
            cards.extend(self.client.cards(self.action, &query)?);
        }
        Ok(cards)
    }

    fn card(&self, number: i64) -> Result<CardAnswer, String> {
        self.client.card(self.action, number)
    }

    fn comments(&self, number: i64) -> Result<Vec<CommentAnswer>, String> {
        self.client.comments(self.action, number)
    }

    fn comment(&self, number: i64, text: &str) -> Result<CommentAnswer, String> {
        self.client.create_comment(self.action, number, text, None)
    }

    fn add_label(&self, number: i64, label: &str) -> Result<CardAnswer, String> {
        self.client.add_label(self.action, number, label, None)
    }

    fn remove_label(&self, number: i64, label: &str) -> Result<CardAnswer, String> {
        self.client.remove_label(self.action, number, label, None)
    }
}

/// Builds the board client one call is answered through, for the board, the instant the call
/// must be answered by, and the name its refusals carry: the real one in the running plugin, a
/// mock in the tests.
pub(crate) type Connect = Rc<dyn Fn(&Board, Instant, &'static str) -> Box<dyn Api>>;

/// A card this child claimed and afkd has not finished: what `finish` writes down.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Live {
    number: i64,
    board: String,
}

/// The armed service.
pub(crate) struct Service {
    cfg: ServiceConfig,
    id: Identity,
    ledger: Ledger,
    /// This child's own pid, which tells it from the siblings sharing its identity and its
    /// directory.
    pid: u32,
    connect: Connect,
    clock: Rc<dyn Clock>,
    diag: Rc<dyn Diag>,
    /// The units handed over and not yet finished or released, by journal key.
    live: BTreeMap<String, Live>,
}

impl Service {
    /// A service of `cfg` as `id`, keeping its ledger under `state_dir`, as the child `pid`.
    pub(crate) fn new(
        cfg: ServiceConfig,
        id: Identity,
        state_dir: &Path,
        pid: u32,
        connect: Connect,
        clock: Rc<dyn Clock>,
        diag: Rc<dyn Diag>,
    ) -> Self {
        Self {
            cfg,
            id,
            ledger: Ledger::new(state_dir),
            pid,
            connect,
            clock,
            diag,
            live: BTreeMap::new(),
        }
    }

    /// One beat: the owed-release sweep, then the claim race card by card, handing over at most
    /// one unit. A board error or a missing list is an idle beat, diagnosed.
    pub(crate) fn poll(&mut self) -> String {
        let deadline = self.clock.now() + CALL_BUDGET;
        let api = (self.connect)(&self.cfg.board(), deadline, "poll");
        match self.claim_next(&*api, deadline) {
            Ok(Some(claimed)) => self.hand_over(&*api, claimed),
            Ok(None) => idle_line(),
            Err(e) => {
                self.diag.err(&e);
                idle_line()
            }
        }
    }

    /// The sweep, then the first card won of the lists in order.
    fn claim_next(&mut self, api: &dyn Api, deadline: Instant) -> Result<Option<Claimed>, String> {
        let board = api.board()?;
        self.sweep(api, &board);
        for list in &self.cfg.claim_from {
            if !board.lists.iter().any(|l| &l.name == list) {
                return Err(no_list("poll", list, &board.lists));
            }
        }
        for list in self.cfg.claim_from.clone() {
            for card in api.open_cards(&list)? {
                if !claimable(&card, &self.cfg) {
                    continue;
                }
                if deadline.saturating_duration_since(self.clock.now()) < CLAIM_RESERVE {
                    self.diag.err(&format_args!(
                        "poll: less than {}s of the call's {}s budget is left; the rest of the \
                         scan waits for the next poll",
                        CLAIM_RESERVE.as_secs(),
                        CALL_BUDGET.as_secs()
                    ));
                    return Ok(None);
                }
                if let Some((card, comments)) = self.race(api, &card)? {
                    return Ok(Some(Claimed {
                        board: board.id.clone(),
                        card,
                        comments,
                    }));
                }
            }
        }
        Ok(None)
    }

    /// Race for `card`: post the marker, settle, re-read, and on a win put the label on. The card
    /// as labelled and the comments read come back; `None` is a loss.
    fn race(
        &self,
        api: &dyn Api,
        card: &CardAnswer,
    ) -> Result<Option<(CardAnswer, Vec<CommentAnswer>)>, String> {
        let number = card.number;
        let mine = api.comment(number, &claim_text(&self.id.owner))?;
        self.clock.sleep(CLAIM_SETTLE);
        let comments = api.comments(number)?;
        let read = api.card(number)?;
        if !won_claim(&markers(&comments), &mine.id, CLAIM_WINDOW) || !claimable(&read, &self.cfg) {
            self.diag
                .narrate(&format!("[afkd_board] lost the claim of #{number}"));
            return Ok(None);
        }
        let labelled = api.add_label(number, CLAIMED_LABEL).map_err(|e| {
            format!("poll: won the claim of #{number} but could not put {CLAIMED_LABEL} on it: {e}")
        })?;
        Ok(Some((labelled, comments)))
    }

    /// Take the label off every card the ledger owes it to this child, then settle what came off;
    /// a removal that fails stays for the next poll.
    fn sweep(&self, api: &dyn Api, board: &BoardAnswer) {
        let owed = match self.ledger.take(&board.id, &self.id.owner, self.pid) {
            Ok(owed) => owed,
            Err(e) => {
                self.diag
                    .err(&format_args!("poll: cannot read the owed releases: {e}"));
                return;
            }
        };
        let exists = board.labels.iter().any(|l| l.name == CLAIMED_LABEL);
        let mut settled = Vec::new();
        for entry in owed {
            let off = !exists
                || match api.remove_label(entry.number, CLAIMED_LABEL) {
                    Ok(_) => true,
                    Err(e) if is_gone(&e) => true,
                    Err(e) => {
                        self.diag.err(&format_args!(
                            "poll: could not take {CLAIMED_LABEL} off #{}; it stays owed: {e}",
                            entry.number
                        ));
                        false
                    }
                };
            if off {
                self.diag.narrate(&format!(
                    "[afkd_board] released #{} ({CLAIMED_LABEL} is off)",
                    entry.number
                ));
                settled.push(entry);
            }
        }
        if let Err(e) = self.ledger.settle(&settled) {
            self.diag
                .err(&format_args!("poll: cannot settle the owed releases: {e}"));
        }
    }

    /// The unit for the card won, fitted to afkd's line, and remembered as live.
    fn hand_over(&mut self, api: &dyn Api, claimed: Claimed) -> String {
        let Claimed {
            board,
            card,
            comments,
        } = claimed;
        let number = card.number;
        let handle = match card_handle("poll", &board, &card) {
            Ok(handle) => handle,
            Err(e) => {
                self.give_back(api, number, &e);
                return idle_line();
            }
        };
        let unit = WireUnit {
            id: handle.id,
            key: handle.key.clone(),
            fields: handle.fields,
            args: self.slot_args(),
            thread: handle.key.clone(),
            seen: Vec::new(),
            me: self.id.owner.clone(),
            env: self.env(),
            files: vec![WireFile {
                path: crate::TASK_FILE.to_string(),
                text: brief(&card, &comments),
            }],
        };
        let line = match fit_poll(&unit) {
            Some(Fitted {
                line,
                description_cut,
                brief_cut,
            }) => {
                if description_cut {
                    self.diag.err(&format_args!(
                        "the description field of #{number} was cut to fit afkd's 64 KiB \
                         plugin line"
                    ));
                }
                if brief_cut {
                    self.diag.err(&format_args!(
                        "the brief for #{number} was cut to fit afkd's 64 KiB plugin line"
                    ));
                }
                line
            }
            None => {
                self.give_back(
                    api,
                    number,
                    "even with its brief cut away, the unit is over afkd's 64 KiB plugin line",
                );
                return idle_line();
            }
        };
        self.diag.narrate(&format!(
            "[afkd_board] claimed #{number} \"{}\"",
            card.title
        ));
        self.live.insert(unit.key, Live { number, board });
        line
    }

    /// Take the label back off a card that cannot be handed over, and say why.
    fn give_back(&self, api: &dyn Api, number: i64, why: &str) {
        self.diag.err(&format_args!(
            "poll: #{number} cannot be handed to afkd: {why}"
        ));
        if let Err(e) = api.remove_label(number, CLAIMED_LABEL) {
            self.diag.err(&format_args!(
                "poll: could not take {CLAIMED_LABEL} back off #{number}: {e}"
            ));
        }
    }

    /// The arguments afkd passes every slot beside the card: `b`, the `afkd_board.Board` the
    /// `key` and `base_url` settings make, as a `call`'s `args` carry one.
    fn slot_args(&self) -> Map<String, Value> {
        let mut args = Map::new();
        args.insert(
            "b".to_string(),
            json!({ "key": self.cfg.key, "base_url": self.cfg.base_url }),
        );
        args
    }

    /// The env a run carries: the board's address, the `Authorization` value, and what the
    /// plugin knows of the run it will be in, JSON: the service, as the run-dir segment names it.
    fn env(&self) -> BTreeMap<String, String> {
        BTreeMap::from([
            (ENV_URL.to_string(), self.cfg.base_url.clone()),
            (
                ENV_AUTHORIZATION.to_string(),
                format!("Bearer {}", self.cfg.key),
            ),
            (
                ENV_RUN.to_string(),
                json!({ "service": self.id.service }).to_string(),
            ),
        ])
    }

    /// Finish a unit: the label stays on until the post-run slot has run, so the card is written
    /// down as owed a release, and the child's next `poll` takes it off. A finish that could not
    /// be written down is `held`: afkd keeps the journal entry and sends `release` for its key
    /// on a later beat, which takes the label off.
    pub(crate) fn finish(&mut self, key: &str, outcome: Option<&str>) -> String {
        let Some(live) = self.live.remove(key) else {
            self.diag.err(&format_args!(
                "finish for {key}, which this plugin holds no claim on"
            ));
            return json!({"ok": true}).to_string();
        };
        let owed = Entry {
            key: key.to_string(),
            number: live.number,
            board: live.board,
            owner: self.id.owner.clone(),
            pid: self.pid,
        };
        match self.ledger.record(owed) {
            Ok(()) => {
                self.diag.narrate(&format!(
                    "[afkd_board] finished #{} ({}); {CLAIMED_LABEL} comes off at the next poll",
                    live.number,
                    outcome.unwrap_or("no outcome")
                ));
                json!({"ok": true}).to_string()
            }
            Err(e) => {
                self.diag.err(&format_args!(
                    "could not write down the release owed for {key}: {e}; afkd holds the claim \
                     and releases it on a later beat"
                ));
                json!({"ok": true, "held": true}).to_string()
            }
        }
    }

    /// Release a journal key, a crashed run's, a `held` finish's or a unit afkd handed straight
    /// back: take the label off the card it names, and forget the key. `true` also for a card
    /// that no longer carries the label, `false` when the board could not be reached (afkd keeps
    /// the key), and `null` for a key that is no key of this board.
    pub(crate) fn release(&mut self, key: &str) -> String {
        self.live.remove(key);
        let reply = |released: serde_json::Value| json!({ "released": released }).to_string();
        let Some((board_id, card_uuid)) = parse_key(key) else {
            self.diag.err(&format_args!(
                "release for {key}, which is no key of an afkd_board card"
            ));
            return reply(json!(null));
        };
        let api = (self.connect)(&self.cfg.board(), self.clock.now() + CALL_BUDGET, "release");
        match self.release_card(&*api, board_id, card_uuid, key) {
            Ok(Some(released)) => reply(json!(released)),
            Ok(None) => reply(json!(null)),
            Err(e) => {
                self.diag.err(&e);
                reply(json!(false))
            }
        }
    }

    /// The body of [`release`](Self::release): `None` for a key of another board.
    fn release_card(
        &self,
        api: &dyn Api,
        board_id: &str,
        card_uuid: &str,
        key: &str,
    ) -> Result<Option<bool>, String> {
        let board = api.board()?;
        if board.id != board_id {
            self.diag.err(&format_args!(
                "release for {key}, a card of board {board_id}; this service reaches board {}",
                board.id
            ));
            return Ok(None);
        }
        let exists = board.labels.iter().any(|l| l.name == CLAIMED_LABEL);
        let carrying = if exists {
            api.cards_labelled(CLAIMED_LABEL)?
        } else {
            Vec::new()
        };
        if let Some(card) = carrying.iter().find(|c| c.id == card_uuid) {
            api.remove_label(card.number, CLAIMED_LABEL)?;
            self.diag.narrate(&format!(
                "[afkd_board] released #{} ({CLAIMED_LABEL} is off)",
                card.number
            ));
        }
        if let Err(e) = self.ledger.forget(board_id, key, &self.id.owner, self.pid) {
            self.diag
                .err(&format_args!("release: cannot read the owed releases: {e}"));
        }
        Ok(Some(true))
    }
}

/// A card won: its board's uuid, the card with the label on, and the comments read when it was
/// won.
struct Claimed {
    board: String,
    card: CardAnswer,
    comments: Vec<CommentAnswer>,
}

/// Whether the board's refusal `error` says the card is gone, which nothing more can be owed
/// to.
fn is_gone(error: &str) -> bool {
    error.contains("has no card")
}

/// The brief afkd lays at `task/README.md`: the card's title and place, its description, its
/// checklist and its comments, claim markers left out.
fn brief(card: &CardAnswer, comments: &[CommentAnswer]) -> String {
    let mut text = format!("# #{} {}\n\n{}\n", card.number, card.title, card.url);
    text.push_str(&format!("\nList: {}\n", card.list));
    if !card.labels.is_empty() {
        text.push_str(&format!("Labels: {}\n", card.labels.join(", ")));
    }
    text.push_str("\n## Description\n\n");
    text.push_str(if card.description.trim().is_empty() {
        "(none)"
    } else {
        card.description.trim_end()
    });
    text.push('\n');
    if !card.checklist.is_empty() {
        text.push_str("\n## Checklist\n\n");
        for ItemAnswer { text: item, done } in &card.checklist {
            text.push_str(&format!("- [{}] {item}\n", if *done { "x" } else { " " }));
        }
    }
    let thread: Vec<&CommentAnswer> = comments
        .iter()
        .filter(|c| !(c.by_key && is_claim(&c.body)))
        .collect();
    if !thread.is_empty() {
        text.push_str("\n## Comments\n");
        for comment in thread {
            let by = comment
                .by
                .as_deref()
                .unwrap_or("someone whose account is gone");
            let run = comment
                .run
                .as_ref()
                .map_or_else(String::new, |run| format!(", in run {}", run.id));
            text.push_str(&format!(
                "\n**{by}** ({}{run}):\n\n{}\n",
                comment.created_at,
                comment.body.trim_end()
            ));
        }
    }
    text
}

#[cfg(test)]
pub(crate) mod mock;
#[cfg(test)]
mod tests;
