//! The real [`BoardClient`]: the Trello REST mapping behind the card-board seam.
//!
//! All Trello-specific knowledge lives here — endpoint shapes, the `key`+`token`
//! query-parameter auth, and how a comment's post time is recovered from its id.
//! The one write that carries operator free text — a posted comment — rides its
//! `text` as a JSON request body rather than a query parameter: a request-line
//! field is length-bounded (a long configured comment 414s with no usable body),
//! while the fixed-width ids the other writes carry cannot overflow and stay in
//! the query. Ported verbatim from afkd's `crates/trello/src/client.rs`, over this
//! crate's own [`crate::http`] spine.
//!
//! The response *parsing* is split into pure functions ([`parse_lists`] and friends)
//! that are unit-tested with no network. The HTTP layer itself is exercised only
//! against a loopback `Stub` (no external host). Errors are typed [`BoardError`]s
//! tagged with the stage they struck; an HTTP failure never panics.

use std::cell::Cell;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::board::{BoardClient, BoardError, Card, CheckItem, Checklist, Comment};
use crate::http::{HttpClient, HttpError, DEFAULT_CONNECT_TIMEOUT, DEFAULT_READ_TIMEOUT};
use crate::lifecycle::ListPosition;
use crate::rfc3339::parse_rfc3339;
use crate::settings::MemberRef;

/// The Trello REST base (`https://api.trello.com/1`).
pub(crate) const TRELLO_BASE: &str = "https://api.trello.com/1";

/// The failure vocabulary the shared HTTP spine reports through: the three staged
/// [`BoardError`] variants, so the plumbing builds one without naming Trello. Every
/// rendered message is the variant's own, unchanged.
impl HttpError for BoardError {
    fn status(stage: &'static str, status: u16) -> Self {
        BoardError::Status { stage, status }
    }

    fn transport(stage: &'static str, reason: String) -> Self {
        BoardError::Transport { stage, reason }
    }

    fn decode(stage: &'static str, reason: &str) -> Self {
        decode_err(stage, reason)
    }
}

/// A [`BoardClient`] backed by the Trello REST API.
pub(crate) struct TrelloClient {
    /// The spine, holding the base, the `key`+`token` credential and the timeout
    /// budgets its requests ride.
    http: HttpClient<BoardError>,
    /// The id of the member the credentials authenticate as, memoized on first
    /// use. The credentials are fixed for the client's life, so the authed
    /// identity cannot change under the memo.
    me: OnceLock<String>,
}

impl TrelloClient {
    /// A client pointed at `base` — the `base_url` setting, which defaults to the public
    /// API — authenticating with `key` + `token`. Uses the default connect/read
    /// timeouts.
    pub(crate) fn with_base(
        base: impl Into<String>,
        key: impl Into<String>,
        token: impl Into<String>,
    ) -> Self {
        Self::with_base_timeouts(
            base,
            key,
            token,
            DEFAULT_CONNECT_TIMEOUT,
            DEFAULT_READ_TIMEOUT,
        )
    }

    /// As [`with_base`](Self::with_base) but with explicit connect/read timeouts.
    /// The `read` budget bounds the write side too. Tests drive a short read
    /// timeout to prove a wedged request becomes an error quickly; production paths
    /// take the defaults.
    pub(crate) fn with_base_timeouts(
        base: impl Into<String>,
        key: impl Into<String>,
        token: impl Into<String>,
        connect: Duration,
        read: Duration,
    ) -> Self {
        Self {
            http: HttpClient::new(
                base,
                // Trello authenticates with `?key=…&token=…` query parameters, so
                // the credential rides the URL. Stamping it in the spine is what
                // keeps every endpoint below free of it — and what makes the
                // spine's URL-free transport reason load-bearing.
                vec![
                    ("key".to_string(), key.into()),
                    ("token".to_string(), token.into()),
                ],
                connect,
                read,
            ),
            me: OnceLock::new(),
        }
    }

    /// The id of the member these credentials authenticate as (`GET /members/me`),
    /// fetched once and memoized.
    ///
    /// The memo is a cache, not a lock: two threads arriving before either
    /// finishes may both issue the GET, and `get_or_init` keeps one. Since the
    /// credentials are fixed the two answers are identical, so the race costs a
    /// request, never correctness.
    fn me(&self, stage: &'static str) -> Result<String, BoardError> {
        if let Some(id) = self.me.get() {
            return Ok(id.clone());
        }
        let id = parse_member_id(stage, &self.http.get(stage, "/members/me")?)?;
        Ok(self.me.get_or_init(|| id).clone())
    }

    /// Resolve `member` to a member id, tagging any fault with `stage`.
    ///
    /// The single resolution path: [`BoardClient::add_member`] and
    /// [`BoardClient::resolve_member`] are its only callers, so the `self` memo is
    /// shared between them while each keeps its own stage label — a swallowed poll
    /// error still says whether the lifecycle action or the intake gate failed.
    fn resolve(
        &self,
        stage: &'static str,
        board_id: &str,
        member: &MemberRef,
    ) -> Result<String, BoardError> {
        match member {
            MemberRef::SelfMember => self.me(stage),
            MemberRef::Username(username) => self.member_by_username(stage, board_id, username),
        }
    }

    /// The id of the board member with `username`, or [`BoardError::MemberNotFound`].
    ///
    /// Deliberately **not** memoized: a board's membership is live state that can
    /// change under a long-lived client.
    fn member_by_username(
        &self,
        stage: &'static str,
        board_id: &str,
        username: &str,
    ) -> Result<String, BoardError> {
        let body = self
            .http
            .get(stage, &format!("/boards/{board_id}/members"))?;
        parse_members(stage, &body)?
            .into_iter()
            .find(|(_, name)| name == username)
            .map(|(id, _)| id)
            .ok_or_else(|| BoardError::MemberNotFound {
                stage,
                name: username.to_string(),
            })
    }

    /// Attach member `member_id` to card `card_id`.
    ///
    /// Unlike the other calls this rides the spine's *tolerating* send, because
    /// Trello faults a re-add rather than treating it as a no-op: an
    /// [`already_a_member`] fault is judged off the response body and swallowed so
    /// the action stays idempotent across retries. Every other status still errors.
    fn attach_member(
        &self,
        stage: &'static str,
        card_id: &str,
        member_id: &str,
    ) -> Result<(), BoardError> {
        let req = self
            .http
            .request("POST", &format!("/cards/{card_id}/idMembers"))
            .query("value", member_id);
        self.http
            .send_tolerating(stage, req, already_a_member)
            .map(|_| ())
    }
}

/// Whether an attach-member fault means "this member is already on the card".
///
/// Narrow on purpose: only a `400` whose body carries one of Trello's two
/// phrasings for a re-add. A genuinely bad member id is also a `400`, with a
/// different body, and must still fault rather than silently claim success.
fn already_a_member(status: u16, body: &str) -> bool {
    if status != 400 {
        return false;
    }
    let body = body.to_ascii_lowercase();
    body.contains("already a member") || body.contains("already on the card")
}

/// Trello's `pos` query value for a [`ListPosition`].
fn pos_value(position: ListPosition) -> &'static str {
    match position {
        ListPosition::Top => "top",
        ListPosition::Bottom => "bottom",
    }
}

impl BoardClient for TrelloClient {
    fn resolve_list(&self, board_id: &str, name: &str) -> Result<String, BoardError> {
        let stage = "resolve list";
        let body = self.http.get(stage, &format!("/boards/{board_id}/lists"))?;
        parse_lists(stage, &body)?
            .into_iter()
            .find(|(_, n)| n == name)
            .map(|(id, _)| id)
            .ok_or_else(|| BoardError::ListNotFound {
                stage,
                name: name.to_string(),
            })
    }

    fn list_cards(&self, list_id: &str) -> Result<Vec<Card>, BoardError> {
        let stage = "list cards";
        // Nest each card's checklists + items in the one fetch (no per-card
        // fan-out): `checklists=all&checkItems=all` folds them into each card
        // object, which `parse_cards` reads. Built off `request` (rather than the
        // bare `get`) for the extra pairs, mirroring `card_comments`.
        //
        // The comment actions nest the same way, and unconditionally: the
        // `discuss_with` tail gate would otherwise cost one round trip *per card*,
        // turning an N-card poll into N+2 requests. A service that does not groom
        // pays a fraction of a second per poll for the wider body — less than the
        // single connect the nesting removes — which is why this is not a flag on
        // the `BoardClient` seam. 1000 is the cap Trello accepts (1001 is a 400).
        let req = self
            .http
            .request("GET", &format!("/lists/{list_id}/cards"))
            .query("checklists", "all")
            .query("checkItems", "all")
            .query("actions", "commentCard")
            .query("actions_limit", "1000")
            .query("action_memberCreator_fields", "fullName,username");
        let body = self.http.send(stage, req)?;
        parse_cards(stage, &body)
    }

    fn board_cards(&self, board_id: &str) -> Result<Vec<Card>, BoardError> {
        let stage = "board cards";
        // `GET /boards/{id}/cards` is documented as *all of the open Cards on a
        // Board*, so an archived card drops out of the parked scan on its own — no
        // bookkeeping to clean up.
        //
        // `fields=` is what keeps a whole-board read cheap: on an 800-card board the
        // default card fields (description, badges, dates, …) would be megabytes a
        // beat, while `name,labels` is tens of kilobytes. `name` rides along because
        // `card_from_value` requires it — and a `Card` the log lines cannot name is
        // no cheaper to print. Nothing else is asked for: the scan reads `id` and
        // `labels` and re-reads the whole card through `read_card` before claiming.
        //
        // The label match is client-side. Trello has no server-side label filter
        // outside `/search`, whose index lags writes by minutes — long enough for a
        // badge afkd just added to be invisible on the next beat.
        let req = self
            .http
            .request("GET", &format!("/boards/{board_id}/cards"))
            .query("fields", "name,labels");
        let body = self.http.send(stage, req)?;
        parse_cards(stage, &body)
    }

    fn read_card(&self, card_id: &str) -> Result<Option<Card>, BoardError> {
        let stage = "read card";
        // `list_cards`' own query minus the list, and with no `fields=`, so Trello's
        // default card fields supply `shortLink`/`name`/`desc`/`idMembers`/`labels`
        // exactly as the poll gets them: the card decodes into the same fully-formed
        // `Card`, comments nested, that the claim path already handles.
        let req = self
            .http
            .request("GET", &format!("/cards/{card_id}"))
            .query("checklists", "all")
            .query("checkItems", "all")
            .query("actions", "commentCard")
            .query("actions_limit", "1000")
            .query("action_memberCreator_fields", "fullName,username");
        // A 404 is "the card is gone", not a fault — the one status this call reads
        // as an answer. It rides the tolerating send (as `attach_member` does) with
        // the verdict latched here, so a body that *is* a 404 but fails to decode
        // still comes back `Ok(None)` while every other status stays an error.
        let gone = Cell::new(false);
        let body = self.http.send_tolerating(stage, req, |status, _| {
            let is_404 = status == 404;
            gone.set(is_404);
            is_404
        })?;
        if gone.get() {
            return Ok(None);
        }
        let value = BoardError::decode_json(stage, &body)?;
        card_from_value(&value)
            .ok_or_else(|| decode_err(stage, "card missing id or name"))
            .map(Some)
    }

    fn card_comments(&self, card_id: &str) -> Result<Vec<Comment>, BoardError> {
        let stage = "read comments";
        // `actions?filter=commentCard` returns the card's comment actions.
        let req = self
            .http
            .request("GET", &format!("/cards/{card_id}/actions"))
            .query("filter", "commentCard")
            // Pin the creator fields the brief attributes with, so the resolution in
            // `action_to_comment` does not depend on Trello's default action shape.
            .query("memberCreator_fields", "fullName,username")
            // Trello's actions endpoint defaults to the newest **50**, so without
            // this the claim-lock read, the feedback delta and the backstop diff
            // all judge a long thread off a window — and disagree with the nested
            // array `list_cards` asks 1000 of. Same cap, one meaning of "the card's
            // comments".
            .query("limit", "1000");
        let body = self.http.send(stage, req)?;
        parse_comments(stage, &body)
    }

    fn card_created_at(&self, card_id: &str) -> Result<SystemTime, BoardError> {
        let stage = "read card created";
        // The board's own record of the card being created. Only reached for a card
        // whose id carried no ObjectId prefix, so this costs a request for that card
        // alone, and only while the `min_age` gate is on.
        let req = self
            .http
            .request("GET", &format!("/cards/{card_id}/actions"))
            .query("filter", "createCard")
            .query("limit", "1");
        let body = self.http.send(stage, req)?;
        parse_created_at(stage, &body)
    }

    fn post_comment(&self, card_id: &str, text: &str) -> Result<Comment, BoardError> {
        let stage = "post comment";
        let req = self
            .http
            .request("POST", &format!("/cards/{card_id}/actions/comments"));
        let body = self
            .http
            .send_json(stage, req, &serde_json::json!({ "text": text }))?;
        parse_posted(stage, &body)
    }

