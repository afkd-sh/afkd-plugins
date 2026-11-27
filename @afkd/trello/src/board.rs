//! The mockable card-board seam the `intake` kind drives, plus an in-memory mock board
//! (`MockBoard`, test-only) for offline tests.
//!
//! The kind does network I/O, but its *logic* — the comment-based claim lock, the
//! gates, the park — is unit-tested with no network by talking only to this trait. The
//! real implementation lives beside it in [`crate::client`]; this module owns the trait,
//! the plain value types it exchanges, the typed [`BoardError`], and the mock. Ported
//! verbatim from afkd's `crates/trello/src/board.rs`.
//!
//! The trait carries only what the kind needs, **by meaning** — resolve a named list,
//! read/post/delete comments, move a card, badge it with a label — so the Trello REST
//! mapping stays confined to the real adapter. What a slot does to a card is a
//! `trello.Account` call ([`crate::writes`]), which does not go through here.

use std::time::{Instant, SystemTime};

use crate::client::ListPosition;
use crate::handles::CardFields;
use crate::settings::MemberRef;

/// A card on the board: the unit of work the kind turns into a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Card {
    /// The card's stable identity on the board.
    pub(crate) id: String,
    /// The id from the card's URL (`trello.com/c/<shortLink>`): the human-facing
    /// handle a run is named after. [`parse_cards`](crate::client::parse_cards)
    /// falls back to `id` when the board supplies none, so this is never empty.
    pub(crate) short_link: String,
    /// The card's short title (the first line of the task text).
    pub(crate) title: String,
    /// The card's address on trello.com (`https://trello.com/c/<shortLink>/<slug>`),
    /// handed to afkd as the handle's `url`. Empty when the board sent none.
    pub(crate) url: String,
    /// The card's longer description (the task-text body; empty when absent).
    pub(crate) description: String,
    /// The card's checklists (its acceptance contract), in board order. Empty
    /// when the card carries none; the kind folds them into the brief so the
    /// agent sees each item and its checkItem id.
    pub(crate) checklists: Vec<Checklist>,
    /// The ids of the members on the card, in board order. Read by the
    /// `only_members` intake gate and by nothing else.
    pub(crate) members: Vec<String>,
    /// The card's label names, in board order. Read by the `only_labels` and
    /// `skip_labels` intake gates, and handed to afkd as the handle's `labels`.
    pub(crate) labels: Vec<String>,
    /// When the card was created, when the board said so for free: decoded from the
    /// card's own id, the same way [`Comment::posted_at`] is. `None` when the id
    /// carried no such time — then the `min_age` gate pays for
    /// [`BoardClient::card_created_at`]. Read by that gate and by nothing else.
    pub(crate) created_at: Option<SystemTime>,
    /// The card's comments, when the board supplied them free with the card list
    /// (the `actions=commentCard` nesting `list_cards` asks for), newest first.
    /// `None` when it named none, and then a caller that needs them pays for
    /// [`BoardClient::card_comments`]. Read by the `discuss_with` tail gate and by
    /// nothing else — the claim decision and the backstop keep their own reads,
    /// since both must see the thread as it is *now* rather than as the poll saw it.
    pub(crate) comments: Option<Vec<Comment>>,
    /// The id of the list the card sits in. Empty when the board did not say: the
    /// narrow [`BoardClient::board_cards`] projection names none. Read by the parked
    /// sweep, to put a card it claims back in the list it sat in, and by nothing else.
    pub(crate) list_id: String,
}

/// One checklist on a card: a named group of check items.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Checklist {
    /// The checklist's display name.
    pub(crate) name: String,
    /// Its items, in board order.
    pub(crate) items: Vec<CheckItem>,
}

/// One item within a checklist: the handle (`id`) the agent ticks, its text, and
/// whether it is currently complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CheckItem {
    /// The item's stable checkItem identity (the id the tick skill acts on).
    pub(crate) id: String,
    /// The item's text.
    pub(crate) name: String,
    /// Whether the item is marked complete.
    pub(crate) complete: bool,
}

/// A comment on a card: the kind's claims and attempt markers are comments,
/// so it reads them back to judge a claim and to count failed attempts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Comment {
    /// The comment's stable identity (used to delete a claim we posted).
    pub(crate) id: String,
    /// The comment body.
    pub(crate) text: String,
    /// The **member id** of whoever posted the comment (Trello's
    /// `idMemberCreator`). This is an identity key, not a display string: the
    /// claim path and the `discuss_with` gate match on it, so it is never
    /// overwritten with a name — see [`author_name`](Self::author_name).
    pub(crate) author: String,
    /// The commenter's display name, resolved best-available at parse time
    /// (`fullName` → `username` → the raw member id, so it is never empty). Display
    /// only — the brief attributes each comment with this; nothing gates on it.
    pub(crate) author_name: String,
    /// When the comment was posted, as the board reports it (recovered from the
    /// comment's own ObjectId). The **order** half of the claim decision: an edit
    /// must never reshuffle who spoke first. Read, never trusted against a local
    /// clock.
    pub(crate) posted_at: SystemTime,
    /// When the comment was last edited, as the board reports it (the action's
    /// `dateLastEdited`), falling back to [`posted_at`](Self::posted_at) for a
    /// comment nobody has edited. The **liveness** half: a claim marker is renewed
    /// by editing it, so this is what says its holder is still alive. Read the same
    /// way — never against a local clock.
    pub(crate) renewed_at: SystemTime,
}

/// An attachment of a card: a file Trello holds (an upload) or a link, as a read of the card's
/// attachments gives it. The kind downloads the uploads, and the files its text links, into
/// the task folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Attachment {
    /// The attachment's stable identity on the card.
    pub(crate) id: String,
    /// Its name: the file name of an upload, the title of a link.
    pub(crate) name: String,
    /// Where it is: Trello's download url of an upload, the target of a link.
    pub(crate) url: String,
    /// Whether Trello holds the file, rather than the card linking to somewhere else.
    pub(crate) is_upload: bool,
    /// The file's size in bytes, when Trello said.
    pub(crate) bytes: Option<u64>,
    /// When it was attached, as the board reports it.
    pub(crate) posted_at: SystemTime,
}

/// A failure reaching the board, tagged with the stage it struck so a swallowed
/// poll error logs *where* it happened. Each variant renders the built-in's own
/// message, byte for byte.
#[derive(Debug)]
pub(crate) enum BoardError {
    /// The board answered with a non-success HTTP status.
    Status {
        /// The stage of work the call belonged to (e.g. `list cards`).
        stage: &'static str,
        /// The HTTP status code returned.
        status: u16,
    },
    /// The board answered a write with a non-success HTTP status and said why in the body.
    Answered {
        /// The stage of work the call belonged to.
        stage: &'static str,
        /// The HTTP status code returned.
        status: u16,
        /// Trello's own words for the refusal.
        message: String,
    },
    /// The request never produced a response (connection/transport failure).
    Transport {
        /// The stage of work the call belonged to.
        stage: &'static str,
        /// A human-readable transport reason.
        reason: String,
    },
    /// The response body could not be decoded into the expected shape.
    Decode {
        /// The stage of work the call belonged to.
        stage: &'static str,
        /// A human-readable decode reason.
        reason: String,
    },
    /// A named list was not found on the board.
    ListNotFound {
        /// The stage of work the call belonged to.
        stage: &'static str,
        /// The list name that was sought.
        name: String,
    },
    /// A named member was not found on the board.
    MemberNotFound {
        /// The stage of work the call belonged to.
        stage: &'static str,
        /// The member username that was sought.
        name: String,
    },
    /// The plugin refused to send the request at all.
    Refused {
        /// The stage of work the call belonged to.
        stage: &'static str,
        /// Why it was not sent.
        reason: String,
    },
}

impl std::fmt::Display for BoardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BoardError::Status { stage, status } => {
                write!(f, "trello {stage}: board returned status {status}")
            }
            BoardError::Answered {
                stage,
                status,
                message,
            } => write!(
                f,
                "trello {stage}: board returned status {status}: {message}"
            ),
            BoardError::Transport { stage, reason } => {
                write!(f, "trello {stage}: no response ({reason})")
            }
            BoardError::Decode { stage, reason } => {
                write!(f, "trello {stage}: undecodable response ({reason})")
            }
            BoardError::ListNotFound { stage, name } => {
                write!(f, "trello {stage}: no list named '{name}'")
            }
            BoardError::MemberNotFound { stage, name } => {
                write!(f, "trello {stage}: no member named '{name}'")
            }
            BoardError::Refused { stage, reason } => write!(f, "trello {stage}: {reason}"),
        }
    }
}

impl BoardError {
    /// The stage label this error was tagged with.
    #[cfg(test)]
    pub(crate) fn stage(&self) -> &'static str {
        match self {
            BoardError::Status { stage, .. }
            | BoardError::Answered { stage, .. }
            | BoardError::Transport { stage, .. }
            | BoardError::Decode { stage, .. }
            | BoardError::ListNotFound { stage, .. }
            | BoardError::MemberNotFound { stage, .. }
            | BoardError::Refused { stage, .. } => stage,
        }
    }
}

