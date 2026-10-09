//! The seam both kinds sit behind: what [`crate::plugin`] drives per call, and what it
//! reads off a claimed unit. The wire's bookkeeping — the live and finished units, the
//! 64 KiB fitting, the `comments` delta, the undelivered-park fallback — is written once
//! in `plugin.rs` against these two traits, and a slot's actions and the kind's run-end
//! writes go through the one [`Outbox`], so the `issue` and `pr` kinds differ only in
//! their vendor half ([`crate::issue`], [`crate::pr`]).

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use crate::client::{GiteaClient, GiteaError, IssueComment};
use crate::common::{ClaimFault, Clock, Diag};
use crate::lifecycle::Vocabulary;
use crate::outbox::{Delivery, Item, Outbox, Write};
use crate::wire::{Facts, UnitOutcome, WireUnit};

/// A claimed unit, as the plugin's bookkeeping reads it.
pub(crate) trait ClaimedUnit {
    /// The unit's stable coordinate, `<full-name>#<number>` — the wire unit's `thread`.
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

    /// Resolve the authenticated user (the claim identity).
    fn resolve_me(&self) -> Result<String, GiteaError>;

    /// Find the first eligible unit across the polled repos and claim it.
    fn try_claim_next(
        &self,
        me: &str,
        diag: &dyn Diag,
        clock: &dyn Clock,
    ) -> Result<Option<Self::Unit>, ClaimFault<GiteaError>>;

    /// The unit as it crosses the wire in a `poll` reply.
    fn wire_unit(&self, unit: &Self::Unit) -> WireUnit;

    /// Release this unit's claim now.
    fn release(&self, unit: &Self::Unit, diag: &dyn Diag);

    /// Release one claim named by a whole journal key — the `release` reply's value.
    fn release_stale(&self, key: &str, diag: &dyn Diag) -> Option<bool>;

    /// Keep this unit's claim marker alive for as long as afkd's fire holds it.
    fn renew(&self, unit: &Self::Unit, renewal: u64, diag: &dyn Diag);

    /// Every comment on the unit's thread, for afkd's mid-run watch.
    fn comments(&self, unit: &Self::Unit) -> Result<Vec<IssueComment>, GiteaError>;

    /// The attempt's verdict. Only reached when [`CALLS`](Self::CALLS) lists `classify`;
    /// otherwise afkd's own verdict stands, which is this default.
    fn classify(_scratch: &Path, verdict: UnitOutcome) -> UnitOutcome {
        verdict
    }

    /// The plugin-owned end of a unit's run: the claim marker's release, and whatever
    /// else the kind itself owes the outcome. Returns whether the park landed or was
    /// queued — always `true` for an outcome that is not a park.
    fn finish(
        &self,
        unit: &Self::Unit,
        outcome: UnitOutcome,
        facts: &Facts,
        now: Instant,
        diag: &dyn Diag,
    ) -> bool;

    /// The forge client the kind's calls go through.
    fn client(&self) -> &dyn GiteaClient;

    /// The writes this kind owes the forge, for the worker that delivers them.
    fn outbox(&self) -> &Arc<Outbox>;

    /// Do `write` on the unit's item as the login it was claimed as, or queue it (see
    /// [`Outbox::deliver`]): an action a slot called, or one of the kind's own run-end
    /// writes.
    fn deliver(
        &self,
        unit: &Self::Unit,
        write: Write,
        now: Instant,
        diag: &dyn Diag,
    ) -> Result<Delivery, GiteaError> {
        self.outbox()
            .deliver(self.client(), &unit.item(), write, now, diag)
    }
}