    fn delete_comment(&self, card_id: &str, comment_id: &str) -> Result<(), BoardError> {
        let stage = "delete comment";
        let req = self.http.request(
            "DELETE",
            &format!("/cards/{card_id}/actions/{comment_id}/comments"),
        );
        self.http.send(stage, req).map(|_| ())
    }

    fn edit_comment(&self, card_id: &str, comment_id: &str, text: &str) -> Result<(), BoardError> {
        let stage = "edit comment";
        // The delete route with the method changed and `post_comment`'s body: Trello
        // spells a comment edit as a PUT on the comment action, and stamps the action
        // with `dateLastEdited` — the liveness half `action_to_comment` reads back.
        let req = self.http.request(
            "PUT",
            &format!("/cards/{card_id}/actions/{comment_id}/comments"),
        );
        self.http
            .send_json(stage, req, &serde_json::json!({ "text": text }))
            .map(|_| ())
    }

    fn move_card(
        &self,
        card_id: &str,
        list_id: &str,
        position: ListPosition,
    ) -> Result<(), BoardError> {
        let stage = "move card";
        let req = self
            .http
            .request("PUT", &format!("/cards/{card_id}"))
            .query("idList", list_id)
            .query("pos", pos_value(position));
        self.http.send(stage, req).map(|_| ())
    }

    fn archive_card(&self, card_id: &str) -> Result<(), BoardError> {
        let stage = "archive card";
        let req = self
            .http
            .request("PUT", &format!("/cards/{card_id}"))
            .query("closed", "true");
        self.http.send(stage, req).map(|_| ())
    }

    fn complete_card(&self, card_id: &str) -> Result<(), BoardError> {
        let stage = "complete card";
        // Trello's "done state" for a card is `dueComplete`; the exact mapping
        // is an open question (see spec/QUESTIONS.md).
        let req = self
            .http
            .request("PUT", &format!("/cards/{card_id}"))
            .query("dueComplete", "true");
        self.http.send(stage, req).map(|_| ())
    }

    fn add_label(&self, board_id: &str, card_id: &str, label_name: &str) -> Result<(), BoardError> {
        let stage = "add label";
        // Resolve the label name against the board's existing labels, creating
        // one when absent (a name-only label — `color=null`, Trello's documented
        // no-color value — since a generic `add_label` imposes no color).
        let existing = parse_labels(
            stage,
            &self
                .http
                .get(stage, &format!("/boards/{board_id}/labels"))?,
        )?;
        let label_id = match existing.into_iter().find(|(_, name)| name == label_name) {
            Some((id, _)) => id,
            None => {
                let req = self
                    .http
                    .request("POST", &format!("/boards/{board_id}/labels"))
                    .query("name", label_name)
                    .query("color", "null");
                parse_created_id(stage, &self.http.send(stage, req)?)?
            }
        };
        // Attach the label to the card; Trello treats attaching an already-attached
        // label as a no-op, so this stays idempotent across retries.
        let req = self
            .http
            .request("POST", &format!("/cards/{card_id}/idLabels"))
            .query("value", &label_id);
        self.http.send(stage, req).map(|_| ())
    }

    fn remove_label(
        &self,
        board_id: &str,
        card_id: &str,
        label_name: &str,
    ) -> Result<(), BoardError> {
        let stage = "remove label";
        // Resolve the label name against the board's existing labels. Unlike
        // `add_label`, never create one — a name that is not on the board can't be
        // on the card, so there is nothing to remove: a no-op success, no DELETE.
        let existing = parse_labels(
            stage,
            &self
                .http
                .get(stage, &format!("/boards/{board_id}/labels"))?,
        )?;
        let Some((label_id, _)) = existing.into_iter().find(|(_, name)| name == label_name) else {
            return Ok(());
        };
        // Detach the label from the card. The DELETE is idempotent server-side —
        // Trello returns 200 whether or not the label was attached — so "on the
        // board but not on the card" is also a no-op success, mirroring
        // `remove_member`'s DELETE.
        let req = self
            .http
            .request("DELETE", &format!("/cards/{card_id}/idLabels/{label_id}"));
        self.http.send(stage, req).map(|_| ())
    }

    fn add_member(
        &self,
        board_id: &str,
        card_id: &str,
        member: &MemberRef,
    ) -> Result<(), BoardError> {
        let stage = "add member";
        // Resolve the operand to a member id first, so an unknown username faults
        // before anything is posted to the card.
        let member_id = self.resolve(stage, board_id, member)?;
        self.attach_member(stage, card_id, &member_id)
    }

    fn remove_member(
        &self,
        board_id: &str,
        card_id: &str,
        member: &MemberRef,
    ) -> Result<(), BoardError> {
        let stage = "remove member";
        // Resolve the operand to a member id first, so an unknown username faults
        // before the card is touched. The DELETE is idempotent server-side —
        // Trello returns the card's remaining members whether or not the member
        // was attached — so no re-remove swallow is needed (unlike `attach_member`,
        // which exists only because Trello *faults* a re-add).
        let member_id = self.resolve(stage, board_id, member)?;
        let req = self
            .http
            .request("DELETE", &format!("/cards/{card_id}/idMembers/{member_id}"));
        self.http.send(stage, req).map(|_| ())
    }

    fn resolve_member(&self, board_id: &str, member: &MemberRef) -> Result<String, BoardError> {
        self.resolve("resolve member", board_id, member)
    }
}

/// Recover a Trello action/comment's post time from its id.
///
/// A Trello id is a 24-hex-character MongoDB ObjectId whose leading 8 hex digits
/// are the Unix-seconds creation time. This lets the claim lock reason about
/// post times without parsing ISO-8601 dates. Returns `None` for a malformed id.
pub(crate) fn object_id_timestamp(id: &str) -> Option<SystemTime> {
    // `get(..8)` yields `None` both when the id is shorter than 8 bytes and when byte 8
    // falls inside a multi-byte char — so a malformed forge id reads as `None` rather
    // than panicking on a non-char-boundary slice.
    let secs = u64::from_str_radix(id.get(..8)?, 16).ok()?;
    Some(UNIX_EPOCH + Duration::from_secs(secs))
}

/// Parse a board's `lists` array into `(id, name)` pairs.
pub(crate) fn parse_lists(
    stage: &'static str,
    body: &str,
) -> Result<Vec<(String, String)>, BoardError> {
    let value = BoardError::decode_json(stage, body)?;
    let array = BoardError::as_array(stage, &value, "lists")?;
    Ok(array
        .iter()
        .filter_map(|l| {
            let id = l.get("id")?.as_str()?.to_string();
            let name = l.get("name")?.as_str()?.to_string();
            Some((id, name))
        })
        .collect())
}

/// Parse a board's `labels` array into `(id, name)` pairs.
///
/// Mirrors [`parse_lists`]: Trello labels carry `id`/`name`/`color`, and the
/// name→id resolve only needs the first two, so a label missing either is
/// skipped (an unnamed label can never match a configured name).
pub(crate) fn parse_labels(
    stage: &'static str,
    body: &str,
) -> Result<Vec<(String, String)>, BoardError> {
    let value = BoardError::decode_json(stage, body)?;
    let array = BoardError::as_array(stage, &value, "labels")?;
    Ok(array
        .iter()
        .filter_map(|l| {
            let id = l.get("id")?.as_str()?.to_string();
            let name = l.get("name")?.as_str()?.to_string();
            Some((id, name))
        })
        .collect())
}

/// Parse a board's `members` array into `(id, username)` pairs.
///
/// Mirrors [`parse_labels`]: the username→id resolve needs only those two fields,
/// so a member missing either is skipped (they can never match a configured name).
pub(crate) fn parse_members(
    stage: &'static str,
    body: &str,
) -> Result<Vec<(String, String)>, BoardError> {
    let value = BoardError::decode_json(stage, body)?;
    let array = BoardError::as_array(stage, &value, "members")?;
    Ok(array
        .iter()
        .filter_map(|m| {
            let id = m.get("id")?.as_str()?.to_string();
            let username = m.get("username")?.as_str()?.to_string();
            Some((id, username))
        })
        .collect())
}

/// Parse the `id` of the authed member from a `GET /members/me` response body.
pub(crate) fn parse_member_id(stage: &'static str, body: &str) -> Result<String, BoardError> {
    let value = BoardError::decode_json(stage, body)?;
    value
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| decode_err(stage, "authed member missing id"))
}

/// Parse the `id` of a freshly created object (a label) from its response body.
pub(crate) fn parse_created_id(stage: &'static str, body: &str) -> Result<String, BoardError> {
    let value = BoardError::decode_json(stage, body)?;
    value
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| decode_err(stage, "created object missing id"))
}

/// Parse a list's `cards` array into [`Card`]s. A card the reader cannot identify
/// (no `id`, no `name`) is skipped rather than faulting the whole poll.
pub(crate) fn parse_cards(stage: &'static str, body: &str) -> Result<Vec<Card>, BoardError> {
    let value = BoardError::decode_json(stage, body)?;
    let array = BoardError::as_array(stage, &value, "cards")?;
    Ok(array.iter().filter_map(card_from_value).collect())
}

/// One card object read into a [`Card`] (`name` → title, `desc` → body), or `None`
/// when it carries no `id`/`name`. The single decode both the list poll
/// ([`parse_cards`]) and the per-card read (`read_card`) go through, so a card
/// arrives the same shape whichever route fetched it.
///
/// `shortLink` arrives for free in Trello's default card fields (neither of those
/// two fetches passes a `fields=` naming it). A missing, non-string, or empty one
/// falls back to the card's `id`, so [`Card::short_link`] is always a usable name
/// component. Every other field degrades to its empty value, so the narrowed
/// `fields=name,labels` projection the board-wide read asks for still decodes.
pub(crate) fn card_from_value(c: &Value) -> Option<Card> {
    let id = c.get("id")?.as_str()?.to_string();
    Some(Card {
        short_link: c
            .get("shortLink")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map_or_else(|| id.clone(), str::to_string),
        // Trello dates no card field, but its id is an ObjectId whose prefix
        // is the creation second — the same decode `posted_at` makes, kept an
        // `Option` so the `min_age` gate can tell "the board dated it" from
        // "the board did not" and pay for `card_created_at` only then.
        created_at: object_id_timestamp(&id),
        id,
        title: c.get("name")?.as_str().unwrap_or("").to_string(),
        description: c
            .get("desc")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        checklists: parse_checklists(c),
        members: parse_members_field(c),
        labels: parse_labels_field(c),
        comments: parse_nested_comments(c),
    })
}

/// Parse a card's `createCard` action array into the card's creation time.
///
/// **Fails open, deliberately:** an empty array (the creation has aged out of the
/// board's retained action history), a missing `date`, or a `date` the reader
/// rejects all read as [`UNIX_EPOCH`] — infinitely old, therefore eligible. A card
/// whose birth the board no longer remembers is by definition old, and a gate that
/// cannot judge age must not silently freeze the queue. A body that is not a JSON
/// array is a different thing — the board being broken, not the card being odd — and
/// stays a [`BoardError::Decode`], exactly as [`parse_cards`] treats one.
///
/// The reader is afkd's shared one ([`parse_rfc3339`]), which is wider
/// than Trello's own `…Z`-only wire shape (it also reads a numeric offset and a
/// bare time). That can only ever *narrow* the fail-open: a date Trello does emit
/// reads as the instant it names, and a shape Trello does not emit reads as an
/// instant instead of the epoch, which makes the `min_age` gate more accurate, not
/// less.
pub(crate) fn parse_created_at(stage: &'static str, body: &str) -> Result<SystemTime, BoardError> {
    let value = BoardError::decode_json(stage, body)?;
    let array = BoardError::as_array(stage, &value, "actions")?;
    Ok(array
        .first()
        .and_then(|a| a.get("date"))
        .and_then(Value::as_str)
        .and_then(parse_rfc3339)
        .unwrap_or(UNIX_EPOCH))
}