/// The card-board operations the kind needs, by meaning (not by REST shape).
pub(crate) trait BoardClient {
    /// Resolve the id of the list named `name` on board `board_id`.
    fn resolve_list(&self, board_id: &str, name: &str) -> Result<String, BoardError>;

    /// List the cards currently in list `list_id`, in board order.
    fn list_cards(&self, list_id: &str) -> Result<Vec<Card>, BoardError>;

    /// Every **open** card on board `board_id`, wherever it sits — the whole-board
    /// read the parked-card scan needs, since a parked card is claimed from no list.
    ///
    /// Deliberately **partial**: only the fields the scan reads (`id`, `title`,
    /// `labels`) are asked for, so an idle beat costs one small request rather than
    /// every card's description, checklists and thread. A caller that needs the
    /// whole card reads it back with [`read_card`](BoardClient::read_card).
    fn board_cards(&self, board_id: &str) -> Result<Vec<Card>, BoardError>;

    /// Read one card whole — the same fully-formed [`Card`] a
    /// [`list_cards`](BoardClient::list_cards) poll yields, comments nested.
    ///
    /// `Ok(None)` is "the board no longer has this card": archived or deleted
    /// between the scan's board read and this one. That is a state to step over,
    /// not an error, so it is distinguished from a genuine board fault.
    fn read_card(&self, card_id: &str) -> Result<Option<Card>, BoardError>;

    /// Every field of the `trello.Card` handle for card `card_id`, as the board has the card
    /// now: its list's name, its board's short link and its members' usernames resolved in
    /// the one read. A card the board no longer has is an error, like any read that fails.
    fn card_handle(&self, card_id: &str) -> Result<CardFields, BoardError>;

    /// Read the comments on card `card_id`.
    fn card_comments(&self, card_id: &str) -> Result<Vec<Comment>, BoardError>;

    /// The card's creation time, from the board's own record of it. Costs a
    /// request per call, so the `min_age` gate calls it only for a card whose
    /// [`Card::created_at`] is `None`.
    fn card_created_at(&self, card_id: &str) -> Result<SystemTime, BoardError>;

    /// Post `text` as a comment on card `card_id`, returning the created comment
    /// (its id and post time, which the claim lock needs).
    fn post_comment(&self, card_id: &str, text: &str) -> Result<Comment, BoardError>;

    /// Delete comment `comment_id` from card `card_id` (releasing a claim lease).
    fn delete_comment(&self, card_id: &str, comment_id: &str) -> Result<(), BoardError>;

    /// Rewrite comment `comment_id` on card `card_id` to `text` (renewing a claim
    /// lease). Trello stamps the edited action with `dateLastEdited`, which is what
    /// a rival's claim decision reads as liveness.
    fn edit_comment(&self, card_id: &str, comment_id: &str, text: &str) -> Result<(), BoardError>;

    /// Move card `card_id` into list `list_id`, landing at `position`.
    fn move_card(
        &self,
        card_id: &str,
        list_id: &str,
        position: ListPosition,
    ) -> Result<(), BoardError>;

    /// Attach the label named `label_name` to card `card_id`, resolving the name
    /// against board `board_id` (creating the label there if it does not yet
    /// exist). Idempotent: attaching an already-attached label is a no-op.
    fn add_label(&self, board_id: &str, card_id: &str, label_name: &str) -> Result<(), BoardError>;

    /// Detach the label named `label_name` from card `card_id`, resolving the name
    /// against board `board_id`. The mirror of
    /// [`add_label`](BoardClient::add_label) and idempotent the same way: removing
    /// a label the card does not carry is a no-op, and — unlike `add_label` — it
    /// never creates the label (a name that is not on the board simply has nothing
    /// to remove).
    fn remove_label(
        &self,
        board_id: &str,
        card_id: &str,
        label_name: &str,
    ) -> Result<(), BoardError>;

    /// Resolve `member` to a member id: [`SelfMember`](MemberRef::SelfMember) to
    /// the member the client's credentials authenticate as,
    /// [`Username`](MemberRef::Username) against `board_id`'s membership. An
    /// unknown username is a [`BoardError::MemberNotFound`].
    ///
    /// A read, safe to call once per poll — which is what the `only_members`
    /// intake gate does, once for each member it names.
    fn resolve_member(&self, board_id: &str, member: &MemberRef) -> Result<String, BoardError>;

    /// The attachments of card `card_id`, oldest first.
    fn card_attachments(&self, card_id: &str) -> Result<Vec<Attachment>, BoardError>;

    /// The bytes of the file at `url`, at most 10 MiB: an attachment's own `url`, or a link a
    /// description or comment holds to a file Trello serves. Refused for a url the plugin
    /// sends no credential to.
    fn download_attachment(&self, url: &str) -> Result<Vec<u8>, BoardError>;

    /// Whether `url` is a file this board serves: a Trello download link of an attachment of
    /// a card, at the origin the plugin sends its credential to. What tells a link to a card's
    /// file from a link to anywhere else.
    fn is_trello_file(&self, url: &str) -> bool;

    /// Hand over the current call's deadline, which every request until the next hand-over
    /// must finish by — set before a call, cleared with `None` after it. A board that
    /// sends no requests has nothing to bound and ignores it.
    fn set_call_deadline(&self, _deadline: Option<Instant>) {}
}

/// Sharing a board behind an [`Arc`](std::sync::Arc) keeps it a `BoardClient`,
/// so a caller can retain a handle while also handing the kind a boxed client.
impl<T: BoardClient + ?Sized> BoardClient for std::sync::Arc<T> {
    fn resolve_list(&self, board_id: &str, name: &str) -> Result<String, BoardError> {
        (**self).resolve_list(board_id, name)
    }
    fn list_cards(&self, list_id: &str) -> Result<Vec<Card>, BoardError> {
        (**self).list_cards(list_id)
    }
    fn board_cards(&self, board_id: &str) -> Result<Vec<Card>, BoardError> {
        (**self).board_cards(board_id)
    }
    fn read_card(&self, card_id: &str) -> Result<Option<Card>, BoardError> {
        (**self).read_card(card_id)
    }
    fn card_handle(&self, card_id: &str) -> Result<CardFields, BoardError> {
        (**self).card_handle(card_id)
    }
    fn card_comments(&self, card_id: &str) -> Result<Vec<Comment>, BoardError> {
        (**self).card_comments(card_id)
    }
    fn card_created_at(&self, card_id: &str) -> Result<SystemTime, BoardError> {
        (**self).card_created_at(card_id)
    }
    fn post_comment(&self, card_id: &str, text: &str) -> Result<Comment, BoardError> {
        (**self).post_comment(card_id, text)
    }
    fn delete_comment(&self, card_id: &str, comment_id: &str) -> Result<(), BoardError> {
        (**self).delete_comment(card_id, comment_id)
    }
    fn edit_comment(&self, card_id: &str, comment_id: &str, text: &str) -> Result<(), BoardError> {
        (**self).edit_comment(card_id, comment_id, text)
    }
    fn move_card(
        &self,
        card_id: &str,
        list_id: &str,
        position: ListPosition,
    ) -> Result<(), BoardError> {
        (**self).move_card(card_id, list_id, position)
    }
    fn add_label(&self, board_id: &str, card_id: &str, label_name: &str) -> Result<(), BoardError> {
        (**self).add_label(board_id, card_id, label_name)
    }
    fn remove_label(
        &self,
        board_id: &str,
        card_id: &str,
        label_name: &str,
    ) -> Result<(), BoardError> {
        (**self).remove_label(board_id, card_id, label_name)
    }
    fn resolve_member(&self, board_id: &str, member: &MemberRef) -> Result<String, BoardError> {
        (**self).resolve_member(board_id, member)
    }
    fn card_attachments(&self, card_id: &str) -> Result<Vec<Attachment>, BoardError> {
        (**self).card_attachments(card_id)
    }
    fn download_attachment(&self, url: &str) -> Result<Vec<u8>, BoardError> {
        (**self).download_attachment(url)
    }
    fn is_trello_file(&self, url: &str) -> bool {
        (**self).is_trello_file(url)
    }
    fn set_call_deadline(&self, deadline: Option<Instant>) {
        (**self).set_call_deadline(deadline)
    }
}

#[cfg(test)]
pub(crate) use mock::SELF_ID;
#[cfg(test)]
pub(crate) use mock::{mock_file_url, Action, MockBoard};

#[cfg(test)]
mod mock {
    use super::*;
    use crate::common::lock;
    use crate::handles::millis;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;
    use std::time::{Duration, UNIX_EPOCH};

    /// A list and the cards it holds.
    #[derive(Default)]
    struct List {
        id: String,
        name: String,
        cards: Vec<String>,
    }

