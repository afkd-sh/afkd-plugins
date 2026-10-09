//! The seam a kind sits behind: what [`crate::plugin`] drives per call, and what it reads
//! off a claimed unit. The wire's bookkeeping — the identity, the live units, the 64 KiB
//! fitting, the `comments` delta, the finished units — is written once in `plugin.rs`
//! against these two traits, and a slot's actions and the run-end release go through the
//! one [`Outbox`], so a kind differs only in its vendor half ([`crate::issue`],
//! [`crate::pr`]).

use std::sync::Arc;
use std::time::Instant;

use crate::client::{GithubClient, GithubError, IssueComment};
use crate::common::{Clock, Diag};
use crate::lifecycle::Vocabulary;
use crate::outbox::{Delivery, Item, Outbox, Write};
use crate::wire::WireUnit;

/// A claimed unit, as the plugin's bookkeeping reads it.
pub(crate) trait ClaimedUnit {
    /// The unit's stable coordinate, `<owner>/<name>#<number>` of its issue or PR — the
    /// wire unit's `thread`.
    fn thread(&self) -> String;

    /// The login the unit was claimed as — the wire unit's `self`.
    fn claimed_as(&self) -> &str;

    /// The unit's issue or pull request, as the [`Outbox`] writes to it and names it.
    fn item(&self) -> Item;
}

/// One kind's vendor half: the calls `plugin.rs` makes per request.
pub(crate) trait Units {
    /// The unit this kind claims.
    type Unit: ClaimedUnit;

    /// The optional calls this kind answers, exactly as `hello` lists them. A call the
    /// kind does not list is refused before it reaches the kind.
    const CALLS: &'static [&'static str];

    /// What this kind's services call its actions by, and the handle parameter naming the
    /// item each acts on.
    const VOCABULARY: Vocabulary;

    /// Set (or, with `None`, clear) the current call's deadline: kept for the poll's
    /// claim reserve, and handed to the client, which clips every request to it.
    fn set_call_deadline(&self, deadline: Option<Instant>);

    /// Resolve the authenticated user's login (the claim identity): GitHub's assignee
    /// writes and its marker owner both name a login, so the login is the identity.
    fn resolve_me(&self) -> Result<String, GithubError>;

    /// Find the first eligible unit and claim it. An error is the built-in's idle beat.
    fn try_claim_next(
        &self,
        me: &str,
        diag: &dyn Diag,
        clock: &dyn Clock,
    ) -> Result<Option<Self::Unit>, GithubError>;

    /// The unit as it crosses the wire in a `poll` reply.
    fn wire_unit(&self, unit: &Self::Unit) -> WireUnit;

    /// Release this unit's claim now, as the identity it was claimed as.
    fn release(&self, unit: &Self::Unit, diag: &dyn Diag);

    /// Release one claim named by a whole journal key, as `me` — the `release` reply's
    /// value.
    fn release_stale(&self, key: &str, me: &str, diag: &dyn Diag) -> Option<bool>;

    /// Keep this unit's claim marker alive for as long as afkd's fire holds it.
    fn renew(&self, unit: &Self::Unit, renewal: u64, diag: &dyn Diag);

    /// Every comment on the unit's thread, for afkd's mid-run watch.
    fn comments(&self, unit: &Self::Unit) -> Result<Vec<IssueComment>, GithubError>;

    /// The plugin-owned end of a unit's run, whatever its outcome: the claim marker's
    /// release, through the [`Outbox`] so the post-run slot's actions queue behind it. One
    /// GitHub cannot be reached for is queued and retried; one it refuses is said lost, and
    /// a marker left behind ages out.
    fn finish(&self, unit: &Self::Unit, now: Instant, diag: &dyn Diag);

    /// The forge client the kind's calls go through.
    fn client(&self) -> &dyn GithubClient;

    /// The writes this kind owes the forge, for the worker that delivers them.
    fn outbox(&self) -> &Arc<Outbox>;

    /// Do `write` on the unit's item as the login it was claimed as, or queue it (see
    /// [`Outbox::deliver`]): an action a slot called, or the run-end release.
    fn deliver(
        &self,
        unit: &Self::Unit,
        write: Write,
        now: Instant,
        diag: &dyn Diag,
    ) -> Result<Delivery, GithubError> {
        self.outbox()
            .deliver(self.client(), &unit.item(), write, now, diag)
    }
}