/// Parse a card object's `idMembers` array into member ids. Arrives for free in
/// Trello's default card fields (the `list_cards` fetch passes no `fields=`), so
/// the intake gate costs no extra request. An absent or non-array field yields an
/// empty vec and a non-string element is skipped — never a panic.
pub(crate) fn parse_members_field(card: &Value) -> Vec<String> {
    card.get("idMembers")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Parse a card object's `labels` array into label names. Like `idMembers`, the
/// full `labels` array (id/name/color) arrives for free in Trello's default card
/// fields (the `list_cards` fetch passes no `fields=`), so the `require_label` gate
/// costs no extra request. An absent or non-array field yields an empty vec, and a
/// colour-only label — one whose `name` is missing, empty, or not a string — is
/// skipped rather than matched. Never panics.
pub(crate) fn parse_labels_field(card: &Value) -> Vec<String> {
    card.get("labels")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|l| l.get("name").and_then(Value::as_str))
                .filter(|name| !name.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Parse a card object's nested `actions` array (from the
/// `actions=commentCard&actions_limit=…` fetch) into the card's comments.
///
/// The distinction the field carries is between "the board named the card's
/// comments" and "it did not": an absent key — and, fail-safe, a key that is
/// present but not an array — yields `None`, which puts the caller back on the
/// per-card [`BoardClient::card_comments`] read; a present array yields `Some`,
/// even when empty (a card nobody has commented on). Entries decode through the
/// same [`action_to_comment`] the per-card route uses, so an unparseable one is
/// dropped there rather than degrading the whole card.
fn parse_nested_comments(card: &Value) -> Option<Vec<Comment>> {
    let actions = card.get("actions")?.as_array()?;
    Some(actions.iter().filter_map(action_to_comment).collect())
}

/// Parse a card object's nested `checklists` array (from the
/// `checklists=all&checkItems=all` fetch). An absent or non-array field yields an
/// empty vec; checklists are kept in fetch order.
pub(crate) fn parse_checklists(card: &Value) -> Vec<Checklist> {
    card.get("checklists")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(parse_checklist).collect())
        .unwrap_or_default()
}

/// One checklist object → a [`Checklist`] (`name`, defaulting to empty, and its
/// `checkItems` in fetch order). An absent `checkItems` array yields no items.
fn parse_checklist(v: &Value) -> Option<Checklist> {
    let name = v
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let items = v
        .get("checkItems")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(parse_check_item).collect())
        .unwrap_or_default();
    Some(Checklist { name, items })
}

/// One checkItem object → a [`CheckItem`]. Requires an `id` and `name`; the
/// `complete` flag is `state == "complete"` (any other or absent state ⇒ false).
fn parse_check_item(v: &Value) -> Option<CheckItem> {
    let id = v.get("id")?.as_str()?.to_string();
    let name = v.get("name")?.as_str()?.to_string();
    let complete = v.get("state").and_then(Value::as_str) == Some("complete");
    Some(CheckItem { id, name, complete })
}

/// Parse a card's comment-action array into [`Comment`]s.
pub(crate) fn parse_comments(stage: &'static str, body: &str) -> Result<Vec<Comment>, BoardError> {
    let value = BoardError::decode_json(stage, body)?;
    let array = BoardError::as_array(stage, &value, "actions")?;
    Ok(array.iter().filter_map(action_to_comment).collect())
}

/// Parse the single comment-action returned by posting a comment.
pub(crate) fn parse_posted(stage: &'static str, body: &str) -> Result<Comment, BoardError> {
    let value = BoardError::decode_json(stage, body)?;
    action_to_comment(&value).ok_or_else(|| decode_err(stage, "posted comment missing id/text"))
}

/// One comment action object → a [`Comment`] (skipped if it lacks an id/text).
///
/// The action carries both halves of the commenter: `idMemberCreator` (the
/// identity key the claim path and `discuss_with` gate match on) and, under
/// `memberCreator`, a human-readable name. The name resolves `fullName` →
/// `username` → the raw id, mirroring `skills/trello/list_comments.py`; a blank
/// field counts as absent, so an empty `fullName` degrades rather than rendering
/// an empty attribution.
fn action_to_comment(action: &Value) -> Option<Comment> {
    let id = action.get("id")?.as_str()?.to_string();
    let text = action.get("data")?.get("text")?.as_str()?.to_string();
    let author = action
        .get("idMemberCreator")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let creator = action.get("memberCreator");
    let field = |name: &str| {
        creator
            .and_then(|m| m.get(name))
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
    };
    let author_name = field("fullName")
        .or_else(|| field("username"))
        .map(str::to_string)
        .unwrap_or_else(|| author.clone());
    let posted_at = object_id_timestamp(&id).unwrap_or(UNIX_EPOCH);
    // Trello's own last-edited stamp, written when a comment is edited and absent
    // until then — so an unedited comment reads as renewed when it was posted, which
    // is exactly the pre-renewal rule.
    let renewed_at = action
        .get("data")
        .and_then(|d| d.get("dateLastEdited"))
        .and_then(Value::as_str)
        .and_then(parse_rfc3339)
        .unwrap_or(posted_at);
    Some(Comment {
        id,
        text,
        author,
        author_name,
        posted_at,
        renewed_at,
    })
}