    /// How a failing stage fails.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Failure {
        /// No response: the network, or a board that is down.
        Transport,
        /// The board answered with this status.
        Status(u16),
        /// The board answered 2xx with a body that cannot be read.
        Decode,
    }

    /// The mutable state of one card on the mock board.
    struct CardState {
        title: String,
        description: String,
        checklists: Vec<Checklist>,
        members: Vec<String>,
        labels: Vec<String>,
        comments: Vec<Comment>,
        /// Whether `list_cards` projects this card's comments as [`Card::comments`]
        /// — the board nesting them free with the poll. Off by default, so every
        /// existing test stays on the per-card-read route.
        nest_comments: bool,
        archived: bool,
        /// Whether `read_card` answers `Ok(None)` for this card while
        /// `board_cards` still lists it — the card archived *between* the scan's
        /// board read and its own, which single-threaded tests cannot otherwise
        /// reach (a card archived before the scan never reaches `read_card`).
        vanish_on_read: bool,
        /// What `list_cards` projects as [`Card::created_at`] — the board dating
        /// the card for free.
        created_at: Option<SystemTime>,
        /// What `card_created_at` answers — the board's own record of the creation,
        /// the fallback route the `min_age` gate pays for.
        created_action: SystemTime,
        /// The card's number on the board, in the order the cards were added.
        number: i64,
        /// The card's attachments with the bytes of each upload, in the order seeded.
        attachments: Vec<(Attachment, Vec<u8>)>,
    }

    /// Hand-written (not derived) so both creation times default to the epoch:
    /// every mock card then reads as decades old *and* answers the free route, so a
    /// test that does not care about age neither changes behaviour nor makes a
    /// fallback request.
    impl Default for CardState {
        fn default() -> Self {
            Self {
                title: String::new(),
                description: String::new(),
                checklists: Vec::new(),
                members: Vec::new(),
                labels: Vec::new(),
                comments: Vec::new(),
                nest_comments: false,
                archived: false,
                vanish_on_read: false,
                created_at: Some(UNIX_EPOCH),
                created_action: UNIX_EPOCH,
                number: 0,
                attachments: Vec::new(),
            }
        }
    }

    /// The member id [`MemberRef::SelfMember`] resolves to on a mock board, so the
    /// common case needs no seeding. Matches the author of a posted comment.
    pub(crate) const SELF_ID: &str = "mock-self";

    /// The short link a mock card reports for its board.
    pub(crate) const MOCK_BOARD: &str = "mockboard";

    /// What a recorded board mutation was.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) enum Action {
        /// A card was moved to a list at a position.
        Move {
            /// The card moved.
            card: String,
            /// The destination list id.
            list: String,
            /// Where in the list it landed.
            position: ListPosition,
        },
        /// A comment was deleted from a card.
        DeleteComment {
            /// The card the comment was on.
            card: String,
            /// The deleted comment id.
            comment: String,
        },
        /// A comment's text was rewritten — a claim lease's renewal.
        EditComment {
            /// The card the comment is on.
            card: String,
            /// The edited comment id.
            comment: String,
            /// The text it now carries.
            text: String,
        },
        /// A named label was added to a card.
        AddLabel {
            /// The card the label was added to.
            card: String,
            /// The label name.
            label: String,
        },
        /// A named label was removed from a card.
        RemoveLabel {
            /// The card the label was removed from.
            card: String,
            /// The label name.
            label: String,
        },
    }

    /// An in-memory [`BoardClient`] with controllable comment post times.
    ///
    /// Tests seed lists, cards, and comments (each with an explicit post time),
    /// then drive the kind against it and inspect the recorded actions. A
    /// stage can be made to fail to exercise the swallow-and-continue path.
    #[derive(Default)]
    pub(crate) struct MockBoard {
        lists: Mutex<Vec<List>>,
        cards: Mutex<HashMap<String, CardState>>,
        actions: Mutex<Vec<Action>>,
        /// The board's membership, username → member id, as `resolve_member` reads it.
        members: Mutex<HashMap<String, String>>,
        /// A stage name that should fail, and how, if any.
        fail_stage: Mutex<Option<(&'static str, Failure)>>,
        /// Whether every stage fails with a transport error: the board out of reach.
        down: std::sync::atomic::AtomicBool,
        /// A substring that makes a *comment post* fail with a transport error, if
        /// any — the finer-grained sibling of [`fail_stage`](Self::fail_stage), for
        /// the several posts that share one stage name.
        fail_post_needle: Mutex<Option<&'static str>>,
        /// Monotonic counter giving each posted comment a unique id + time.
        next_post: AtomicU64,
        /// How many times `resolve_member` has been called, so a test can pin the
        /// intake gate's board traffic ("once per poll", and none when unset).
        resolves: AtomicU64,
        /// How many times `card_created_at` has been called, so a test can pin that
        /// the `min_age` gate pays for the fallback only when the card is undated.
        created_at_reads: AtomicU64,
        /// Every board call's stage name, in order — the request log a test reads
        /// to pin what a poll costs, not just what it decided.
        calls: Mutex<Vec<&'static str>>,
        /// The card ids `read_card` was asked for, in order — so a test can pin
        /// *which* cards a bounded scan reached, not merely how many.
        card_reads: Mutex<Vec<String>>,
        /// Every call deadline the board was handed, in order. Recorded only: the mock's
        /// requests stay instant.
        call_deadlines: Mutex<Vec<Option<Instant>>>,
    }

    /// The address a mock card reports, as Trello's default card fields carry one.
    pub(crate) fn card_url(id: &str) -> String {
        format!("https://trello.com/c/{id}")
    }

    /// The origin the mock board serves its files from: what its [`is_trello_file`] takes.
    ///
    /// [`is_trello_file`]: BoardClient::is_trello_file
    pub(crate) const MOCK_FILES: &str = "https://mock.trello.test/";

    /// The download url of the upload `id` named `name` on the mock card `card`.
    pub(crate) fn mock_file_url(card: &str, id: &str, name: &str) -> String {
        format!("{MOCK_FILES}1/cards/{card}/attachments/{id}/download/{name}")
    }

    impl MockBoard {
        /// A fresh, empty board.
        pub(crate) fn new() -> Self {
            Self::default()
        }

        /// Add a named list, returning its id (its name doubles as its id).
        pub(crate) fn add_list(&self, name: &str) -> String {
            let mut lists = lock(&self.lists);
            lists.push(List {
                id: name.to_string(),
                name: name.to_string(),
                cards: Vec::new(),
            });
            name.to_string()
        }

        /// Add a card to a list with a title and description.
        pub(crate) fn add_card(&self, list: &str, id: &str, title: &str, description: &str) {
            lock(&self.lists)
                .iter_mut()
                .find(|l| l.id == list)
                .expect("list exists")
                .cards
                .push(id.to_string());
            let mut cards = lock(&self.cards);
            let number = i64::try_from(cards.len()).unwrap_or(0) + 1;
            cards.insert(
                id.to_string(),
                CardState {
                    title: title.to_string(),
                    description: description.to_string(),
                    number,
                    ..CardState::default()
                },
            );
        }

        /// Seed a checklist named `name` on a card, its items given as
        /// `(id, name, complete)` triples — mirroring `add_card`/`seed_comment`,
        /// so a test can attach the acceptance contract the kind renders.
        pub(crate) fn seed_checklist(&self, card: &str, name: &str, items: &[(&str, &str, bool)]) {
            lock(&self.cards)
                .get_mut(card)
                .expect("card exists")
                .checklists
                .push(Checklist {
                    name: name.to_string(),
                    items: items
                        .iter()
                        .map(|(id, text, complete)| CheckItem {
                            id: id.to_string(),
                            name: text.to_string(),
                            complete: *complete,
                        })
                        .collect(),
                });
        }

        /// Seed a member id onto a card, as the board reports it in `idMembers` —
        /// what the `only_members` intake gate filters on.
        pub(crate) fn seed_card_member(&self, card: &str, member_id: &str) {
            lock(&self.cards)
                .get_mut(card)
                .expect("card exists")
                .members
                .push(member_id.to_string());
        }

        /// Seed a label name onto a card, as the board reports it in the card's
        /// `labels` array — what the `only_labels` and `skip_labels` intake gates filter
        /// on.
        pub(crate) fn seed_card_label(&self, card: &str, label_name: &str) {
            lock(&self.cards)
                .get_mut(card)
                .expect("card exists")
                .labels
                .push(label_name.to_string());
        }

        /// Seed what the board dates a card with for free — what `list_cards`
        /// projects as [`Card::created_at`]. `None` clears it, putting the card on
        /// the `card_created_at` fallback route the way an unparseable id does.
        pub(crate) fn seed_card_created_at(&self, card: &str, at: Option<SystemTime>) {
            lock(&self.cards)
                .get_mut(card)
                .expect("card exists")
                .created_at = at;
        }

        /// Seed the board's own record of a card's creation — what
        /// `card_created_at` answers, the fallback route's reply.
        pub(crate) fn seed_card_created_action(&self, card: &str, at: SystemTime) {
            lock(&self.cards)
                .get_mut(card)
                .expect("card exists")
                .created_action = at;
        }

        /// Seed a member of the *board* (not of a card), so a
        /// [`MemberRef::Username`] resolves to `id`.
        pub(crate) fn seed_board_member(&self, username: &str, id: &str) {
            lock(&self.members).insert(username.to_string(), id.to_string());
        }

        /// How many times `resolve_member` has been called on this board.
        pub(crate) fn resolve_count(&self) -> u64 {
            self.resolves.load(Ordering::SeqCst)
        }

        /// How many times `card_created_at` has been called on this board.
        pub(crate) fn created_at_calls(&self) -> u64 {
            self.created_at_reads.load(Ordering::SeqCst)
        }

        /// Every board call's stage name so far, in order — what a poll actually
        /// asked the board for.
        pub(crate) fn calls(&self) -> Vec<&'static str> {
            lock(&self.calls).clone()
        }

        /// The card ids `read_card` has been asked for, in order.
        pub(crate) fn card_reads(&self) -> Vec<String> {
            lock(&self.card_reads).clone()
        }

        /// Every call deadline handed to this board, in order.
        pub(crate) fn call_deadlines(&self) -> Vec<Option<Instant>> {
            lock(&self.call_deadlines).clone()
        }

        /// Make `read_card` answer `Ok(None)` for this card while `board_cards`
        /// still lists it — the card archived between the two reads.
        pub(crate) fn vanish_on_read(&self, card: &str) {
            lock(&self.cards)
                .get_mut(card)
                .expect("card exists")
                .vanish_on_read = true;
        }

        /// Make `list_cards` nest this card's comments into [`Card::comments`], the
        /// way the real board does when the poll asks it to. Off by default, so a
        /// test opts in per card — which is what lets one list mix a nested card
        /// with one the board named no comments for.
        pub(crate) fn nest_comments(&self, card: &str) {
            lock(&self.cards)
                .get_mut(card)
                .expect("card exists")
                .nest_comments = true;
        }

        /// Seed a pre-existing comment on a card, posted `secs` seconds after the
        /// epoch (the explicit post time the claim lock reasons about), authored by
        /// the generic `"seed"` identity — the common case for the claim-lock tests,
        /// which do not reason about authorship.
        pub(crate) fn seed_comment(&self, card: &str, id: &str, text: &str, secs: u64) {
            self.seed_comment_by(card, id, text, "seed", secs);
        }

        /// Seed a pre-existing comment authored by an explicit member id — what the
        /// `discuss_with` tail gate keys its self/allowed decision on. Pass
        /// [`SELF_ID`] to seed a comment afkd authored, or a board member id to seed
        /// a human's. The display name defaults to the id, which is the real
        /// parser's own fallback when the board names no `memberCreator`.
        pub(crate) fn seed_comment_by(
            &self,
            card: &str,
            id: &str,
            text: &str,
            author: &str,
            secs: u64,
        ) {
            self.seed_comment_named(card, id, text, author, author, secs);
        }

        /// Seed a comment whose member id and display name differ — the real shape
        /// of a board member's comment, and what an attribution test needs to tell
        /// the identity key apart from the rendered name.
        pub(crate) fn seed_comment_named(
            &self,
            card: &str,
            id: &str,
            text: &str,
            author: &str,
            author_name: &str,
            secs: u64,
        ) {
            lock(&self.cards)
                .get_mut(card)
                .expect("card exists")
                .comments
                .push(Comment {
                    id: id.to_string(),
                    text: text.to_string(),
                    author: author.to_string(),
                    author_name: author_name.to_string(),
                    posted_at: UNIX_EPOCH + Duration::from_secs(secs),
                    // A seeded comment is unedited: written when it was last touched.
                    renewed_at: UNIX_EPOCH + Duration::from_secs(secs),
                });
        }

        /// Seed a comment posted `secs` after the epoch and last **edited** at
        /// `renewed_secs` — a claim marker its holder is still renewing, which is
        /// what tells the order half of the claim decision from the liveness half.
        pub(crate) fn seed_comment_renewed(
            &self,
            card: &str,
            id: &str,
            text: &str,
            secs: u64,
            renewed_secs: u64,
        ) {
            self.seed_comment(card, id, text, secs);
            if let Some(comment) = lock(&self.cards)
                .get_mut(card)
                .expect("card exists")
                .comments
                .iter_mut()
                .find(|c| c.id == id)
            {
                comment.renewed_at = UNIX_EPOCH + Duration::from_secs(renewed_secs);
            }
        }

        /// Seed an upload `id` named `name` on a card, attached `secs` after the epoch, whose
        /// bytes are `bytes`: served at [`mock_file_url`]. Returns that url, which a text
        /// can link.
        pub(crate) fn seed_upload(
            &self,
            card: &str,
            id: &str,
            name: &str,
            bytes: &[u8],
            secs: u64,
        ) -> String {
            let url = mock_file_url(card, id, name);
            lock(&self.cards)
                .get_mut(card)
                .expect("card exists")
                .attachments
                .push((
                    Attachment {
                        id: id.to_string(),
                        name: name.to_string(),
                        url: url.clone(),
                        is_upload: true,
                        bytes: Some(bytes.len() as u64),
                        posted_at: UNIX_EPOCH + Duration::from_secs(secs),
                    },
                    bytes.to_vec(),
                ));
            url
        }

        /// Seed an upload that claims to be `size` bytes, whatever its bytes are: the card's
        /// attachment list is what names the size, so a test can put one over the cap
        /// without holding the bytes.
        pub(crate) fn seed_upload_sized(
            &self,
            card: &str,
            id: &str,
            name: &str,
            bytes: &[u8],
            size: u64,
            secs: u64,
        ) -> String {
            let url = self.seed_upload(card, id, name, bytes, secs);
            if let Some((attachment, _)) = lock(&self.cards)
                .get_mut(card)
                .expect("card exists")
                .attachments
                .iter_mut()
                .find(|(a, _)| a.id == id)
            {
                attachment.bytes = Some(size);
            }
            url
        }

        /// Seed a link attachment `id` on a card, pointing at `url`, attached `secs` after
        /// the epoch: no file Trello holds.
        pub(crate) fn seed_link_attachment(
            &self,
            card: &str,
            id: &str,
            name: &str,
            url: &str,
            secs: u64,
        ) {
            lock(&self.cards)
                .get_mut(card)
                .expect("card exists")
                .attachments
                .push((
                    Attachment {
                        id: id.to_string(),
                        name: name.to_string(),
                        url: url.to_string(),
                        is_upload: false,
                        bytes: None,
                        posted_at: UNIX_EPOCH + Duration::from_secs(secs),
                    },
                    Vec::new(),
                ));
        }

        /// Put a card at the top of list `list`, wherever it was, as a slot's
        /// `trello.update_card(account, card, list=…)` leaves it. Recorded nowhere and
        /// counted as no request: the slot's calls are `writes.rs`'s, not the kind's.
        pub(crate) fn seed_card_in(&self, card: &str, list: &str) {
            let mut lists = lock(&self.lists);
            for l in lists.iter_mut() {
                l.cards.retain(|c| c != card);
            }
            lists
                .iter_mut()
                .find(|l| l.id == list)
                .expect("list exists")
                .cards
                .insert(0, card.to_string());
        }

        /// Archive a card, as a person does on the board: nothing lists or reads it after.
        pub(crate) fn seed_archived(&self, card: &str) {
            lock(&self.cards)
                .get_mut(card)
                .expect("card exists")
                .archived = true;
        }

        /// Make every call belonging to `stage` fail with a transport error until
        /// cleared.
        pub(crate) fn fail(&self, stage: &'static str) {
            *lock(&self.fail_stage) = Some((stage, Failure::Transport));
        }

        /// Make every call belonging to `stage` fail with the board answering `status`
        /// until cleared.
        pub(crate) fn fail_status(&self, stage: &'static str, status: u16) {
            *lock(&self.fail_stage) = Some((stage, Failure::Status(status)));
        }

        /// Make every call belonging to `stage` fail with a reply that cannot be read
        /// until cleared.
        pub(crate) fn fail_decode(&self, stage: &'static str) {
            *lock(&self.fail_stage) = Some((stage, Failure::Decode));
        }

        /// Make every call fail with a transport error until cleared: the board out of
        /// reach, as in a network outage.
        pub(crate) fn go_down(&self) {
            self.down.store(true, Ordering::SeqCst);
        }

        /// Make every `post_comment` whose text contains `needle` fail until cleared,
        /// leaving the other posts on that stage working.
        ///
        /// `fail("post comment")` is too wide wherever a beat posts more than one
        /// comment: a park posts the claim, the agent's question, the `[afkd-ran]`
        /// watermark and the `[afkd-park]` owner marker through the single
        /// `"post comment"` stage, so naming the stage fails the claim and nothing
        /// ever parks. Cleared by [`clear_failure`](Self::clear_failure), so "stop
        /// failing" stays one knob.
        pub(crate) fn fail_post_matching(&self, needle: &'static str) {
            *lock(&self.fail_post_needle) = Some(needle);
        }

        /// Stop failing.
        pub(crate) fn clear_failure(&self) {
            *lock(&self.fail_stage) = None;
            *lock(&self.fail_post_needle) = None;
            self.down.store(false, Ordering::SeqCst);
        }

        /// Set the wall-clock base (seconds since epoch) the next posted comment
        /// is stamped with, so a test controls a fresh claim's post time.
        pub(crate) fn set_clock(&self, secs: u64) {
            self.next_post.store(secs, Ordering::SeqCst);
        }

        /// The mutations recorded so far, in order.
        pub(crate) fn actions(&self) -> Vec<Action> {
            lock(&self.actions).clone()
        }

        /// The current comments on a card (for assertions).
        pub(crate) fn comments_on(&self, card: &str) -> Vec<Comment> {
            lock(&self.cards)
                .get(card)
                .map(|c| c.comments.clone())
                .unwrap_or_default()
        }

        fn guard(&self, stage: &'static str) -> Result<(), BoardError> {
            lock(&self.calls).push(stage);
            let failure = match *lock(&self.fail_stage) {
                _ if self.down.load(Ordering::SeqCst) => Failure::Transport,
                Some((failing, failure)) if failing == stage => failure,
                _ => return Ok(()),
            };
            Err(match failure {
                Failure::Transport => BoardError::Transport {
                    stage,
                    reason: "mock failure".into(),
                },
                Failure::Status(status) => BoardError::Status { stage, status },
                Failure::Decode => BoardError::Decode {
                    stage,
                    reason: "mock failure".into(),
                },
            })
        }
    }

    impl BoardClient for MockBoard {
        fn resolve_list(&self, _board_id: &str, name: &str) -> Result<String, BoardError> {
            self.guard("resolve list")?;
            lock(&self.lists)
                .iter()
                .find(|l| l.name == name)
                .map(|l| l.id.clone())
                .ok_or_else(|| BoardError::ListNotFound {
                    stage: "resolve list",
                    name: name.to_string(),
                })
        }

        fn list_cards(&self, list_id: &str) -> Result<Vec<Card>, BoardError> {
            self.guard("list cards")?;
            let lists = lock(&self.lists);
            let cards = lock(&self.cards);
            let Some(list) = lists.iter().find(|l| l.id == list_id) else {
                return Ok(Vec::new());
            };
            Ok(list
                .cards
                .iter()
                .filter_map(|id| {
                    cards.get(id).filter(|c| !c.archived).map(|c| Card {
                        id: id.clone(),
                        // Mirrors the parser's fallback: a mock card's short link is its id.
                        short_link: id.clone(),
                        title: c.title.clone(),
                        url: card_url(id),
                        description: c.description.clone(),
                        checklists: c.checklists.clone(),
                        members: c.members.clone(),
                        labels: c.labels.clone(),
                        created_at: c.created_at,
                        comments: c.nest_comments.then(|| c.comments.clone()),
                        list_id: list.id.clone(),
                    })
                })
                .collect())
        }

        fn board_cards(&self, _board_id: &str) -> Result<Vec<Card>, BoardError> {
            self.guard("board cards")?;
            let lists = lock(&self.lists);
            let cards = lock(&self.cards);
            // Lists in board order, cards in list order — deterministic, unlike the
            // `cards` map, which is what a rotation assertion needs to read.
            Ok(lists
                .iter()
                .flat_map(|l| l.cards.iter())
                .filter_map(|id| {
                    cards.get(id).filter(|c| !c.archived).map(|c| Card {
                        id: id.clone(),
                        short_link: id.clone(),
                        title: c.title.clone(),
                        labels: c.labels.clone(),
                        url: String::new(),
                        // The narrow `fields=name,labels` projection, spelled out:
                        // a caller reading a url, a description, a checklist, a member or
                        // a thread off the board-wide read finds nothing here rather
                        // than passing on the mock's generosity.
                        description: String::new(),
                        checklists: Vec::new(),
                        members: Vec::new(),
                        created_at: None,
                        comments: None,
                        list_id: String::new(),
                    })
                })
                .collect())
        }

        fn read_card(&self, card_id: &str) -> Result<Option<Card>, BoardError> {
            self.guard("read card")?;
            lock(&self.card_reads).push(card_id.to_string());
            let lists = lock(&self.lists);
            let cards = lock(&self.cards);
            let Some(c) = cards
                .get(card_id)
                .filter(|c| !c.archived && !c.vanish_on_read)
            else {
                return Ok(None);
            };
            let list_id = lists
                .iter()
                .find(|l| l.cards.iter().any(|id| id == card_id))
                .map(|l| l.id.clone())
                .unwrap_or_default();
            // The per-card route always carries the thread — that is what it is for —
            // so `nest_comments` (which models the *list* poll's nesting) does not
            // gate it.
            Ok(Some(Card {
                id: card_id.to_string(),
                short_link: card_id.to_string(),
                title: c.title.clone(),
                url: card_url(card_id),
                description: c.description.clone(),
                checklists: c.checklists.clone(),
                members: c.members.clone(),
                labels: c.labels.clone(),
                created_at: c.created_at,
                comments: Some(c.comments.clone()),
                list_id,
            }))
        }

        fn card_handle(&self, card_id: &str) -> Result<CardFields, BoardError> {
            self.guard("read card fields")?;
            let lists = lock(&self.lists);
            let cards = lock(&self.cards);
            let members = lock(&self.members);
            let Some(c) = cards.get(card_id) else {
                return Err(BoardError::Status {
                    stage: "read card fields",
                    status: 404,
                });
            };
            let list = lists
                .iter()
                .find(|l| l.cards.iter().any(|id| id == card_id))
                .map(|l| l.name.clone())
                .unwrap_or_default();
            // The board reports a member by username; one nobody seeded by id reads as the
            // id, as the real parser's own fallback does for a name.
            let username = |id: &String| {
                members
                    .iter()
                    .find(|(_, member)| *member == id)
                    .map_or_else(|| id.clone(), |(username, _)| username.clone())
            };
            let created = c.created_at.map_or(0, millis);
            Ok(CardFields {
                number: c.number,
                name: c.title.clone(),
                url: card_url(card_id),
                description: c.description.clone(),
                board: MOCK_BOARD.to_string(),
                list,
                labels: c.labels.clone(),
                members: c.members.iter().map(username).collect(),
                due: None,
                due_complete: false,
                archived: c.archived,
                created_at: created,
                last_activity_at: created,
            })
        }

        fn card_comments(&self, card_id: &str) -> Result<Vec<Comment>, BoardError> {
            self.guard("read comments")?;
            Ok(lock(&self.cards)
                .get(card_id)
                .map(|c| c.comments.clone())
                .unwrap_or_default())
        }

        fn card_created_at(&self, card_id: &str) -> Result<SystemTime, BoardError> {
            self.guard("read card created")?;
            self.created_at_reads.fetch_add(1, Ordering::SeqCst);
            // An unknown card is dated at the epoch — the same fail-open the real
            // client applies to a board that has forgotten the creation action.
            Ok(lock(&self.cards)
                .get(card_id)
                .map_or(UNIX_EPOCH, |c| c.created_action))
        }

        fn post_comment(&self, card_id: &str, text: &str) -> Result<Comment, BoardError> {
            self.guard("post comment")?;
            // After the guard, so a post the board refused is still in `calls()` —
            // a request that failed is a request that was made.
            if lock(&self.fail_post_needle).is_some_and(|n| text.contains(n)) {
                return Err(BoardError::Transport {
                    stage: "post comment",
                    reason: "mock failure".into(),
                });
            }
            let secs = self.next_post.fetch_add(1, Ordering::SeqCst);
            let comment = Comment {
                id: format!("c{secs}"),
                text: text.to_string(),
                author: SELF_ID.into(),
                author_name: SELF_ID.into(),
                posted_at: UNIX_EPOCH + Duration::from_secs(secs),
                // Freshly posted: never edited, so the two times agree.
                renewed_at: UNIX_EPOCH + Duration::from_secs(secs),
            };
            lock(&self.cards)
                .get_mut(card_id)
                .expect("card exists")
                .comments
                .push(comment.clone());
            Ok(comment)
        }

        fn delete_comment(&self, card_id: &str, comment_id: &str) -> Result<(), BoardError> {
            self.guard("delete comment")?;
            // The real route is not idempotent: Trello answers 404 on a comment that
            // is already gone and `TrelloClient::delete_comment` maps any non-2xx to
            // a `BoardError`, which reaches the operator as a diagnostic. Model that,
            // so a caller that deletes the same id twice — because it worked off a
            // stale comment snapshot — reddens here instead of passing silently.
            let known = lock(&self.cards).get_mut(card_id).is_some_and(|card| {
                let before = card.comments.len();
                card.comments.retain(|c| c.id != comment_id);
                card.comments.len() < before
            });
            // Recorded whether or not the id was live: the action log is what the
            // board was *asked* to do, so a redundant delete is visible to a test
            // counting requests as well as to one reading the diagnostic.
            lock(&self.actions).push(Action::DeleteComment {
                card: card_id.to_string(),
                comment: comment_id.to_string(),
            });
            if known {
                Ok(())
            } else {
                Err(BoardError::Status {
                    stage: "delete comment",
                    status: 404,
                })
            }
        }

        fn edit_comment(
            &self,
            card_id: &str,
            comment_id: &str,
            text: &str,
        ) -> Result<(), BoardError> {
            self.guard("edit comment")?;
            // The board rewrites the text and stamps `dateLastEdited` — the half the
            // claim decision reads as liveness — leaving the ObjectId, and so the
            // post time the order is taken on, exactly where it was.
            let edited_at = UNIX_EPOCH + Duration::from_secs(self.next_post.load(Ordering::SeqCst));
            if let Some(card) = lock(&self.cards).get_mut(card_id) {
                for comment in card.comments.iter_mut().filter(|c| c.id == comment_id) {
                    comment.text = text.to_string();
                    comment.renewed_at = edited_at;
                }
            }
            lock(&self.actions).push(Action::EditComment {
                card: card_id.to_string(),
                comment: comment_id.to_string(),
                text: text.to_string(),
            });
            Ok(())
        }

        fn move_card(
            &self,
            card_id: &str,
            list_id: &str,
            position: ListPosition,
        ) -> Result<(), BoardError> {
            self.guard("move card")?;
            let mut lists = lock(&self.lists);
            for list in lists.iter_mut() {
                list.cards.retain(|c| c != card_id);
            }
            if let Some(dest) = lists.iter_mut().find(|l| l.id == list_id) {
                match position {
                    ListPosition::Top => dest.cards.insert(0, card_id.to_string()),
                    ListPosition::Bottom => dest.cards.push(card_id.to_string()),
                }
            }
            lock(&self.actions).push(Action::Move {
                card: card_id.to_string(),
                list: list_id.to_string(),
                position,
            });
            Ok(())
        }

        fn add_label(
            &self,
            _board_id: &str,
            card_id: &str,
            label_name: &str,
        ) -> Result<(), BoardError> {
            self.guard("add label")?;
            // Attach the label too, the mirror of `remove_label`'s detach: a badge the
            // trigger adds itself has to be what the *next* beat's board read sees, or
            // no test could prove the park's own label is what re-finds the card.
            // Idempotent, like the real client's.
            if let Some(card) = lock(&self.cards).get_mut(card_id) {
                if !card.labels.iter().any(|l| l == label_name) {
                    card.labels.push(label_name.to_string());
                }
            }
            lock(&self.actions).push(Action::AddLabel {
                card: card_id.to_string(),
                label: label_name.to_string(),
            });
            Ok(())
        }

        fn remove_label(
            &self,
            _board_id: &str,
            card_id: &str,
            label_name: &str,
        ) -> Result<(), BoardError> {
            self.guard("remove label")?;
            // Unlike `add_label` (record-only), detach the label too, so a
            // `only_labels` + `remove_label` round-trip observes the change
            // through `list_cards`.
            if let Some(card) = lock(&self.cards).get_mut(card_id) {
                card.labels.retain(|l| l != label_name);
            }
            lock(&self.actions).push(Action::RemoveLabel {
                card: card_id.to_string(),
                label: label_name.to_string(),
            });
            Ok(())
        }

        fn resolve_member(
            &self,
            _board_id: &str,
            member: &MemberRef,
        ) -> Result<String, BoardError> {
            self.guard("resolve member")?;
            self.resolves.fetch_add(1, Ordering::SeqCst);
            match member {
                MemberRef::SelfMember => Ok(SELF_ID.to_string()),
                MemberRef::Username(username) => lock(&self.members)
                    .get(username)
                    .cloned()
                    .ok_or_else(|| BoardError::MemberNotFound {
                        stage: "resolve member",
                        name: username.clone(),
                    }),
            }
        }

        fn card_attachments(&self, card_id: &str) -> Result<Vec<Attachment>, BoardError> {
            self.guard("read attachments")?;
            let mut found: Vec<Attachment> = lock(&self.cards)
                .get(card_id)
                .map(|c| c.attachments.iter().map(|(a, _)| a.clone()).collect())
                .unwrap_or_default();
            found.sort_by(|a, b| (a.posted_at, &a.id).cmp(&(b.posted_at, &b.id)));
            Ok(found)
        }

        fn download_attachment(&self, url: &str) -> Result<Vec<u8>, BoardError> {
            self.guard("download attachment")?;
            if !self.is_trello_file(url) {
                return Err(BoardError::Refused {
                    stage: "download attachment",
                    reason: format!("{url} is hosted where the plugin sends no credentials"),
                });
            }
            lock(&self.cards)
                .values()
                .flat_map(|c| c.attachments.iter())
                .find(|(a, _)| a.url == url)
                .map(|(_, bytes)| bytes.clone())
                .ok_or(BoardError::Status {
                    stage: "download attachment",
                    status: 404,
                })
        }

        fn is_trello_file(&self, url: &str) -> bool {
            url.starts_with(MOCK_FILES)
        }

        fn set_call_deadline(&self, deadline: Option<Instant>) {
            lock(&self.call_deadlines).push(deadline);
        }
    }

    #[test]
    fn attachments_are_read_oldest_first_with_their_upload_flag() {
        let board = MockBoard::new();
        board.add_list("src");
        board.add_card("src", "c1", "one", "");
        board.add_card("src", "c2", "two", "");
        let late = board.seed_upload("c1", "b", "late.pdf", b"%PDF", 300);
        board.seed_link_attachment("c1", "z", "docs", "https://example.com/docs", 200);
        let early = board.seed_upload("c1", "a", "early.png", b"\x89PNG", 100);
        board.seed_upload("c2", "x", "other.txt", b"x", 50);

        let read = board.card_attachments("c1").unwrap();
        assert_eq!(
            read.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
            ["a", "z", "b"],
            "oldest first, an upload and a link alike"
        );
        assert_eq!(read[0].url, early);
        assert_eq!(read[2].url, late);
        assert_eq!(
            read.iter().map(|a| a.is_upload).collect::<Vec<_>>(),
            [true, false, true]
        );
        assert_eq!(read[0].bytes, Some(4));
        assert_eq!(read[1].bytes, None);
        assert!(board.card_attachments("none").unwrap().is_empty());
        assert_eq!(board.calls(), ["read attachments", "read attachments"]);
    }

    #[test]
    fn an_upload_downloads_byte_for_byte_and_nothing_else_does() {
        let board = MockBoard::new();
        board.add_list("src");
        board.add_card("src", "c1", "one", "");
        let bytes = b"\x89PNG\r\n\x1a\n\0\xff\xfe--end";
        let url = board.seed_upload("c1", "a", "shot.png", bytes, 100);
        board.seed_link_attachment("c1", "l", "docs", "https://example.com/docs", 200);

        assert!(board.is_trello_file(&url));
        assert!(!board.is_trello_file("https://example.com/docs"));
        assert_eq!(board.download_attachment(&url).unwrap(), bytes);
        assert!(matches!(
            board.download_attachment("https://example.com/docs"),
            Err(BoardError::Refused { .. })
        ));
        assert!(matches!(
            board.download_attachment(&mock_file_url("c1", "gone", "gone.png")),
            Err(BoardError::Status { status: 404, .. })
        ));
    }

    #[test]
    fn the_attachment_stages_fail_on_demand() {
        let board = MockBoard::new();
        board.add_list("src");
        board.add_card("src", "c1", "one", "");
        let url = board.seed_upload("c1", "a", "shot.png", b"x", 100);

        board.fail("read attachments");
        let err = board.card_attachments("c1").unwrap_err();
        assert_eq!(err.stage(), "read attachments");
        assert!(board.download_attachment(&url).is_ok(), "only that stage");
        board.clear_failure();

        board.fail_status("download attachment", 503);
        let err = board.download_attachment(&url).unwrap_err();
        assert_eq!(err.stage(), "download attachment");
        assert!(matches!(err, BoardError::Status { status: 503, .. }));
        assert!(board.card_attachments("c1").is_ok(), "only that stage");
        board.clear_failure();
        assert_eq!(board.download_attachment(&url).unwrap(), b"x");
    }

    #[test]
    fn resolve_list_finds_by_name_else_errors() {
        let board = MockBoard::new();
        let id = board.add_list("Up for Grabs");
        assert_eq!(board.resolve_list("b", "Up for Grabs").unwrap(), id);
        assert!(matches!(
            board.resolve_list("b", "Missing"),
            Err(BoardError::ListNotFound { .. })
        ));
    }

    #[test]
    fn cards_listed_in_order_and_skip_archived() {
        let board = MockBoard::new();
        board.add_list("src");
        board.add_card("src", "c1", "one", "");
        board.add_card("src", "c2", "two", "d2");
        assert_eq!(
            board
                .list_cards("src")
                .unwrap()
                .iter()
                .map(|c| c.id.clone())
                .collect::<Vec<_>>(),
            vec!["c1", "c2"]
        );
        board.seed_archived("c1");
        assert_eq!(
            board
                .list_cards("src")
                .unwrap()
                .iter()
                .map(|c| c.id.clone())
                .collect::<Vec<_>>(),
            vec!["c2"]
        );
    }

    #[test]
    fn board_cards_lists_every_open_card_across_lists_and_projects_only_labels() {
        // The whole-board read the parked scan runs on: lists in board order, cards in
        // list order, archived ones gone — and *narrow*. Only the id, the title and the
        // labels survive, mirroring the real client's `fields=name,labels`, so a caller
        // that reached for a description, a checklist or a thread here would find
        // nothing rather than passing on the mock's generosity.
        let board = MockBoard::new();
        board.add_list("Up for Grabs");
        board.add_list("In Progress");
        board.add_card("Up for Grabs", "c1", "one", "d1");
        board.add_card("Up for Grabs", "c2", "two", "d2");
        board.add_card("In Progress", "c3", "three", "d3");
        board.seed_card_label("c3", "Awaiting Reply");
        board.seed_checklist("c1", "Acceptance", &[("i1", "does the thing", true)]);
        board.seed_comment("c1", "x1", "hello", 100);

        let cards = board.board_cards("BID").unwrap();
        assert_eq!(
            cards.iter().map(|c| c.id.clone()).collect::<Vec<_>>(),
            vec!["c1", "c2", "c3"]
        );
        assert_eq!(cards[2].labels, vec!["Awaiting Reply".to_string()]);
        assert_eq!(cards[0].title, "one");
        assert_eq!(cards[0].description, "");
        assert!(cards[0].checklists.is_empty());
        assert_eq!(cards[0].comments, None);

        board.seed_archived("c2");
        assert_eq!(
            board
                .board_cards("BID")
                .unwrap()
                .iter()
                .map(|c| c.id.clone())
                .collect::<Vec<_>>(),
            vec!["c1", "c3"],
            "an archived card drops out of the board read on its own"
        );
    }

    #[test]
    fn read_card_answers_the_whole_card_or_none_when_it_is_gone() {
        // The per-card read the scan follows the board read with: the same fully-formed
        // card `list_cards` yields, comments nested unconditionally (that is what this
        // route is for). `Ok(None)` — not an error — for a card that is archived,
        // unknown, or vanished between the two reads.
        let board = MockBoard::new();
        board.add_list("src");
        board.add_card("src", "c1", "one", "the body");
        board.seed_checklist("c1", "Acceptance", &[("i1", "does the thing", true)]);
        board.seed_card_label("c1", "Awaiting Reply");
        board.seed_comment("c1", "x1", "hello", 100);
        board.add_card("src", "c2", "two", "");
        board.add_card("src", "c3", "three", "");

        let card = board.read_card("c1").unwrap().expect("the card is open");
        assert_eq!(card.description, "the body");
        assert_eq!(card.labels, vec!["Awaiting Reply".to_string()]);
        assert_eq!(card.checklists.len(), 1);
        assert_eq!(
            card.comments.as_deref().map(|c| c.len()),
            Some(1),
            "the per-card route always carries the thread"
        );

        board.seed_archived("c2");
        board.vanish_on_read("c3");
        assert_eq!(board.read_card("c2").unwrap(), None, "archived");
        assert_eq!(
            board.read_card("c3").unwrap(),
            None,
            "gone between the reads"
        );
        assert_eq!(board.read_card("ghost").unwrap(), None, "never existed");
        // A vanished card is still listed by the board read — that is the race.
        assert!(board
            .board_cards("BID")
            .unwrap()
            .iter()
            .any(|c| c.id == "c3"));
        assert_eq!(board.card_reads(), ["c1", "c2", "c3", "ghost"]);
    }

    #[test]
    fn add_label_attaches_and_remove_label_detaches() {
        // The pair is symmetric, and both are idempotent — which is what lets a park's
        // own badge be the thing the next beat's board read finds, and lets the claim
        // take it off again with no bookkeeping.
        let board = MockBoard::new();
        board.add_list("src");
        board.add_card("src", "c1", "one", "");
        board.add_label("BID", "c1", "Awaiting Reply").unwrap();
        board.add_label("BID", "c1", "Awaiting Reply").unwrap();
        assert_eq!(
            board.list_cards("src").unwrap()[0].labels,
            vec!["Awaiting Reply".to_string()],
            "attached once, however many times it is added"
        );
        board.remove_label("BID", "c1", "Awaiting Reply").unwrap();
        board.remove_label("BID", "c1", "Awaiting Reply").unwrap();
        assert!(board.list_cards("src").unwrap()[0].labels.is_empty());
    }

    #[test]
    fn seed_checklist_surfaces_in_list_cards() {
        let board = MockBoard::new();
        board.add_list("src");
        board.add_card("src", "c1", "one", "");
        board.seed_checklist("c1", "Acceptance", &[("i1", "does the thing", true)]);
        board.seed_checklist("c1", "Follow-up", &[("i2", "later", false)]);
        let cards = board.list_cards("src").unwrap();
        assert_eq!(cards.len(), 1);
        assert_eq!(
            cards[0].checklists,
            vec![
                Checklist {
                    name: "Acceptance".into(),
                    items: vec![CheckItem {
                        id: "i1".into(),
                        name: "does the thing".into(),
                        complete: true,
                    }],
                },
                Checklist {
                    name: "Follow-up".into(),
                    items: vec![CheckItem {
                        id: "i2".into(),
                        name: "later".into(),
                        complete: false,
                    }],
                },
            ]
        );
    }

    #[test]
    fn seed_card_member_surfaces_in_list_cards() {
        let board = MockBoard::new();
        board.add_list("src");
        board.add_card("src", "c1", "one", "");
        board.add_card("src", "c2", "two", "");
        board.seed_card_member("c1", SELF_ID);
        board.seed_card_member("c1", "m9");
        let cards = board.list_cards("src").unwrap();
        assert_eq!(
            cards[0].members,
            vec![SELF_ID.to_string(), "m9".to_string()]
        );
        // A card nobody is on carries no members, not a missing field.
        assert!(cards[1].members.is_empty());
    }

    #[test]
    fn seed_card_label_surfaces_in_list_cards() {
        let board = MockBoard::new();
        board.add_list("src");
        board.add_card("src", "c1", "one", "");
        board.add_card("src", "c2", "two", "");
        board.seed_card_label("c1", "Redo");
        board.seed_card_label("c1", "Bug");
        let cards = board.list_cards("src").unwrap();
        assert_eq!(cards[0].labels, vec!["Redo".to_string(), "Bug".to_string()]);
        // A card with no labels carries an empty vec, not a missing field.
        assert!(cards[1].labels.is_empty());
    }

    #[test]
    fn the_two_creation_routes_are_seeded_and_counted_separately() {
        // A mock card defaults to "dated at the epoch, for free" on both routes, so
        // an age-blind test neither changes behaviour nor pays for the fallback.
        // Clearing the free route is what an unparseable card id looks like; the
        // fallback then answers `created_action`, and only that route is counted.
        let board = MockBoard::new();
        board.add_list("src");
        board.add_card("src", "c1", "one", "");
        board.add_card("src", "c2", "two", "");
        let born = UNIX_EPOCH + Duration::from_secs(1_784_785_050);
        board.seed_card_created_at("c1", Some(born));
        board.seed_card_created_at("c2", None);
        board.seed_card_created_action("c2", born);

        let cards = board.list_cards("src").unwrap();
        assert_eq!(cards[0].created_at, Some(born));
        assert_eq!(cards[1].created_at, None);
        assert_eq!(board.created_at_calls(), 0, "listing dates nothing by hand");

        assert_eq!(board.card_created_at("c2").unwrap(), born);
        // A card the board never heard of still answers — the epoch, i.e. eligible.
        assert_eq!(board.card_created_at("ghost").unwrap(), UNIX_EPOCH);
        assert_eq!(board.created_at_calls(), 2);
    }

    #[test]
    fn mock_resolve_member_maps_self_and_usernames() {
        let board = MockBoard::new();
        board.seed_board_member("marisa", "m1");
        assert_eq!(
            board.resolve_member("BID", &MemberRef::SelfMember).unwrap(),
            SELF_ID
        );
        assert_eq!(
            board
                .resolve_member("BID", &MemberRef::Username("marisa".into()))
                .unwrap(),
            "m1"
        );
        let err = board
            .resolve_member("BID", &MemberRef::Username("ghost".into()))
            .unwrap_err();
        assert!(
            matches!(&err, BoardError::MemberNotFound { stage: "resolve member", name } if name == "ghost"),
            "got {err:?}"
        );
        // Every call is counted, faults included — the counter is what pins the
        // intake gate's board traffic.
        assert_eq!(board.resolve_count(), 3);
    }

    #[test]
    fn posted_comments_get_increasing_times_and_delete_removes() {
        let board = MockBoard::new();
        board.add_list("src");
        board.add_card("src", "c1", "one", "");
        board.set_clock(100);
        let a = board.post_comment("c1", "first").unwrap();
        let b = board.post_comment("c1", "second").unwrap();
        assert!(b.posted_at > a.posted_at);
        board.delete_comment("c1", &a.id).unwrap();
        let left = board.comments_on("c1");
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].id, b.id);
    }

    #[test]
    fn list_cards_on_an_unknown_list_is_empty() {
        let board = MockBoard::new();
        assert!(board.list_cards("nope").unwrap().is_empty());
    }

    #[test]
    fn move_to_bottom_appends_and_records_the_action() {
        let board = MockBoard::new();
        board.add_list("src");
        board.add_list("dst");
        board.add_card("dst", "c0", "zero", "");
        board.add_card("src", "c1", "one", "");
        board.move_card("c1", "dst", ListPosition::Bottom).unwrap();
        assert_eq!(
            board
                .list_cards("dst")
                .unwrap()
                .iter()
                .map(|c| c.id.clone())
                .collect::<Vec<_>>(),
            vec!["c0", "c1"],
        );
        assert_eq!(
            board.actions(),
            vec![Action::Move {
                card: "c1".into(),
                list: "dst".into(),
                position: ListPosition::Bottom,
            }]
        );
    }

    #[test]
    fn seed_card_in_puts_a_card_at_the_top_of_a_list_and_records_nothing() {
        let board = MockBoard::new();
        board.add_list("src");
        board.add_list("dst");
        board.add_card("dst", "c0", "zero", "");
        board.add_card("src", "c1", "one", "");
        board.seed_card_in("c1", "dst");
        assert_eq!(
            board
                .list_cards("dst")
                .unwrap()
                .iter()
                .map(|c| c.id.clone())
                .collect::<Vec<_>>(),
            vec!["c1", "c0"],
        );
        assert!(board.list_cards("src").unwrap().is_empty());
        assert!(board.actions().is_empty(), "no action recorded");
        assert_eq!(
            board.calls(),
            ["list cards", "list cards"],
            "and no request counted but the two reads above"
        );
    }

    #[test]
    fn add_label_records_the_action() {
        let board = MockBoard::new();
        board.add_list("src");
        board.add_card("src", "c1", "one", "");
        board.add_label("BID", "c1", "Problem").unwrap();
        assert_eq!(
            board.actions(),
            vec![Action::AddLabel {
                card: "c1".into(),
                label: "Problem".into(),
            }]
        );
    }

    #[test]
    fn remove_label_records_and_detaches() {
        // Unlike `add_label` (record-only), the mock actually detaches the label,
        // so a seeded label is gone from `list_cards` afterward — the round-trip a
        // `only_labels` + `remove_label` config observes.
        let board = MockBoard::new();
        board.add_list("src");
        board.add_card("src", "c1", "one", "");
        board.seed_card_label("c1", "Redo");
        board.remove_label("BID", "c1", "Redo").unwrap();
        assert_eq!(
            board.actions(),
            vec![Action::RemoveLabel {
                card: "c1".into(),
                label: "Redo".into(),
            }]
        );
        assert!(
            board.list_cards("src").unwrap()[0].labels.is_empty(),
            "the label should be detached from the card"
        );
    }

    #[test]
    fn remove_label_absent_label_is_a_no_op_success() {
        // Removing a label the card never carried records the action but leaves the
        // (empty) label set unchanged — the idempotent no-op success.
        let board = MockBoard::new();
        board.add_list("src");
        board.add_card("src", "c1", "one", "");
        board.remove_label("BID", "c1", "Redo").unwrap();
        assert_eq!(
            board.actions(),
            vec![Action::RemoveLabel {
                card: "c1".into(),
                label: "Redo".into(),
            }]
        );
        assert!(board.list_cards("src").unwrap()[0].labels.is_empty());
    }

    /// The fixture contract ~120 sibling tests lean on: `fail(stage)` fails **that**
    /// stage — every one of them, each error tagged with its own name — and nothing
    /// else, and `clear_failure()` puts the board back. Walking the whole stage table
    /// is what keeps a newly-guarded board call from being silently un-injectable.
    #[test]
    fn failure_injection_fails_only_the_named_stage_and_clears() {
        for stage in [
            "resolve list",
            "list cards",
            "read comments",
            "read card created",
            "post comment",
            "delete comment",
            "move card",
            "add label",
            "remove label",
            "resolve member",
        ] {
            let board = MockBoard::new();
            board.add_list("src");
            board.add_card("src", "c1", "one", "");
            // The one call each stage guards, as a closure so the same call is made
            // twice: once with the failure armed, once after it is cleared.
            let call = || match stage {
                "resolve list" => board.resolve_list("b", "src").err(),
                "list cards" => board.list_cards("src").err(),
                "read comments" => board.card_comments("c1").err(),
                "read card created" => board.card_created_at("c1").err(),
                "post comment" => board.post_comment("c1", "x").err(),
                // Deleting needs a comment that exists: the mock answers an unknown id
                // the way the real route does (404), so a bare id would fail for that
                // reason instead of for the injection under test.
                "delete comment" => {
                    let c = board
                        .post_comment("c1", "x")
                        .expect("posting is its own stage");
                    board.delete_comment("c1", &c.id).err()
                }
                "move card" => board.move_card("c1", "src", ListPosition::Top).err(),
                "add label" => board.add_label("b", "c1", "x").err(),
                "remove label" => board.remove_label("b", "c1", "x").err(),
                "resolve member" => board.resolve_member("b", &MemberRef::SelfMember).err(),
                _ => unreachable!(),
            };

            board.fail(stage);
            let Some(err) = call() else {
                panic!("`{stage}` should fail while its injection is armed");
            };
            assert_eq!(err.stage(), stage, "the error is tagged with its own stage");

            // Scoped: a *different* board call still succeeds while the injection is armed.
            if stage == "move card" {
                assert!(board.add_label("b", "c1", "y").is_ok(), "`{stage}` leaked");
            } else {
                assert!(
                    board.move_card("c1", "src", ListPosition::Top).is_ok(),
                    "`{stage}` leaked"
                );
            }

            board.clear_failure();
            assert!(
                call().is_none(),
                "`{stage}` succeeds again once the failure is cleared"
            );
        }
    }
}

