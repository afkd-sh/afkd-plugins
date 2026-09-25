//! The seam both kinds sit behind: what [`crate::plugin`] drives per call, and what it
//! reads off a claimed unit. The wire's bookkeeping — the live units, the 64 KiB fitting,
//! the `comments` delta, the undelivered-finish fallback — is written once in
//! `plugin.rs` against these two traits, so the `gitea` and `gitea_pr_review` kinds
//! differ only in their vendor half ([`crate::issue`], [`crate::pr`]).

use std::path::Path;

use crate::client::{GiteaError, IssueComment};
use crate::common::{ClaimFault, Clock, Diag};
use crate::wire::{Facts, UnitOutcome, WireUnit};

/// A claimed unit, as the plugin's bookkeeping reads it.
pub(crate) trait ClaimedUnit {
    /// The unit's stable coordinate, `<full-name>#<number>` — the wire unit's `thread`.
    fn thread(&self) -> String;

    /// The login the unit was claimed as — the wire unit's `self`.
    fn claimed_as(&self) -> &str;
}

/// One kind's vendor half: the calls `plugin.rs` makes per request.
pub(crate) trait Units {
    /// The unit this kind claims.
    type Unit: ClaimedUnit;

    /// The optional calls this kind answers, exactly as `hello` lists them. A call the
    /// kind does not list is refused before it reaches the kind.
    const CALLS: &'static [&'static str];

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

    /// The terminal lifecycle. Returns whether the moment reached the remote.
    fn finish(
        &self,
        unit: &Self::Unit,
        outcome: UnitOutcome,
        facts: &Facts,
        diag: &dyn Diag,
    ) -> bool;
}