fn decode_err(stage: &'static str, reason: &str) -> BoardError {
    BoardError::Decode {
        stage,
        reason: reason.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread::JoinHandle;

    /// What the stub server saw on the wire for one request: the start line split
    /// into method + target (path?query), the headers as raw lines, and the body.
    struct Captured {
        method: String,
        target: String,
        headers: Vec<String>,
        body: String,
    }

    impl Captured {
        /// The request target's path (everything before `?`).
        fn path(&self) -> &str {
            self.target.split('?').next().unwrap_or("")
        }

        /// The decoded `key=value` query pairs of the request target.
        fn query(&self) -> Vec<(String, String)> {
            let q = match self.target.split_once('?') {
                Some((_, q)) => q,
                None => return Vec::new(),
            };
            q.split('&')
                .filter(|s| !s.is_empty())
                .map(|pair| {
                    let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
                    (url_decode(k), url_decode(v))
                })
                .collect()
        }

        /// Whether the query carries `key=value`.
        fn has_query(&self, key: &str, value: &str) -> bool {
            self.query().iter().any(|(k, v)| k == key && v == value)
        }

        /// The value of header `name` (case-insensitive), if present.
        fn header(&self, name: &str) -> Option<String> {
            let want = format!("{}:", name.to_ascii_lowercase());
            self.headers.iter().find_map(|h| {
                let lower = h.to_ascii_lowercase();
                lower.strip_prefix(&want).map(|v| v.trim().to_string())
            })
        }
    }

    /// Minimal percent-decoding (enough for the query values these tests assert),
    /// including `+` → space, so request assertions read the real sent values.
    fn url_decode(s: &str) -> String {
        let bytes = s.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'%' if i + 2 < bytes.len() => {
                    let hi = (bytes[i + 1] as char).to_digit(16);
                    let lo = (bytes[i + 2] as char).to_digit(16);
                    if let (Some(hi), Some(lo)) = (hi, lo) {
                        out.push((hi * 16 + lo) as u8);
                        i += 3;
                        continue;
                    }
                    out.push(bytes[i]);
                    i += 1;
                }
                b'+' => {
                    out.push(b' ');
                    i += 1;
                }
                b => {
                    out.push(b);
                    i += 1;
                }
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    /// A one-shot HTTP/1.1 stub: accepts a single connection, reads exactly one
    /// request (start line + headers + any `Content-Length` body), hands the
    /// captured request back over a channel, and writes the canned `response`.
    ///
    /// This drives the real [`TrelloClient`] HTTP code (built on `ureq`) against a
    /// local socket via [`TrelloClient::with_base`] — the same base-URL seam the
    /// overseer live check uses — so the GET/POST/PUT/DELETE request construction
    /// and the response/error mapping are exercised with no real network.
    struct Stub {
        base: String,
        handle: Option<JoinHandle<()>>,
        rx: mpsc::Receiver<Captured>,
    }

    impl Stub {
        /// Bind a loopback port and serve `response` (a full raw HTTP/1.1 reply)
        /// to the first connection.
        fn serve(response: impl Into<String>) -> Self {
            Self::serve_seq(vec![response.into()])
        }

        /// Bind a loopback port and serve `responses` in order, one per accepted
        /// connection — for a client that issues several sequential requests (the
        /// resolve-create-attach of `add_label`). Each queued response carries a
        /// `Connection: close` header (see [`ok_json_close`]) so `ureq` opens a
        /// fresh connection per request and the accept-loop stays in lockstep,
        /// regardless of keep-alive. Each request is captured in order.
        fn serve_seq(responses: Vec<String>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
            let port = listener.local_addr().unwrap().port();
            let (tx, rx) = mpsc::channel();
            let handle = std::thread::spawn(move || {
                for response in responses {
                    let (mut stream, _) = listener.accept().expect("accept");
                    let captured = read_request(&stream);
                    stream.write_all(response.as_bytes()).expect("write reply");
                    stream.flush().ok();
                    tx.send(captured).ok();
                }
            });
            Self {
                base: format!("http://127.0.0.1:{port}"),
                handle: Some(handle),
                rx,
            }
        }

        /// A client pointed at this stub, authenticating with the given creds.
        fn client(&self, key: &str, token: &str) -> TrelloClient {
            TrelloClient::with_base(&self.base, key, token)
        }

        /// Block until the stub has captured its (single) request, then return it.
        fn captured(self) -> Captured {
            let captured = self.rx.recv().expect("stub captured a request");
            if let Some(h) = self.handle {
                let _ = h.join();
            }
            captured
        }

        /// Block until the stub has captured `n` requests (one per queued
        /// response), returning them in the order they arrived.
        fn captured_seq(self, n: usize) -> Vec<Captured> {
            let captured: Vec<Captured> = (0..n)
                .map(|_| self.rx.recv().expect("stub captured a request"))
                .collect();
            if let Some(h) = self.handle {
                let _ = h.join();
            }
            captured
        }
    }

    /// Read one HTTP/1.1 request off `stream` (start line, headers, and any
    /// `Content-Length` body) into a [`Captured`]. A `try_clone` keeps the write
    /// half usable by the caller for the reply.
    fn read_request(stream: &std::net::TcpStream) -> Captured {
        let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));

        let mut start = String::new();
        reader.read_line(&mut start).expect("read start line");
        let mut parts = start.trim_end().split(' ');
        let method = parts.next().unwrap_or("").to_string();
        let target = parts.next().unwrap_or("").to_string();

        let mut headers = Vec::new();
        let mut content_length = 0usize;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).expect("read header");
            let trimmed = line.trim_end().to_string();
            if trimmed.is_empty() {
                break;
            }
            if let Some(rest) = trimmed.to_ascii_lowercase().strip_prefix("content-length:") {
                content_length = rest.trim().parse().unwrap_or(0);
            }
            headers.push(trimmed);
        }

        let mut body = vec![0u8; content_length];
        if content_length > 0 {
            reader.read_exact(&mut body).expect("read body");
        }

        Captured {
            method,
            target,
            headers,
            body: String::from_utf8_lossy(&body).into_owned(),
        }
    }

    const OK_EMPTY_ARRAY: &str =
        "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nContent-Type: application/json\r\n\r\n[]";

    fn ok_json(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\r\n{}",
            body.len(),
            body
        )
    }

    /// As [`ok_json`] but with a `Connection: close` header, so a client that
    /// issues several sequential requests opens a fresh connection per request —
    /// keeping [`Stub::serve_seq`]'s accept-loop in lockstep with the responses.
    fn ok_json_close(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
    }

    #[test]
    fn resolve_list_gets_board_lists_with_auth_and_finds_the_named_list() {
        let body = r#"[{"id":"l1","name":"Up for Grabs"},{"id":"l2","name":"Doing"}]"#;
        let resp = ok_json(body);
        let stub = Stub::serve(resp);
        let client = stub.client("KEY", "TOK");

        let id = client.resolve_list("BID", "Doing").unwrap();
        assert_eq!(id, "l2");

        let req = stub.captured();
        assert_eq!(req.method, "GET");
        assert_eq!(req.path(), "/boards/BID/lists");
        assert!(req.has_query("key", "KEY"), "key query: {:?}", req.query());
        assert!(
            req.has_query("token", "TOK"),
            "token query: {:?}",
            req.query()
        );
        // ureq addresses the stub by its loopback authority, and a GET carries no
        // request body.
        let host = req.header("host").expect("Host header");
        assert!(host.starts_with("127.0.0.1"), "Host: {host}");
        assert!(req.body.is_empty(), "GET body: {:?}", req.body);
    }

    #[test]
    fn resolve_list_missing_name_is_list_not_found() {
        let stub = Stub::serve(OK_EMPTY_ARRAY);
        let client = stub.client("k", "t");
        let err = client.resolve_list("BID", "Nope").unwrap_err();
        assert!(matches!(err, BoardError::ListNotFound { name, .. } if name == "Nope"));
        let _ = stub.captured();
    }

    #[test]
    fn list_cards_parses_the_response_into_cards() {
        let body = r#"[{"id":"c1","name":"Fix","desc":"do it"}]"#;
        let resp = ok_json(body);
        let stub = Stub::serve(resp);
        let client = stub.client("k", "t");

        let cards = client.list_cards("L9").unwrap();
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].title, "Fix");

        let req = stub.captured();
        assert_eq!(req.method, "GET");
        assert_eq!(req.path(), "/lists/L9/cards");
    }

    #[test]
    fn board_cards_reads_the_whole_board_narrowly() {
        // The parked scan's one request per beat. It hits the board's own card
        // collection — documented as every OPEN card — and asks for `name,labels`
        // only: on an 800-card board the default card fields would be megabytes a
        // beat. `name` rides along because `card_from_value` requires it, and the
        // narrowed reply still decodes, with every unasked-for field at its empty
        // value (this is what a `fields=labels` reply would look like otherwise:
        // every card dropped, and the scan silently finding nothing).
        let body = r#"[{"id":"c1","name":"Fix the retry backoff","labels":[{"id":"l1","name":"Awaiting Reply"}]},
                       {"id":"c2","name":"Next up","labels":[]}]"#;
        let stub = Stub::serve(ok_json(body));
        let client = stub.client("k", "t");

        let cards = client.board_cards("BID").unwrap();
        assert_eq!(
            cards.len(),
            2,
            "no card is dropped by the narrow projection"
        );
        assert_eq!(cards[0].labels, vec!["Awaiting Reply".to_string()]);
        assert_eq!(cards[0].title, "Fix the retry backoff");
        assert_eq!(cards[0].description, "");
        assert_eq!(cards[0].comments, None);
        assert!(cards[1].labels.is_empty());

        let req = stub.captured();
        assert_eq!(req.method, "GET");
        assert_eq!(req.path(), "/boards/BID/cards");
        assert!(
            req.has_query("fields", "name,labels"),
            "the narrow projection: {:?}",
            req.query()
        );
    }

    #[test]
    fn read_card_asks_the_list_polls_own_query_and_decodes_one_whole_card() {
        // The per-card read carries `list_cards`' query minus the list, and no
        // `fields=` — so Trello's default card fields supply shortLink/desc/labels
        // and the card decodes into the *same* fully-formed `Card`, comments nested,
        // the poll already hands the claim path.
        let body = r#"{"id":"c1","name":"Fix the retry backoff","desc":"body 修复","shortLink":"1Rkelydw",
            "labels":[{"id":"l1","name":"Awaiting Reply"}],
            "checklists":[{"name":"Acceptance","checkItems":[{"id":"i1","name":"first","state":"complete"}]}],
            "actions":[{"id":"a1","type":"commentCard","date":"2026-09-06T08:50:08.000Z",
                        "idMemberCreator":"mem-phil","data":{"text":"reading 1 — the list."},
                        "memberCreator":{"fullName":"Phil Ek","username":"phil"}}]}"#;
        let stub = Stub::serve(ok_json(body));
        let client = stub.client("k", "t");

        let card = client.read_card("c1").unwrap().expect("an open card");
        assert_eq!(card.short_link, "1Rkelydw");
        assert_eq!(card.description, "body 修复");
        assert_eq!(card.labels, vec!["Awaiting Reply".to_string()]);
        assert_eq!(card.checklists.len(), 1);
        let comments = card.comments.expect("the thread is nested");
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].author, "mem-phil");
        assert_eq!(comments[0].author_name, "Phil Ek");

        let req = stub.captured();
        assert_eq!(req.method, "GET");
        assert_eq!(req.path(), "/cards/c1");
        for (k, v) in [
            ("checklists", "all"),
            ("checkItems", "all"),
            ("actions", "commentCard"),
            ("actions_limit", "1000"),
            ("action_memberCreator_fields", "fullName,username"),
        ] {
            assert!(req.has_query(k, v), "{k}={v} missing: {:?}", req.query());
        }
        assert!(
            !req.query().iter().any(|(k, _)| k == "fields"),
            "no `fields=`, or the default card fields would be withheld: {:?}",
            req.query()
        );
    }

    #[test]
    fn read_card_answers_none_for_a_404_and_errors_on_anything_else() {
        // A card archived or deleted between the board read and its own is `Ok(None)`
        // — a state to step over, not a fault. Every other status stays an error, so
        // a board that is merely broken never reads as "the card is gone".
        let gone = "HTTP/1.1 404 Not Found\r\nContent-Length: 32\r\nContent-Type: text/plain\r\n\r\nThe requested resource was not";
        let stub = Stub::serve(gone);
        let client = stub.client("k", "t");
        assert_eq!(client.read_card("c1").unwrap(), None);
        let _ = stub.captured();

        let denied = "HTTP/1.1 401 Unauthorized\r\nContent-Length: 12\r\nContent-Type: text/plain\r\n\r\ninvalid key\n";
        let stub = Stub::serve(denied);
        let client = stub.client("k", "t");
        assert!(matches!(
            client.read_card("c1").unwrap_err(),
            BoardError::Status {
                stage: "read card",
                status: 401
            }
        ));
        let _ = stub.captured();

        // And a 200 whose body is not a card is a decode fault, not a silent `None`.
        let stub = Stub::serve(ok_json(r#"{"no":"id here"}"#));
        let client = stub.client("k", "t");
        assert!(matches!(
            client.read_card("c1").unwrap_err(),
            BoardError::Decode { .. }
        ));
        let _ = stub.captured();
    }

    #[test]
    fn list_cards_parses_checklists_items_and_state() {
        // The nested-fetch shape: each card object carries a `checklists` array,
        // each checklist a `checkItems` array with a per-item `state`.
        let body = r#"[{"id":"c1","name":"Fix","desc":"do it","checklists":[
            {"name":"Acceptance","checkItems":[
                {"id":"i1","name":"first","state":"complete"},
                {"id":"i2","name":"second","state":"incomplete"}]},
            {"name":"Empty","checkItems":[]}]}]"#;
        let stub = Stub::serve(ok_json(body));
        let client = stub.client("k", "t");

        let cards = client.list_cards("L9").unwrap();
        assert_eq!(cards.len(), 1);
        assert_eq!(
            cards[0].checklists,
            vec![
                Checklist {
                    name: "Acceptance".into(),
                    items: vec![
                        CheckItem {
                            id: "i1".into(),
                            name: "first".into(),
                            complete: true,
                        },
                        CheckItem {
                            id: "i2".into(),
                            name: "second".into(),
                            complete: false,
                        },
                    ],
                },
                Checklist {
                    name: "Empty".into(),
                    items: Vec::new(),
                },
            ]
        );

        // The fetch asks the board to nest the checklists and their items.
        let req = stub.captured();
        assert!(
            req.has_query("checklists", "all"),
            "query: {:?}",
            req.query()
        );
        assert!(
            req.has_query("checkItems", "all"),
            "query: {:?}",
            req.query()
        );
    }

    #[test]
    fn card_comments_sends_commentcard_filter_and_parses_actions() {
        let body = r#"[{"id":"5f0000000000000000000001","idMemberCreator":"m1",
                        "data":{"text":"hi"}}]"#;
        let resp = ok_json(body);
        let stub = Stub::serve(resp);
        let client = stub.client("k", "t");

        let comments = client.card_comments("CARD").unwrap();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].text, "hi");
        assert_eq!(comments[0].author, "m1");

        let req = stub.captured();
        assert_eq!(req.method, "GET");
        assert_eq!(req.path(), "/cards/CARD/actions");
        assert!(req.has_query("filter", "commentCard"));
        // The creator fields the display name resolves from are pinned explicitly.
        assert!(req.has_query("memberCreator_fields", "fullName,username"));
        // And the window is pinned wide: Trello's actions endpoint defaults to the
        // newest 50, so without this a long thread's claim lock, feedback delta and
        // backstop diff would each judge off a truncation the nested route does not
        // share.
        assert!(req.has_query("limit", "1000"));
        assert!(req.has_query("key", "k"));
        assert!(req.has_query("token", "t"));
    }

    /// The instant both creation-time routes must agree on: 2026-07-23T05:37:30Z,
    /// the ObjectId prefix `6a61a89a` and the RFC-3339 date below alike.
    const BORN: Duration = Duration::from_secs(1_784_785_050);

    #[test]
    fn list_cards_dates_each_card_from_its_object_id() {
        // Route 1, for free out of the poll's own response: a real 24-hex card id
        // carries its creation second, and an id that is not one (an API-compatible
        // non-Trello board, a fixture) reads as `None` — the card the `min_age` gate
        // then has to ask the board about.
        let body = r#"[{"id":"6a61a89a0000000000000001","name":"Dated"},
                       {"id":"not-an-object-id","name":"Undated"}]"#;
        let stub = Stub::serve(ok_json(body));
        let cards = stub.client("k", "t").list_cards("L9").unwrap();
        assert_eq!(cards[0].created_at, Some(UNIX_EPOCH + BORN));
        assert_eq!(cards[1].created_at, None);
        let _ = stub.captured();
    }

    #[test]
    fn card_created_at_sends_the_createcard_filter_and_reads_the_date() {
        // Route 2: the board's own record of the creation, asked for by filter, one
        // action deep. It must land on the same instant route 1 decodes from the id
        // above — the two routes are one answer, not two.
        let body = r#"[{"id":"a1","type":"createCard","date":"2026-07-23T05:37:30.000Z"}]"#;
        let stub = Stub::serve(ok_json(body));
        let client = stub.client("k", "t");

        let born = client.card_created_at("CARD").unwrap();
        assert_eq!(born, UNIX_EPOCH + BORN);
        assert_eq!(
            born,
            object_id_timestamp("6a61a89a0000000000000001").unwrap(),
            "the id route and the action route must date one card alike"
        );

        let req = stub.captured();
        assert_eq!(req.method, "GET");
        assert_eq!(req.path(), "/cards/CARD/actions");
        assert!(req.has_query("filter", "createCard"));
        assert!(req.has_query("limit", "1"));
        assert!(req.has_query("key", "k"));
        assert!(req.has_query("token", "t"));
    }

    #[test]
    fn card_created_at_falls_open_to_the_epoch() {
        // A card the board can no longer date is by definition old, so each of these
        // reads as the epoch — eligible — rather than freezing the card in the queue
        // with no explanation.
        for (body, why) in [
            ("[]", "the creation aged out of the retained action history"),
            (r#"[{"id":"a1"}]"#, "the action carries no date"),
            (r#"[{"date":42}]"#, "the date is not even a string"),
            (r#"[{"date":"whenever"}]"#, "the date does not parse"),
        ] {
            let stub = Stub::serve(ok_json(body));
            let born = stub.client("k", "t").card_created_at("CARD").unwrap();
            assert_eq!(born, UNIX_EPOCH, "{why}");
            let _ = stub.captured();
        }
    }

    #[test]
    fn card_created_at_surfaces_transport_and_shape_failures() {
        // The board being unreachable, or answering with something that is not an
        // action array at all, is the board being broken — not the card being odd —
        // so it propagates for the poll to log and retry rather than failing open.
        let stub = Stub::serve("HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n");
        let err = stub.client("k", "t").card_created_at("CARD").unwrap_err();
        assert!(
            matches!(
                err,
                BoardError::Status {
                    stage: "read card created",
                    status: 500
                }
            ),
            "got {err:?}"
        );
        let _ = stub.captured();

        let stub = Stub::serve(ok_json(r#"{"message":"not an array"}"#));
        let err = stub.client("k", "t").card_created_at("CARD").unwrap_err();
        assert!(
            matches!(
                err,
                BoardError::Decode {
                    stage: "read card created",
                    ..
                }
            ),
            "got {err:?}"
        );
        let _ = stub.captured();
    }

    #[test]
    fn rfc3339_reads_trellos_shape() {
        // Trello emits a fractional part; the API documents the field without one.
        // Both are the same instant, and it is the one the matching ObjectId decodes.
        let with_frac = parse_rfc3339("2026-07-23T05:37:30.000Z").expect("fractional form");
        let without = parse_rfc3339("2026-07-23T05:37:30Z").expect("bare form");
        assert_eq!(with_frac, without);
        assert_eq!(with_frac, UNIX_EPOCH + BORN);
        // A longer fraction is read (and ignored) the same way.
        assert_eq!(
            parse_rfc3339("2026-07-23T05:37:30.123456Z").expect("microseconds"),
            with_frac
        );
        // The boundaries of the supported range: the epoch itself, and the civil
        // arithmetic's two traps — a leap day, and a year that is a century but not
        // a leap year (2100-03-01 is one day past a February that has 28 days).
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(UNIX_EPOCH));
        assert_eq!(
            parse_rfc3339("2000-02-29T12:00:00Z"),
            Some(UNIX_EPOCH + Duration::from_secs(951_825_600))
        );
        assert_eq!(
            parse_rfc3339("2100-03-01T00:00:00Z"),
            Some(UNIX_EPOCH + Duration::from_secs(4_107_542_400))
        );
        assert_eq!(
            parse_rfc3339("2026-12-31T23:59:59Z"),
            Some(UNIX_EPOCH + Duration::from_secs(1_798_761_599))
        );
    }

    #[test]
    fn rfc3339_rejects_anything_else() {
        // Garbage the shared reader refuses is `None`, which `parse_created_at`
        // folds into its fail-open epoch. Nothing here may panic — the input is
        // whatever a board put on the wire.
        for (input, why) in [
            ("", "empty"),
            ("whenever", "not a date at all"),
            ("2026-07-23", "a date with no time"),
            ("2026-13-01T00:00:00Z", "month 13"),
            ("2026-00-01T00:00:00Z", "month 0"),
            ("2026-07-32T00:00:00Z", "day 32"),
            ("2026-07-23T24:00:00Z", "hour 24"),
            ("2026-07-23T05:60:00Z", "minute 60"),
            ("1969-12-31T23:59:59Z", "before the epoch"),
            ("20x6-07-23T05:37:30Z", "a non-digit in a field"),
            ("2026/07/23T05:37:30Z", "the wrong separators"),
            ("２０２６-07-23T05:37:30Z", "wide digits (multi-byte)"),
            (
                "2026-07-23T05:37:30Ω",
                "a multi-byte char where `Z` belongs",
            ),
        ] {
            assert_eq!(parse_rfc3339(input), None, "{why}: {input:?}");
        }
    }

    #[test]
    fn the_shared_reader_is_deliberately_wider_than_trellos_wire_shape() {
        // These six were rejected by the reader trello used to carry, whose own
        // contract was "the shape Trello emits and nothing more". The shared reader
        // is the forges' too — gitea and gitlab emit `+02:00`, and a bare time read
        // as UTC is behaviour their tests pin — so it reads them rather than
        // refusing them, and that is deliberate, not drift.
        //
        // It cannot change the `min_age` gate against a real board: Trello dates
        // every action `…Z`, so none of these shapes reaches `parse_created_at`
        // from Trello. Where one somehow did, reading it yields a real instant
        // where the strict reader yielded the fail-open epoch — a *more* accurate
        // gate, never a card wrongly held back.
        let born = UNIX_EPOCH + BORN; // 2026-07-23T05:37:30Z
        assert_eq!(
            parse_rfc3339("2026-07-23T05:37:30"),
            Some(born),
            "a bare time with no zone marker reads as UTC"
        );
        assert_eq!(
            parse_rfc3339("2026-07-23T05:37:30+02:00"),
            Some(born - Duration::from_secs(2 * 3600)),
            "a numeric offset is applied, not refused"
        );
        assert_eq!(
            parse_rfc3339("2026-07-23T05:37:30.Z"),
            Some(born),
            "an empty fractional part is dropped with the rest of the fraction"
        );
        assert_eq!(
            parse_rfc3339("2026-07-23T05:37:30.00xZ"),
            Some(born),
            "so is a non-numeric one — only the whole second is read"
        );
        assert_eq!(
            parse_rfc3339("2026-07-23T05:37:60Z"),
            Some(born + Duration::from_secs(30)),
            "a leap second folds into the next minute instead of being refused"
        );
        assert_eq!(
            parse_rfc3339("2026-02-29T00:00:00Z"),
            parse_rfc3339("2026-03-01T00:00:00Z"),
            "29 February in a common year normalizes onto 1 March"
        );
    }

    /// The id/name split the brief depends on: `author` is always the raw
    /// `idMemberCreator` (what the claim path and `discuss_with` gate match on),
    /// while `author_name` resolves `fullName` → `username` → the id. A blank
    /// `fullName` counts as absent, so no comment ever renders `**:**`.
    #[test]
    fn a_comments_author_name_resolves_fullname_then_username_then_the_id() {
        let cases = [
            (
                r#""memberCreator":{"fullName":"Álvaro Pérez","username":"alvaro"},"#,
                "Álvaro Pérez",
                "fullName wins",
            ),
            (
                r#""memberCreator":{"fullName":"   ","username":"alvaro"},"#,
                "alvaro",
                "a blank fullName degrades to username",
            ),
            (
                r#""memberCreator":{"username":"alvaro"},"#,
                "alvaro",
                "no fullName at all degrades to username",
            ),
            (
                "",
                "m1",
                "no memberCreator at all degrades to the raw member id",
            ),
        ];
        for (creator, expected, why) in cases {
            let body = format!(
                r#"[{{"id":"5f0000000000000000000001","idMemberCreator":"m1",{creator}
                     "data":{{"text":"hi"}}}}]"#
            );
            let stub = Stub::serve(ok_json(&body));
            let comments = stub.client("k", "t").card_comments("CARD").unwrap();
            assert_eq!(comments[0].author_name, expected, "{why}");
            // The identity key is untouched by the resolution, in every case.
            assert_eq!(comments[0].author, "m1", "{why}");
        }
    }

    #[test]
    fn post_comment_posts_text_and_returns_the_created_comment() {
        let body = r#"{"id":"5f0000000000000000000009","data":{"text":"claim"}}"#;
        let resp = ok_json(body);
        let stub = Stub::serve(resp);
        let client = stub.client("k", "t");

        let posted = client.post_comment("CARD", "claim me").unwrap();
        assert_eq!(posted.id, "5f0000000000000000000009");
        assert_eq!(posted.text, "claim");

        let req = stub.captured();
        assert_eq!(req.method, "POST");
        assert_eq!(req.path(), "/cards/CARD/actions/comments");
        // The text rides an `application/json` body, not the request line: only
        // `key`/`token` stay in the query, and no `text` pair is present.
        assert_eq!(req.body, r#"{"text":"claim me"}"#);
        assert_eq!(
            req.header("content-type").as_deref(),
            Some("application/json")
        );
        assert!(req.has_query("key", "k"));
        assert!(req.has_query("token", "t"));
        assert!(
            !req.query().iter().any(|(k, _)| k == "text"),
            "query: {:?}",
            req.query()
        );
        // ...and nowhere on the request line either.
        assert!(!req.target.contains("claim me"), "target: {}", req.target);
    }

    #[test]
    fn post_comment_sends_an_oversized_comment_in_the_body() {
        // A comment far longer than any request line could carry: on the old code
        // path this ~16 KB of `text` would have to fit the URL and 414. In the
        // body it just posts. This is the regression the card exists to prevent.
        let text = "x".repeat(16 * 1024);
        let body = r#"{"id":"5f000000000000000000000a","data":{"text":"ok"}}"#;
        let stub = Stub::serve(ok_json(body));
        let client = stub.client("k", "t");

        let posted = client.post_comment("CARD", &text).unwrap();
        assert_eq!(posted.id, "5f000000000000000000000a");

        let req = stub.captured();
        assert_eq!(req.body, serde_json::json!({ "text": text }).to_string());
        // The oversized text never touched the request line.
        assert!(!req.target.contains(&text), "target carried the text");
    }

    #[test]
    fn delete_comment_issues_a_delete_to_the_comment_path() {
        let stub = Stub::serve(ok_json("{}"));
        let client = stub.client("k", "t");
        client.delete_comment("CARD", "CMT").unwrap();

        let req = stub.captured();
        assert_eq!(req.method, "DELETE");
        assert_eq!(req.path(), "/cards/CARD/actions/CMT/comments");
        assert!(req.has_query("key", "k"));
        assert!(req.has_query("token", "t"));
    }

    /// The renewal's wire contract: the delete route with the method changed and
    /// `post_comment`'s JSON body — the text rides the body, never the request line,
    /// and the comment is edited **by id** so a renewal cannot mint a second marker.
    #[test]
    fn edit_comment_puts_the_text_on_the_comment_path() {
        let stub = Stub::serve(ok_json("{}"));
        let client = stub.client("k", "t");
        let text = "[afkd-claim] owner=björn-öst[bot] renewal=7";
        client.edit_comment("CARD", "CMT", text).unwrap();

        let req = stub.captured();
        assert_eq!(req.method, "PUT");
        assert_eq!(req.path(), "/cards/CARD/actions/CMT/comments");
        assert_eq!(req.body, serde_json::json!({ "text": text }).to_string());
        assert!(req.has_query("key", "k"));
        assert!(req.has_query("token", "t"));
        // The text is in the body, not the query or the request line.
        assert!(
            !req.query().iter().any(|(k, _)| k == "text"),
            "query: {:?}",
            req.query()
        );
        assert!(!req.target.contains("renewal=7"), "target: {}", req.target);
    }

    #[test]
    fn move_card_puts_idlist_and_pos() {
        let stub = Stub::serve(ok_json("{}"));
        let client = stub.client("k", "t");
        client
            .move_card("CARD", "DEST", ListPosition::Bottom)
            .unwrap();

        let req = stub.captured();
        assert_eq!(req.method, "PUT");
        assert_eq!(req.path(), "/cards/CARD");
        assert!(req.has_query("idList", "DEST"));
        assert!(req.has_query("pos", "bottom"));
    }

    #[test]
    fn archive_card_puts_closed_true() {
        let stub = Stub::serve(ok_json("{}"));
        let client = stub.client("k", "t");
        client.archive_card("CARD").unwrap();

        let req = stub.captured();
        assert_eq!(req.method, "PUT");
        assert_eq!(req.path(), "/cards/CARD");
        assert!(req.has_query("closed", "true"));
    }

    #[test]
    fn complete_card_puts_duecomplete_true() {
        let stub = Stub::serve(ok_json("{}"));
        let client = stub.client("k", "t");
        client.complete_card("CARD").unwrap();

        let req = stub.captured();
        assert_eq!(req.method, "PUT");
        assert_eq!(req.path(), "/cards/CARD");
        assert!(req.has_query("dueComplete", "true"));
    }

    #[test]
    fn add_label_attaches_existing_label_by_id() {
        // The label already exists on the board: two requests — GET the board's
        // labels, then POST the resolved id onto the card's idLabels (no create).
        let labels = r#"[{"id":"lbl9","name":"Problem","color":null}]"#;
        let stub = Stub::serve_seq(vec![ok_json_close(labels), ok_json_close("{}")]);
        let client = stub.client("k", "t");

        client.add_label("BID", "CARD", "Problem").unwrap();

        let reqs = stub.captured_seq(2);
        assert_eq!(reqs[0].method, "GET");
        assert_eq!(reqs[0].path(), "/boards/BID/labels");
        assert!(reqs[0].has_query("key", "k"));
        assert!(reqs[0].has_query("token", "t"));
        assert_eq!(reqs[1].method, "POST");
        assert_eq!(reqs[1].path(), "/cards/CARD/idLabels");
        assert!(
            reqs[1].has_query("value", "lbl9"),
            "query: {:?}",
            reqs[1].query()
        );
    }

    #[test]
    fn add_label_creates_missing_label_then_attaches() {
        // The label is absent: three requests — GET (empty), POST a new board
        // label with `name` + `color=null`, then POST its id onto the card.
        let created = r#"{"id":"new1","name":"Problem","color":null}"#;
        let stub = Stub::serve_seq(vec![
            ok_json_close("[]"),
            ok_json_close(created),
            ok_json_close("{}"),
        ]);
        let client = stub.client("k", "t");

        client.add_label("BID", "CARD", "Problem").unwrap();

        let reqs = stub.captured_seq(3);
        assert_eq!(reqs[0].method, "GET");
        assert_eq!(reqs[0].path(), "/boards/BID/labels");
        assert_eq!(reqs[1].method, "POST");
        assert_eq!(reqs[1].path(), "/boards/BID/labels");
        assert!(
            reqs[1].has_query("name", "Problem"),
            "query: {:?}",
            reqs[1].query()
        );
        assert!(
            reqs[1].has_query("color", "null"),
            "query: {:?}",
            reqs[1].query()
        );
        assert_eq!(reqs[2].method, "POST");
        assert_eq!(reqs[2].path(), "/cards/CARD/idLabels");
        assert!(
            reqs[2].has_query("value", "new1"),
            "query: {:?}",
            reqs[2].query()
        );
    }

    #[test]
    fn remove_label_resolves_and_deletes() {
        // The label exists on the board: two requests — GET the board's labels, then
        // DELETE the resolved id off the card's idLabels. The mirror of `add_label`'s
        // resolve, DELETE-side instead of POST.
        let labels = r#"[{"id":"lbl9","name":"Redo","color":null}]"#;
        let stub = Stub::serve_seq(vec![ok_json_close(labels), ok_json_close("{}")]);
        let client = stub.client("k", "t");

        client.remove_label("BID", "CARD", "Redo").unwrap();

        let reqs = stub.captured_seq(2);
        assert_eq!(reqs[0].method, "GET");
        assert_eq!(reqs[0].path(), "/boards/BID/labels");
        assert_eq!(reqs[1].method, "DELETE");
        assert_eq!(reqs[1].path(), "/cards/CARD/idLabels/lbl9");
        assert!(
            reqs[1].has_query("key", "k"),
            "query: {:?}",
            reqs[1].query()
        );
        assert!(
            reqs[1].has_query("token", "t"),
            "query: {:?}",
            reqs[1].query()
        );
    }

    #[test]
    fn remove_label_absent_label_is_a_no_op() {
        // No board label carries the name ⇒ it can't be on the card: a no-op success
        // with exactly one request (the GET, no DELETE) — and it never creates a
        // label, unlike `add_label`.
        let stub = Stub::serve_seq(vec![ok_json_close("[]")]);
        let client = stub.client("k", "t");

        client.remove_label("BID", "CARD", "Redo").unwrap();

        let reqs = stub.captured_seq(1);
        assert_eq!(reqs[0].method, "GET");
        assert_eq!(reqs[0].path(), "/boards/BID/labels");
    }

    #[test]
    fn add_member_self_resolves_me_once_and_posts() {
        // Two `self` adds on the same client: the authed id is fetched once and
        // memoized, so the second add goes straight to the attach POST.
        let me = r#"{"id":"me1","username":"afkd-bot"}"#;
        let stub = Stub::serve_seq(vec![
            ok_json_close(me),
            ok_json_close("{}"),
            ok_json_close("{}"),
        ]);
        let client = stub.client("k", "t");

        client
            .add_member("BID", "CARD", &MemberRef::SelfMember)
            .unwrap();
        client
            .add_member("BID", "CARD", &MemberRef::SelfMember)
            .unwrap();

        let reqs = stub.captured_seq(3);
        assert_eq!(reqs[0].method, "GET");
        assert_eq!(reqs[0].path(), "/members/me");
        assert_eq!(reqs[1].method, "POST");
        assert_eq!(reqs[1].path(), "/cards/CARD/idMembers");
        assert!(
            reqs[1].has_query("value", "me1"),
            "query: {:?}",
            reqs[1].query()
        );
        // The memo holds: the second add issues no second `GET /members/me`.
        assert_eq!(reqs[2].method, "POST");
        assert_eq!(reqs[2].path(), "/cards/CARD/idMembers");
        assert!(reqs[2].has_query("value", "me1"));
    }

    #[test]
    fn add_member_by_username_resolves_and_posts() {
        // A username is resolved against the board's members, then attached by id.
        let members = r#"[{"id":"m1","username":"marisa"},{"id":"m2","username":"robin"}]"#;
        let stub = Stub::serve_seq(vec![ok_json_close(members), ok_json_close("{}")]);
        let client = stub.client("k", "t");

        client
            .add_member("BID", "CARD", &MemberRef::Username("marisa".into()))
            .unwrap();

        let reqs = stub.captured_seq(2);
        assert_eq!(reqs[0].method, "GET");
        assert_eq!(reqs[0].path(), "/boards/BID/members");
        assert!(reqs[0].has_query("key", "k"));
        assert!(reqs[0].has_query("token", "t"));
        assert_eq!(reqs[1].method, "POST");
        assert_eq!(reqs[1].path(), "/cards/CARD/idMembers");
        assert!(
            reqs[1].has_query("value", "m1"),
            "query: {:?}",
            reqs[1].query()
        );
    }

    #[test]
    fn add_member_unknown_username_is_member_not_found_and_posts_nothing() {
        // Resolution precedes the attach, so an unrecognized username never posts.
        // Exactly one response is queued: the stub's accept loop ends after the GET,
        // and the single captured request pins that it was the GET.
        let stub = Stub::serve_seq(vec![ok_json_close("[]")]);
        let client = stub.client("k", "t");

        let err = client
            .add_member("BID", "CARD", &MemberRef::Username("ghost".into()))
            .unwrap_err();
        assert!(
            matches!(&err, BoardError::MemberNotFound { stage: "add member", name } if name == "ghost"),
            "got {err:?}"
        );

        let reqs = stub.captured_seq(1);
        assert_eq!(reqs[0].method, "GET");
        assert_eq!(reqs[0].path(), "/boards/BID/members");
    }

    #[test]
    fn two_add_members_post_both_and_clear_nothing() {
        // The additive contract at the request level: two members, two attach POSTs
        // carrying distinct ids, and nothing that replaces or clears the member set.
        // Board membership is deliberately uncached, so each add re-resolves it.
        let members = r#"[{"id":"m1","username":"marisa"},{"id":"m2","username":"robin"}]"#;
        let stub = Stub::serve_seq(vec![
            ok_json_close(members),
            ok_json_close("{}"),
            ok_json_close(members),
            ok_json_close("{}"),
        ]);
        let client = stub.client("k", "t");

        client
            .add_member("BID", "CARD", &MemberRef::Username("marisa".into()))
            .unwrap();
        client
            .add_member("BID", "CARD", &MemberRef::Username("robin".into()))
            .unwrap();

        let reqs = stub.captured_seq(4);
        assert_eq!(reqs[0].path(), "/boards/BID/members");
        assert_eq!(reqs[2].path(), "/boards/BID/members");
        assert!(reqs[1].has_query("value", "m1"));
        assert!(reqs[3].has_query("value", "m2"));
        // Nothing replaces the member set: no PUT/DELETE, and no request rewrites
        // the card itself (`PUT /cards/CARD?idMembers=…` would be the replace).
        for req in &reqs {
            assert!(
                req.method == "GET" || req.method == "POST",
                "unexpected {} {}",
                req.method,
                req.target
            );
            assert_ne!(req.path(), "/cards/CARD", "nothing rewrites the card");
        }
    }

    #[test]
    fn add_member_swallows_the_already_a_member_fault() {
        // Trello faults a re-add rather than treating it as a no-op; the narrow
        // 400-with-that-message swallow keeps `add_member` idempotent across retries.
        let me = r#"{"id":"me1"}"#;
        let already = r#"{"message":"member is already on the card"}"#;
        let fault = format!(
            "HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{}",
            already.len(),
            already
        );
        let stub = Stub::serve_seq(vec![ok_json_close(me), fault]);
        let client = stub.client("k", "t");

        client
            .add_member("BID", "CARD", &MemberRef::SelfMember)
            .unwrap();

        let reqs = stub.captured_seq(2);
        assert_eq!(reqs[1].path(), "/cards/CARD/idMembers");
    }

    #[test]
    fn add_member_surfaces_an_unrecognized_attach_fault() {
        // A 400 that is *not* the re-add fault (here: a bad member id) still errors,
        // rather than silently claiming the member was added.
        let me = r#"{"id":"me1"}"#;
        let bad = r#"{"message":"invalid value for idMember"}"#;
        let fault = format!(
            "HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{}",
            bad.len(),
            bad
        );
        let stub = Stub::serve_seq(vec![ok_json_close(me), fault]);
        let client = stub.client("k", "t");

        let err = client
            .add_member("BID", "CARD", &MemberRef::SelfMember)
            .unwrap_err();
        assert!(
            matches!(
                err,
                BoardError::Status {
                    stage: "add member",
                    status: 400
                }
            ),
            "got {err:?}"
        );
        let _ = stub.captured_seq(2);
    }

    #[test]
    fn remove_member_self_resolves_me_and_deletes() {
        // The mirror of `add_member`: resolve the authed id, then DELETE it off the
        // card's members — the detach the config's `remove_member self` asks for.
        let me = r#"{"id":"me1","username":"afkd-bot"}"#;
        let stub = Stub::serve_seq(vec![ok_json_close(me), ok_json_close("{}")]);
        let client = stub.client("k", "t");

        client
            .remove_member("BID", "CARD", &MemberRef::SelfMember)
            .unwrap();

        let reqs = stub.captured_seq(2);
        assert_eq!(reqs[0].method, "GET");
        assert_eq!(reqs[0].path(), "/members/me");
        assert_eq!(reqs[1].method, "DELETE");
        assert_eq!(reqs[1].path(), "/cards/CARD/idMembers/me1");
        assert!(
            reqs[1].has_query("key", "k"),
            "query: {:?}",
            reqs[1].query()
        );
        assert!(
            reqs[1].has_query("token", "t"),
            "query: {:?}",
            reqs[1].query()
        );
    }

    #[test]
    fn remove_member_repeat_is_a_no_op() {
        // Trello's DELETE /idMembers is idempotent server-side, returning a 200 with
        // the card's remaining members whether or not the member was attached. So two
        // successive `self` removals both succeed, and the authed id is resolved once
        // (memoized) — the second removal goes straight to the DELETE.
        let me = r#"{"id":"me1","username":"afkd-bot"}"#;
        let stub = Stub::serve_seq(vec![
            ok_json_close(me),
            ok_json_close("{}"),
            ok_json_close("{}"),
        ]);
        let client = stub.client("k", "t");

        client
            .remove_member("BID", "CARD", &MemberRef::SelfMember)
            .unwrap();
        client
            .remove_member("BID", "CARD", &MemberRef::SelfMember)
            .unwrap();

        let reqs = stub.captured_seq(3);
        assert_eq!(reqs[0].method, "GET");
        assert_eq!(reqs[0].path(), "/members/me");
        assert_eq!(reqs[1].method, "DELETE");
        assert_eq!(reqs[1].path(), "/cards/CARD/idMembers/me1");
        // The memo holds: the second removal issues no second `GET /members/me`.
        assert_eq!(reqs[2].method, "DELETE");
        assert_eq!(reqs[2].path(), "/cards/CARD/idMembers/me1");
    }

    #[test]
    fn remove_member_unknown_username_is_member_not_found_and_deletes_nothing() {
        // Resolution precedes the DELETE, so an unrecognized username never touches
        // the card. Exactly one response is queued: the stub's accept loop ends after
        // the GET, and the single captured request pins that it was the GET.
        let stub = Stub::serve_seq(vec![ok_json_close("[]")]);
        let client = stub.client("k", "t");

        let err = client
            .remove_member("BID", "CARD", &MemberRef::Username("ghost".into()))
            .unwrap_err();
        assert!(
            matches!(&err, BoardError::MemberNotFound { stage: "remove member", name } if name == "ghost"),
            "got {err:?}"
        );

        let reqs = stub.captured_seq(1);
        assert_eq!(reqs[0].method, "GET");
        assert_eq!(reqs[0].path(), "/boards/BID/members");
    }

    #[test]
    fn add_member_maps_a_transport_failure_on_the_attach() {
        // The attach POST inspects the error body itself, so it cannot reuse `body`'s
        // outcome mapping and its transport arm needs its own proof. The `me` GET is
        // served; the attach connection is then closed with no reply at all, which is
        // a transport failure — tagged with the `add member` stage.
        let stub = Stub::serve_seq(vec![ok_json_close(r#"{"id":"me1"}"#), String::new()]);
        let client = stub.client("k", "t");

        let err = client
            .add_member("BID", "CARD", &MemberRef::SelfMember)
            .unwrap_err();
        assert!(
            matches!(
                err,
                BoardError::Transport {
                    stage: "add member",
                    ..
                }
            ),
            "got {err:?}"
        );
        let _ = stub.captured_seq(2);
    }

    #[test]
    fn non_success_status_maps_to_status_error_tagged_with_stage() {
        let resp = "HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\n\r\nnot found";
        let stub = Stub::serve(resp);
        let client = stub.client("k", "t");
        let err = client.list_cards("L").unwrap_err();
        assert!(
            matches!(
                err,
                BoardError::Status {
                    stage: "list cards",
                    status: 404
                }
            ),
            "got {err:?}"
        );
        let _ = stub.captured();
    }

    #[test]
    fn server_error_status_maps_to_status_error() {
        let resp = "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n";
        let stub = Stub::serve(resp);
        let client = stub.client("k", "t");
        let err = client.resolve_list("B", "X").unwrap_err();
        assert!(
            matches!(
                err,
                BoardError::Status {
                    stage: "resolve list",
                    status: 503
                }
            ),
            "got {err:?}"
        );
        let _ = stub.captured();
    }

    #[test]
    fn malformed_json_body_maps_to_decode_error_tagged_with_stage() {
        let resp = ok_json("this is not json");
        let stub = Stub::serve(resp);
        let client = stub.client("k", "t");
        let err = client.list_cards("L").unwrap_err();
        assert!(
            matches!(
                err,
                BoardError::Decode {
                    stage: "list cards",
                    ..
                }
            ),
            "got {err:?}"
        );
        let _ = stub.captured();
    }

    #[test]
    fn transport_failure_to_a_dead_port_maps_to_transport_error() {
        // Bind then drop a listener so the port is (almost certainly) free, and
        // point the client at it: connecting fails, exercising the Transport arm.
        let port = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let client = TrelloClient::with_base(format!("http://127.0.0.1:{port}"), "k", "t");
        let err = client.list_cards("L").unwrap_err();
        assert!(
            matches!(
                err,
                BoardError::Transport {
                    stage: "list cards",
                    ..
                }
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn post_comment_body_carries_special_characters_verbatim_as_json() {
        // Adversarial text — a query separator (`&`), a `=`, a newline, and a
        // non-ASCII char — that would all need escaping on a request line. In the
        // JSON body it round-trips verbatim: the body is exactly the serialization
        // of `{"text": text}`, and nothing lands (escaped or not) on the target.
        let body = r#"{"id":"5f0000000000000000000009","data":{"text":"x"}}"#;
        let stub = Stub::serve(ok_json(body));
        let client = stub.client("k", "t");
        let text = "a&b = c\nnågot 🎉";
        client.post_comment("CARD", text).unwrap();

        let req = stub.captured();
        assert_eq!(req.body, serde_json::json!({ "text": text }).to_string());
        // Nothing to percent-encode: the text never touched the request line, so
        // the target carries no `%`-escape at all.
        assert!(!req.target.contains('%'), "target: {}", req.target);
        assert!(!req.target.contains("a&b"), "target: {}", req.target);
    }

    #[test]
    fn transport_error_reason_never_carries_the_auth_query() {
        // A transport failure must not fold the request URL — which carries the
        // live `?key=…&token=…` auth — into the error the trigger logs to stderr.
        // Point the client (with a recognizable token) at a dead port and assert
        // the secret appears nowhere in the resulting BoardError, Display included.
        let port = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let token = "SUPERSECRET-TOKEN-9f3a";
        let key = "SUPERSECRET-KEY-1b2c";
        let client = TrelloClient::with_base(format!("http://127.0.0.1:{port}"), key, token);
        let err = client.list_cards("L").unwrap_err();

        let BoardError::Transport { reason, .. } = &err else {
            panic!("expected a transport error, got {err:?}");
        };
        assert!(
            !reason.contains(token) && !reason.contains(key),
            "auth leaked into transport reason: {reason}"
        );
        // The user-facing Display (what `log_board` prints) must also stay clean.
        let shown = err.to_string();
        assert!(
            !shown.contains(token) && !shown.contains(key),
            "auth leaked into the logged error: {shown}"
        );
    }

    /// Bind a loopback port and accept one connection, draining the request but
    /// never writing a reply and holding the socket open — so a client with a
    /// finite read timeout must surface a transport error rather than block
    /// forever. The serving thread is detached (it self-expires); the test never
    /// joins it, so it returns as soon as the client's read budget elapses.
    fn silent_base() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                // Read the request so ureq finishes sending, then sit on the open
                // socket without ever replying. The hold is far longer than the
                // test's assertion margin so the client's *read timeout* is always
                // what ends the call — a slow run fails as "too slow", never by
                // racing this detached thread's socket drop.
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                std::thread::sleep(Duration::from_secs(10));
                drop(stream);
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    #[test]
    fn a_wedged_request_times_out_as_a_transport_error_within_the_request_deadline() {
        // AC#3 regression guard, on a *fresh* connect: a board call to a server that
        // accepts but never responds must not park the thread forever. With short
        // budgets it degrades to a `BoardError::Transport` (which the trigger's
        // wind-down already releases the slot past), and does so well inside them.
        // The socket is armed from the request deadline (connect + read = 1.2s here)
        // rather than the read timeout alone, since the agent now sets one.
        let client = TrelloClient::with_base_timeouts(
            silent_base(),
            "k",
            "t",
            Duration::from_secs(1),
            Duration::from_millis(200),
        );
        let start = std::time::Instant::now();
        let err = client.card_comments("CARD").unwrap_err();
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "should time out near the 1.2s request deadline, took {:?}",
            start.elapsed()
        );
        assert!(
            matches!(
                err,
                BoardError::Transport {
                    stage: "read comments",
                    ..
                }
            ),
            "got {err:?}"
        );
    }

    /// Bind a loopback port and serve **one** connection twice: request #1 is
    /// answered with a keep-alive `ok_json` reply (no `Connection: close`, exact
    /// `Content-Length`), so ureq pools the socket once the body is drained; request
    /// #2 is then read *off that same socket*, handed back over the channel, and
    /// never answered — the socket held open far past the assertion margin.
    ///
    /// So the second call genuinely rides a recycled connection, which is the case
    /// with no timeouts of its own: only the request deadline can end it. A second
    /// dial-out is never accepted, so a client that declined to reuse would leave
    /// the channel empty rather than passing vacuously. The serving thread is
    /// detached and self-expires — the test never joins it.
    fn recycled_then_silent_base() -> (String, mpsc::Receiver<Captured>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            // Request #1: answered keep-alive, so the socket returns to the pool.
            let _ = read_request(&stream);
            if stream.write_all(ok_json("[]").as_bytes()).is_err() {
                return;
            }
            stream.flush().ok();
            // Request #2, on the recycled socket: captured (so the test can prove
            // the reuse happened) and then never answered. The hold outlasts the
            // assertion margin, so a slow run fails as "too slow" rather than by
            // racing this detached thread's socket drop.
            tx.send(read_request(&stream)).ok();
            std::thread::sleep(Duration::from_secs(10));
            drop(stream);
        });
        (format!("http://127.0.0.1:{port}"), rx)
    }

    #[test]
    fn a_recycled_connection_still_honours_the_request_deadline() {
        // The fresh-connect wedged test above only covers the *first* request on a
        // socket. This is the pooled-reuse case the spine has to hold: a recycled
        // connection skips the dial, so `timeout_connect` never applies to it and
        // the budgets that end it are the per-op ones plus the global deadline
        // (`connect + read`) measured from the request's start.
        let (base, second_request) = recycled_then_silent_base();
        let client = TrelloClient::with_base_timeouts(
            base,
            "k",
            "t",
            Duration::from_secs(1),
            Duration::from_millis(200),
        );
        // Call #1 succeeds and leaves the keep-alive socket in the pool.
        assert!(
            client.card_comments("CARD").unwrap().is_empty(),
            "empty comment list expected"
        );
        // Call #2 rides that recycled connection; it must trip the deadline rather
        // than block forever.
        let start = std::time::Instant::now();
        let err = client.card_comments("CARD").unwrap_err();
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "recycled call should time out near the 1.2s request deadline, took {:?}",
            start.elapsed()
        );
        assert!(
            matches!(
                err,
                BoardError::Transport {
                    stage: "read comments",
                    ..
                }
            ),
            "got {err:?}"
        );
        // And it really was a reuse: the stub accepted one connection, and request
        // #2 arrived on it. Without this the test would pass just as well against a
        // client that opened a fresh socket and tripped the same deadline.
        let second = second_request
            .recv_timeout(Duration::from_secs(1))
            .expect("request #2 must ride the pooled socket — the stub accepts once");
        assert_eq!(second.path(), "/cards/CARD/actions");
    }

    #[test]
    fn url_decode_handles_percent_plus_and_passthrough() {
        assert_eq!(url_decode("a%20b"), "a b");
        assert_eq!(url_decode("a+b"), "a b");
        assert_eq!(url_decode("plain"), "plain");
        // A stray, un-decodable `%` is passed through verbatim.
        assert_eq!(url_decode("100%done"), "100%done");
    }

    #[test]
    fn captured_query_is_empty_when_the_target_has_no_question_mark() {
        let cap = Captured {
            method: "GET".into(),
            target: "/no/query".into(),
            headers: Vec::new(),
            body: String::new(),
        };
        assert!(cap.query().is_empty());
        assert_eq!(cap.path(), "/no/query");
    }

    #[test]
    fn a_client_is_built_off_the_base_it_is_handed() {
        let client = TrelloClient::with_base(TRELLO_BASE, "k", "t");
        assert_eq!(client.http.base(), "https://api.trello.com/1");
    }

    #[test]
    fn object_id_timestamp_reads_leading_hex_seconds() {
        // 0x5f000000 = 1593820160; the remaining 16 hex chars are ignored.
        let t = object_id_timestamp("5f0000000000000000000000").unwrap();
        assert_eq!(
            t.duration_since(UNIX_EPOCH).unwrap(),
            Duration::from_secs(0x5f00_0000)
        );
        assert!(object_id_timestamp("short").is_none());
        assert!(object_id_timestamp("zzzzzzzz0000000000000000").is_none());
        // A malformed id whose first 8 bytes straddle a multi-byte char must read as
        // `None` (the documented "malformed id" outcome), not panic mid-codepoint —
        // the function takes whatever the forge returns and must never crash on it.
        assert!(object_id_timestamp("1234567\u{1f600}").is_none());
    }

    #[test]
    fn parse_lists_finds_id_and_name_pairs() {
        let body = r#"[{"id":"l1","name":"Up for Grabs"},{"id":"l2","name":"In Progress"}]"#;
        let lists = parse_lists("resolve list", body).unwrap();
        assert_eq!(
            lists,
            vec![
                ("l1".to_string(), "Up for Grabs".to_string()),
                ("l2".to_string(), "In Progress".to_string())
            ]
        );
    }

    #[test]
    fn parse_labels_finds_id_and_name_pairs() {
        let body = r#"[{"id":"l1","name":"Problem","color":"red"},
                       {"id":"l2","name":"Blocked","color":null},
                       {"id":"l3","color":"green"}]"#;
        let labels = parse_labels("add label", body).unwrap();
        // The third label carries no name and is skipped.
        assert_eq!(
            labels,
            vec![
                ("l1".to_string(), "Problem".to_string()),
                ("l2".to_string(), "Blocked".to_string()),
            ]
        );
    }

    #[test]
    fn parse_members_finds_id_and_username_pairs() {
        let body = r#"[{"id":"m1","username":"marisa","fullName":"Marisa"},
                       {"id":"m2","username":"robin"},
                       {"id":"m3","fullName":"No Username"}]"#;
        let members = parse_members("add member", body).unwrap();
        // The third member carries no username and is skipped.
        assert_eq!(
            members,
            vec![
                ("m1".to_string(), "marisa".to_string()),
                ("m2".to_string(), "robin".to_string()),
            ]
        );
        let err = parse_members("add member", r#"{"not":"an array"}"#).unwrap_err();
        assert!(matches!(
            err,
            BoardError::Decode {
                stage: "add member",
                ..
            }
        ));
    }

    #[test]
    fn parse_member_id_reads_the_id_else_decode_errors() {
        assert_eq!(
            parse_member_id("add member", r#"{"id":"me1","username":"afkd-bot"}"#).unwrap(),
            "me1"
        );
        let err = parse_member_id("add member", "{}").unwrap_err();
        assert!(matches!(
            err,
            BoardError::Decode {
                stage: "add member",
                ..
            }
        ));
    }

    #[test]
    fn already_a_member_only_matches_the_documented_fault() {
        // Trello's two phrasings for a re-add, matched case-insensitively on a 400.
        assert!(already_a_member(
            400,
            "member is already a member of the card"
        ));
        assert!(already_a_member(400, "That member is Already On The Card"));
        // A different 400 (a bad member id) must still fault, as must any other
        // status carrying the message.
        assert!(!already_a_member(400, "invalid value for idMember"));
        assert!(!already_a_member(404, "already a member"));
        assert!(!already_a_member(500, ""));
    }

    #[test]
    fn parse_created_id_reads_the_id_else_decode_errors() {
        assert_eq!(
            parse_created_id("add label", r#"{"id":"new1","name":"Problem"}"#).unwrap(),
            "new1"
        );
        let err = parse_created_id("add label", r#"{"name":"no id"}"#).unwrap_err();
        assert!(matches!(
            err,
            BoardError::Decode {
                stage: "add label",
                ..
            }
        ));
    }

    #[test]
    fn parse_cards_maps_name_to_title_and_desc_to_description() {
        let body = r#"[{"id":"c1","name":"Fix bug","desc":"steps here"},
                       {"id":"c2","name":"No desc"}]"#;
        let cards = parse_cards("list cards", body).unwrap();
        assert_eq!(cards.len(), 2);
        assert_eq!(cards[0].title, "Fix bug");
        assert_eq!(cards[0].description, "steps here");
        assert_eq!(cards[1].description, "");
        // A card with no `checklists` key parses to an empty vec, not a panic.
        assert!(cards[0].checklists.is_empty());
    }

    #[test]
    fn parse_cards_lifts_id_members() {
        // `idMembers` becomes `Card.members`, in board order — the field the intake
        // gate filters on. An absent key, and a field that is not an array at all,
        // both read as "nobody is on this card" rather than panicking.
        let body = r#"[{"id":"c1","name":"Two","idMembers":["m1","m2"]},
                       {"id":"c2","name":"None"},
                       {"id":"c3","name":"Junk","idMembers":"nope"},
                       {"id":"c4","name":"Mixed","idMembers":["m3",7]}]"#;
        let cards = parse_cards("list cards", body).unwrap();
        assert_eq!(cards[0].members, vec!["m1".to_string(), "m2".to_string()]);
        assert!(cards[1].members.is_empty());
        assert!(cards[2].members.is_empty());
        // A non-string element is skipped, not coerced.
        assert_eq!(cards[3].members, vec!["m3".to_string()]);
    }

    #[test]
    fn parse_cards_lifts_labels() {
        // The `labels` array's names become `Card.labels`, in board order — the
        // field the `require_label` gate filters on. An absent key and a non-array
        // field both read as "no labels"; a colour-only label (missing, empty, or
        // non-string `name`) is skipped rather than matched, and never panics.
        let body = r#"[{"id":"c1","name":"Two","labels":[{"id":"l1","name":"Redo","color":"green"},{"id":"l2","name":"Bug","color":"red"}]},
                       {"id":"c2","name":"None"},
                       {"id":"c3","name":"Junk","labels":"nope"},
                       {"id":"c4","name":"ColourOnly","labels":[{"id":"l3","color":"blue"},{"id":"l4","name":"","color":"black"},{"id":"l5","name":"Kept"}]}]"#;
        let cards = parse_cards("list cards", body).unwrap();
        assert_eq!(cards[0].labels, vec!["Redo".to_string(), "Bug".to_string()]);
        assert!(cards[1].labels.is_empty());
        assert!(cards[2].labels.is_empty());
        // The nameless and empty-name labels are skipped; only the named one survives.
        assert_eq!(cards[3].labels, vec!["Kept".to_string()]);
    }

    #[test]
    fn parse_cards_lifts_short_link() {
        // `shortLink` becomes `Card.short_link` — the handle a run is named after.
        // A key that is absent, empty, or not a string falls back to the card's `id`,
        // so the naming site never has to defend against an empty path component.
        let body = r#"[{"id":"c1","name":"Linked","shortLink":"1Rkelydw"},
                       {"id":"c2","name":"Absent"},
                       {"id":"c3","name":"Empty","shortLink":""},
                       {"id":"c4","name":"Junk","shortLink":7}]"#;
        let cards = parse_cards("list cards", body).unwrap();
        assert_eq!(cards[0].short_link, "1Rkelydw");
        assert_eq!(cards[1].short_link, "c2");
        assert_eq!(cards[2].short_link, "c3");
        assert_eq!(cards[3].short_link, "c4");
    }

    /// The nested-comment decode, over the three shapes the board actually answers
    /// with, and against the per-card route it replaces.
    ///
    /// The populated card carries what a groomed card's thread really looks like:
    /// multi-line text with a blank line and trailing spaces, CJK + an emoji + an
    /// em dash, a creator whose `fullName` is blank (so the `username` fallback
    /// fires), one whose `memberCreator` is missing entirely (so the raw member id
    /// stands in), an empty-string body, and an action with no `data.text` at all —
    /// which is dropped rather than decoding as an empty comment.
    #[test]
    fn list_cards_nests_comments_and_distinguishes_absent_from_empty() {
        let actions = r#"[
            {"id":"5f0000000000000000000001","idMemberCreator":"m1",
             "data":{"text":"first line\n\n  看起来不对 🚨 — the **bold** claim in §2 is wrong  "},
             "memberCreator":{"fullName":"Ada Lovelace","username":"ada"}},
            {"id":"5f0000000000000000000002","idMemberCreator":"m2",
             "data":{"text":"naïve"},
             "memberCreator":{"fullName":"   ","username":"bob-döner"}},
            {"id":"5f0000000000000000000003","idMemberCreator":"m3","data":{"text":""}},
            {"id":"5f0000000000000000000004","idMemberCreator":"m4","data":{}}
        ]"#;
        let body = format!(
            r#"[{{"id":"c1","name":"Nested","actions":{actions}}},
                {{"id":"c2","name":"Quiet","actions":[]}},
                {{"id":"c3","name":"Unasked"}},
                {{"id":"c4","name":"Odd","actions":"not-an-array"}}]"#
        );
        let cards = parse_cards("list cards", &body).unwrap();

        // The board supplied a thread: `Some`, in the order it gave (newest first,
        // which the tail gate does not depend on), minus the textless action.
        let nested = cards[0].comments.as_ref().expect("c1 carries its comments");
        assert_eq!(nested.len(), 3, "the action with no text is dropped");
        // Every field the tail gate and the brief read survives the nesting — and
        // reads *identically* to the per-card route over the same actions, which is
        // the property that lets one replace the other.
        let per_card = parse_comments("read comments", actions).unwrap();
        assert_eq!(nested, &per_card, "both routes decode one thread alike");
        assert_eq!(
            nested[0].text, "first line\n\n  看起来不对 🚨 — the **bold** claim in §2 is wrong  ",
            "the body is carried verbatim, blank line and trailing spaces included"
        );
        assert_eq!(nested[0].author, "m1");
        assert_eq!(nested[0].author_name, "Ada Lovelace");
        assert_eq!(
            nested[1].author_name, "bob-döner",
            "a blank fullName falls back to the username"
        );
        assert_eq!(
            nested[2].author_name, "m3",
            "no memberCreator at all falls back to the raw member id"
        );
        assert_eq!(nested[2].text, "", "an empty body is still a comment");
        // The post time the `discuss_with` boundary keys on rides the action id.
        assert_eq!(
            nested[0].posted_at,
            object_id_timestamp("5f0000000000000000000001").unwrap()
        );

        // A card nobody has commented on: `Some(vec![])` — the board *did* answer,
        // so the tail gate reads it as first sight without paying for a read.
        assert_eq!(cards[1].comments.as_deref(), Some(&[][..]));
        // No `actions` key: the board named none, so the caller pays for the read.
        assert_eq!(cards[2].comments, None);
        // And a key of the wrong shape reads the same fail-safe way, rather than
        // silently claiming the card has no comments.
        assert_eq!(cards[3].comments, None);
    }

    #[test]
    fn list_cards_sends_no_fields_param() {
        // The regression lock behind the intake gate costing nothing: `list_cards`
        // passes no `fields=`, so Trello returns its DEFAULT card fields — which
        // include `idMembers` and `shortLink`. Narrowing the fetch with a `fields=`
        // param for some future need would silently drop `idMembers` and make
        // `require_member` match no card at all, and drop `shortLink` so every run
        // fell back to being named from the internal id; so the two params it does
        // send are pinned too.
        let stub = Stub::serve(ok_json("[]"));
        let client = stub.client("k", "t");
        client.list_cards("L9").unwrap();

        let req = stub.captured();
        assert!(
            !req.query().iter().any(|(k, _)| k == "fields"),
            "list_cards must not narrow the card fields: {:?}",
            req.query()
        );
        assert!(req.has_query("checklists", "all"));
        assert!(req.has_query("checkItems", "all"));
        // The comment nesting rides the same fetch, so a `discuss_with` poll costs
        // one request rather than one per card. Dropping any of these three silently
        // puts the tail gate back on the per-card fan-out.
        assert!(req.has_query("actions", "commentCard"));
        assert!(req.has_query("actions_limit", "1000"));
        assert!(req.has_query("action_memberCreator_fields", "fullName,username"));
    }

    #[test]
    fn resolve_member_self_is_memoized() {
        // The intake gate resolves `self` once per poll; the memo makes every poll
        // after the first cost nothing. One `/members/me` response is queued, so a
        // second GET would hang the second resolve rather than quietly pass.
        let stub = Stub::serve_seq(vec![ok_json_close(r#"{"id":"me1"}"#)]);
        let client = stub.client("k", "t");

        assert_eq!(
            client
                .resolve_member("BID", &MemberRef::SelfMember)
                .unwrap(),
            "me1"
        );
        assert_eq!(
            client
                .resolve_member("BID", &MemberRef::SelfMember)
                .unwrap(),
            "me1"
        );

        let reqs = stub.captured_seq(1);
        assert_eq!(reqs[0].method, "GET");
        assert_eq!(reqs[0].path(), "/members/me");
    }

    #[test]
    fn resolve_member_by_username_is_not_memoized() {
        // Board membership is live state, so the username form re-reads it every
        // time — two resolves, two `GET /boards/BID/members`.
        let members = r#"[{"id":"m1","username":"marisa"}]"#;
        let stub = Stub::serve_seq(vec![ok_json_close(members), ok_json_close(members)]);
        let client = stub.client("k", "t");

        let marisa = MemberRef::Username("marisa".into());
        assert_eq!(client.resolve_member("BID", &marisa).unwrap(), "m1");
        assert_eq!(client.resolve_member("BID", &marisa).unwrap(), "m1");

        let reqs = stub.captured_seq(2);
        assert_eq!(reqs[0].path(), "/boards/BID/members");
        assert_eq!(reqs[1].path(), "/boards/BID/members");
    }

    #[test]
    fn resolve_member_unknown_username_is_tagged_with_the_gate_stage() {
        // The gate and the lifecycle action share one resolution path but keep their
        // own stage labels, so a swallowed poll error says which one failed. The
        // `add member` half is pinned by
        // `add_member_unknown_username_is_member_not_found_and_posts_nothing`.
        let stub = Stub::serve_seq(vec![ok_json_close("[]")]);
        let client = stub.client("k", "t");

        let err = client
            .resolve_member("BID", &MemberRef::Username("ghost".into()))
            .unwrap_err();
        assert!(
            matches!(&err, BoardError::MemberNotFound { stage: "resolve member", name } if name == "ghost"),
            "got {err:?}"
        );
        let _ = stub.captured_seq(1);
    }

    #[test]
    fn parse_check_item_reads_state_and_defaults_incomplete() {
        // `state:"complete"` ⇒ complete; anything else (here "incomplete", and an
        // absent state) ⇒ false.
        let done = serde_json::json!({"id":"i1","name":"a","state":"complete"});
        let open = serde_json::json!({"id":"i2","name":"b","state":"incomplete"});
        let bare = serde_json::json!({"id":"i3","name":"c"});
        assert!(parse_check_item(&done).unwrap().complete);
        assert!(!parse_check_item(&open).unwrap().complete);
        assert!(!parse_check_item(&bare).unwrap().complete);
        // An item missing its id (or name) is skipped rather than parsed.
        assert!(parse_check_item(&serde_json::json!({"name":"no id"})).is_none());
    }

    #[test]
    fn parse_checklists_absent_field_is_empty() {
        // A card object with no `checklists` key yields an empty vec.
        assert!(parse_checklists(&serde_json::json!({"id":"c1","name":"x"})).is_empty());
    }

    #[test]
    fn parse_comments_pulls_text_author_and_time_from_action() {
        let body = r#"[{"id":"5f0000000000000000000001","idMemberCreator":"m1",
                        "data":{"text":"hello"}}]"#;
        let comments = parse_comments("read comments", body).unwrap();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].text, "hello");
        assert_eq!(comments[0].author, "m1");
        assert_eq!(
            comments[0].posted_at.duration_since(UNIX_EPOCH).unwrap(),
            Duration::from_secs(0x5f00_0000)
        );
    }

    /// The liveness half of a comment: Trello stamps an **edited** action with
    /// `data.dateLastEdited` and writes nothing until then, so a renewed claim reads
    /// back its edit time and an ordinary comment falls back to its ObjectId post
    /// time. Real 24-hex ids and a real RFC-3339 stamp with an offset, since that
    /// offset is what the reader has to normalise.
    #[test]
    fn an_edited_action_carries_its_last_edit_as_the_renewal_time() {
        let body = r#"[{"id":"6a7f118d9a663d521d85c645","idMemberCreator":"m1",
                        "data":{"text":"[afkd-claim] owner=björn-öst[bot] renewal=12",
                                "dateLastEdited":"2026-09-01T21:21:07+02:00"}},
                       {"id":"6a7f118d9a663d521d85c646","idMemberCreator":"m2",
                        "data":{"text":"still stuck on this one 🤔\n\nsee the log above"}}]"#;
        let comments = parse_comments("read comments", body).unwrap();
        assert_eq!(comments.len(), 2);
        // 2026-09-01T19:21:07Z, whichever offset spelled it — not the ObjectId's
        // second, which is what an unedited comment falls back to.
        let renewed = comments[0].renewed_at.duration_since(UNIX_EPOCH).unwrap();
        assert_eq!(renewed, Duration::from_secs(1_788_290_467));
        assert_ne!(comments[0].renewed_at, comments[0].posted_at);
        assert_eq!(
            comments[0].posted_at.duration_since(UNIX_EPOCH).unwrap(),
            Duration::from_secs(0x6a7f_118d)
        );
        // No `dateLastEdited`: unedited, so the two times agree — the pre-renewal rule.
        assert_eq!(comments[1].renewed_at, comments[1].posted_at);
        assert_eq!(
            comments[1].renewed_at.duration_since(UNIX_EPOCH).unwrap(),
            Duration::from_secs(0x6a7f_118d)
        );
    }

    /// A `dateLastEdited` the reader cannot make sense of degrades to the post time
    /// rather than to the epoch — an unreadable stamp must not make a live claim look
    /// 56 years stale.
    #[test]
    fn an_unreadable_last_edit_falls_back_to_the_post_time() {
        for stamp in [r#""not a date""#, "null", "1788290467", r#""""#] {
            let body = format!(
                r#"[{{"id":"6a7f118d9a663d521d85c645","data":{{"text":"x","dateLastEdited":{stamp}}}}}]"#
            );
            let comments = parse_comments("read comments", &body).unwrap();
            assert_eq!(
                comments[0].renewed_at, comments[0].posted_at,
                "{stamp} did not fall back to the post time"
            );
        }
    }

    #[test]
    fn parse_posted_reads_the_single_returned_action() {
        let body = r#"{"id":"5f0000000000000000000009","data":{"text":"claim"}}"#;
        let posted = parse_posted("post comment", body).unwrap();
        assert_eq!(posted.id, "5f0000000000000000000009");
        assert_eq!(posted.text, "claim");
    }

    #[test]
    fn decode_failures_are_tagged_with_their_stage() {
        let err = parse_lists("resolve list", "not json").unwrap_err();
        assert_eq!(err.stage(), "resolve list");
        let err = parse_cards("list cards", r#"{"not":"an array"}"#).unwrap_err();
        assert!(matches!(err, BoardError::Decode { .. }));
    }

    #[test]
    fn pos_value_maps_top_and_bottom() {
        assert_eq!(pos_value(ListPosition::Top), "top");
        assert_eq!(pos_value(ListPosition::Bottom), "bottom");
    }
}
