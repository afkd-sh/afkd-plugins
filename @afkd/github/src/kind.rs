//! The seam a kind sits behind: what [`crate::plugin`] drives per call, and what it reads
//! off a claimed unit. The wire's bookkeeping — the identity, the live units, the 64 KiB
//! fitting, the `comments` delta, the `held` answer — is written once in `plugin.rs`
//! against these two traits, so a kind differs only in its vendor half
//! ([`crate::issue`]).

use crate::client::{GithubError, IssueComment};
use crate::common::{Clock, Diag};
use crate::wire::{Facts, UnitOutcome, WireUnit};

/// A claimed unit, as the plugin's bookkeeping reads it.
pub(crate) trait ClaimedUnit {
    /// The unit's stable coordinate, `<owner>/<name>#<number>` — the wire unit's `thread`.
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

    /// The terminal lifecycle. Returns whether the moment reached the remote.
    fn finish(
        &self,
        unit: &Self::Unit,
        outcome: UnitOutcome,
        facts: &Facts,
        diag: &dyn Diag,
    ) -> bool;
}