#[cfg(test)]
mod board_error_tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn stage_is_reported_for_every_error_variant() {
        assert_eq!(
            BoardError::Status {
                stage: "list cards",
                status: 500
            }
            .stage(),
            "list cards"
        );
        assert_eq!(
            BoardError::Answered {
                stage: "delete comment",
                status: 401,
                message: "unauthorized permission requested".into()
            }
            .stage(),
            "delete comment"
        );
        assert_eq!(
            BoardError::Transport {
                stage: "move card",
                reason: "boom".into()
            }
            .stage(),
            "move card"
        );
        assert_eq!(
            BoardError::Decode {
                stage: "post comment",
                reason: "bad".into()
            }
            .stage(),
            "post comment"
        );
        assert_eq!(
            BoardError::ListNotFound {
                stage: "resolve list",
                name: "X".into()
            }
            .stage(),
            "resolve list"
        );
        assert_eq!(
            BoardError::MemberNotFound {
                stage: "add member",
                name: "ghost".into()
            }
            .stage(),
            "add member"
        );
        assert_eq!(
            BoardError::Refused {
                stage: "download attachment",
                reason: "elsewhere".into()
            }
            .stage(),
            "download attachment"
        );
    }

    /// Each variant renders the built-in's own message, byte for byte — the line an
    /// operator greps the service log for.
    #[test]
    fn every_variant_renders_the_built_ins_message() {
        for (err, shown) in [
            (
                BoardError::Status {
                    stage: "list cards",
                    status: 503,
                },
                "trello list cards: board returned status 503",
            ),
            (
                BoardError::Answered {
                    stage: "update comment",
                    status: 401,
                    message: "unauthorized permission requested".into(),
                },
                "trello update comment: board returned status 401: unauthorized permission requested",
            ),
            (
                BoardError::Transport {
                    stage: "move card",
                    reason: "io: Connection refused".into(),
                },
                "trello move card: no response (io: Connection refused)",
            ),
            (
                BoardError::Decode {
                    stage: "post comment",
                    reason: "expected an array of actions".into(),
                },
                "trello post comment: undecodable response (expected an array of actions)",
            ),
            (
                BoardError::ListNotFound {
                    stage: "resolve list",
                    name: "Pågående".into(),
                },
                "trello resolve list: no list named 'Pågående'",
            ),
            (
                BoardError::MemberNotFound {
                    stage: "add member",
                    name: "ghost".into(),
                },
                "trello add member: no member named 'ghost'",
            ),
            (
                BoardError::Refused {
                    stage: "download attachment",
                    reason: "the attachment is hosted at evil.example, which the plugin sends \
                             no credentials to"
                        .into(),
                },
                "trello download attachment: the attachment is hosted at evil.example, which \
                 the plugin sends no credentials to",
            ),
        ] {
            assert_eq!(err.to_string(), shown);
        }
    }

    #[test]
    fn arc_board_delegates_every_operation_to_the_inner_board() {
        let board = Arc::new(MockBoard::new());
        board.add_list("src");
        board.add_card("src", "c1", "one", "desc");

        // Read paths delegate.
        assert_eq!(board.resolve_list("b", "src").unwrap(), "src");
        assert_eq!(board.list_cards("src").unwrap().len(), 1);
        assert!(board.card_comments("c1").unwrap().is_empty());
        assert_eq!(board.card_created_at("c1").unwrap(), std::time::UNIX_EPOCH);
        assert_eq!(
            board.resolve_member("b", &MemberRef::SelfMember).unwrap(),
            SELF_ID
        );

        // Mutating paths delegate (post/delete/move/label).
        let posted = board.post_comment("c1", "hi").unwrap();
        board.delete_comment("c1", &posted.id).unwrap();
        board.move_card("c1", "src", ListPosition::Top).unwrap();
        board.add_label("b", "c1", "Problem").unwrap();
        board.remove_label("b", "c1", "Problem").unwrap();
        let deadline = Instant::now();
        board.set_call_deadline(Some(deadline));
        board.set_call_deadline(None);

        // The Arc handle and the inner board are the same state.
        assert_eq!(board.resolve_count(), 1);
        assert_eq!(board.created_at_calls(), 1);
        assert_eq!(board.call_deadlines(), [Some(deadline), None]);
        assert!(board.actions().iter().any(
            |a| matches!(a, Action::AddLabel { card, label } if card == "c1" && label == "Problem")
        ));
        assert!(board.actions().iter().any(
            |a| matches!(a, Action::RemoveLabel { card, label } if card == "c1" && label == "Problem")
        ));
    }
}
