//! Serialization of the Monero lock phase across concurrent swaps.
//!
//! wallet2 only marks an output spent once the transaction spending it is in a
//! block, and monero-sys has no reserve API, so two overlapping swaps can pick
//! the same output and the loser's lock transaction becomes a permanent
//! double-spend that monerod rejects forever. [`MoneroLockPhase`] hands out one
//! process-wide permit for the phase; each swap tracks its participation
//! through a [`LockPhaseSession`] so the state machine only has to say where
//! its current state sits in the phase ([`PhaseStep`]).

use std::time::{Duration, Instant};

use tokio::sync::{Mutex, MutexGuard};

/// Longest a swap may hold the permit per acquisition. Past it, a swap that already built or
/// relayed its lock gives the permit up, so a publish wedged on a rejected lock cannot starve
/// the other swaps; a swap still selecting outputs releases it and queues again (see
/// [`LockPhaseSession::hold_expired`]).
pub const MONERO_LOCK_PHASE_MAX_HOLD: Duration = Duration::from_secs(20 * 60);

/// Hard cap on building the lock in `XmrReadyToLock`, which runs inside the phase. 4.15.0
/// retries the construction for `monero_lock_retry_timeout` (30 min on mainnet), longer than
/// a hold, and that budget only stops new attempts: the last one runs on, and one attempt can
/// wait up to a cooldown for its construction turn and then build for another cooldown.
pub const LOCK_CONSTRUCTION_BUDGET: Duration = Duration::from_secs(10 * 60);

/// Hard cap on the shared-wallet scan that follows a failed construction, also inside the
/// phase. It decides between an early refund (the shared wallet is provably empty) and
/// waiting for the cancel timelock (it is not, or the scan did not finish in time).
pub const LOCK_FAILURE_SCAN_BUDGET: Duration = Duration::from_secs(5 * 60);

/// Room the caps must leave in a hold for the rest of an `XmrReadyToLock` step (the Bitcoin
/// subscription it starts with).
const LOCK_SELECTION_SLACK: Duration = Duration::from_secs(60);

// A failing `XmrReadyToLock` step runs the construction and then the scan under one hold. If
// their caps did not fit in it, the hold would cut the step, the swap would queue again with
// fresh budgets, and it would never reach its early refund.
const _: () = assert!(
    LOCK_CONSTRUCTION_BUDGET.as_secs()
        + LOCK_FAILURE_SCAN_BUDGET.as_secs()
        + LOCK_SELECTION_SLACK.as_secs()
        <= MONERO_LOCK_PHASE_MAX_HOLD.as_secs()
);

/// Where a swap's current state sits relative to the serialized phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhaseStep {
    /// Outside the phase.
    Outside,
    /// Selecting outputs and building the lock (`XmrReadyToLock`). Nothing has been built or
    /// published yet, so the swap can always release the permit and queue for it again.
    Selecting,
    /// The lock was built (`XmrLockTransactionConstructed`) or relayed
    /// (`XmrLockTransactionSent`): its inputs stay committed until it confirms.
    Committed,
}

/// What a session did when its hold ran out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldExpired {
    /// Released the permit and stayed armed: the next [`LockPhaseSession::sync_to`] queues
    /// for it again, behind the swaps already waiting, and starts a fresh hold.
    Requeued,
    /// Gave the permit up: the swap continues unserialized until it leaves the phase or
    /// returns to [`PhaseStep::Selecting`].
    Abandoned,
}

/// Process-wide serialization of the Monero lock phase.
///
/// The phase spans output selection (`XmrReadyToLock`) through the lock
/// transaction's first confirmation (`XmrLockTransactionSent`).
pub struct MoneroLockPhase {
    mutex: Mutex<()>,
    max_hold: Duration,
}

impl MoneroLockPhase {
    /// A lock phase whose sessions may hold the permit for at most `max_hold` per acquisition.
    pub fn new(max_hold: Duration) -> Self {
        Self {
            mutex: Mutex::new(()),
            max_hold,
        }
    }

    /// Start tracking one swap's participation in the phase.
    pub fn session(&self) -> LockPhaseSession<'_> {
        LockPhaseSession {
            phase: self,
            held: None,
            abandoned: false,
        }
    }
}

/// One swap's view of the serialized phase, held on `run_until`'s stack because
/// construct, publish and confirm are separate states with a persist in between.
pub struct LockPhaseSession<'a> {
    phase: &'a MoneroLockPhase,
    held: Option<(MutexGuard<'a, ()>, Instant)>,
    /// Set once this swap overstays its hold with a built or relayed lock and continues
    /// unserialized.
    abandoned: bool,
}

impl LockPhaseSession<'_> {
    /// Bring the session in line with where the swap's current state sits: queue for the
    /// process-wide permit inside the phase (FIFO), release it outside. A swap that gave the
    /// permit up with a built or relayed lock does not queue again for that lock, but it does
    /// before it selects outputs again: a rebuild returns to `XmrReadyToLock`.
    pub async fn sync_to(&mut self, step: PhaseStep) {
        match step {
            PhaseStep::Outside => self.release(),
            PhaseStep::Selecting => {
                if self.abandoned {
                    tracing::info!(
                        "Queueing for the serialized Monero lock phase again before rebuilding the lock"
                    );
                    self.abandoned = false;
                }
                self.acquire().await;
            }
            PhaseStep::Committed => {
                if !self.abandoned {
                    self.acquire().await;
                }
            }
        }
    }

    async fn acquire(&mut self) {
        if self.held.is_none() {
            let guard = self.phase.mutex.lock().await;
            tracing::debug!("Entered the serialized Monero lock phase");
            self.held = Some((guard, Instant::now()));
        }
    }

    /// Remaining time this swap may keep the permit, or `None` when it is not
    /// holding it (never entered, already released, or abandoned).
    pub fn deadline(&self) -> Option<Duration> {
        self.held
            .as_ref()
            .map(|(_, since)| self.phase.max_hold.saturating_sub(since.elapsed()))
    }

    /// Whether this session currently holds the process-wide permit.
    pub fn holds_permit(&self) -> bool {
        self.held.is_some()
    }

    /// Leave the phase normally: release the permit and re-arm the session for
    /// a future phase.
    pub fn release(&mut self) {
        if self.held.take().is_some() {
            tracing::debug!("Left the serialized Monero lock phase");
        }
        self.abandoned = false;
    }

    /// Let go of the permit after the hold ran out while the swap was at `step`.
    ///
    /// While [`PhaseStep::Selecting`] nothing is built or published, so the swap releases
    /// the permit and queues again ([`HoldExpired::Requeued`]); giving it up there would let
    /// the swap build and publish its lock unserialized while the next swap holds the permit.
    /// With a built or relayed lock ([`PhaseStep::Committed`]) the swap gives the permit up
    /// and continues unserialized ([`HoldExpired::Abandoned`]). Outside the phase there is
    /// nothing to protect and the permit is released.
    pub fn hold_expired(&mut self, step: PhaseStep) -> HoldExpired {
        match step {
            PhaseStep::Committed => {
                self.held = None;
                self.abandoned = true;
                HoldExpired::Abandoned
            }
            PhaseStep::Selecting | PhaseStep::Outside => {
                self.release();
                HoldExpired::Requeued
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WAIT: Duration = Duration::from_millis(50);

    #[tokio::test]
    async fn phase_is_exclusive_until_released() {
        let phase = MoneroLockPhase::new(Duration::from_secs(60));

        let mut first = phase.session();
        first.sync_to(PhaseStep::Selecting).await;
        assert!(first.holds_permit());

        let mut second = phase.session();
        assert!(
            tokio::time::timeout(WAIT, second.sync_to(PhaseStep::Selecting))
                .await
                .is_err(),
            "second session must wait while the first holds the permit"
        );
        assert!(!second.holds_permit());

        first.release();
        assert!(!first.holds_permit());

        tokio::time::timeout(WAIT, second.sync_to(PhaseStep::Selecting))
            .await
            .expect("second session acquires once the first released");
        assert!(second.holds_permit());
    }

    #[tokio::test]
    async fn one_permit_covers_selection_through_confirmation() {
        let phase = MoneroLockPhase::new(Duration::from_secs(60));
        let mut session = phase.session();

        session.sync_to(PhaseStep::Selecting).await;
        let selecting = session.deadline().expect("held sessions have a deadline");
        tokio::time::sleep(WAIT).await;

        // Built, then relayed: same permit, same hold.
        session.sync_to(PhaseStep::Committed).await;
        session.sync_to(PhaseStep::Committed).await;
        assert!(session.holds_permit());
        assert!(session.deadline().expect("still held") < selecting);

        session.sync_to(PhaseStep::Outside).await;
        assert!(!session.holds_permit());
    }

    #[tokio::test]
    async fn deadline_is_bounded_by_max_hold_and_none_when_not_held() {
        let phase = MoneroLockPhase::new(Duration::from_secs(60));
        let mut session = phase.session();

        assert_eq!(session.deadline(), None);

        session.sync_to(PhaseStep::Selecting).await;
        let deadline = session.deadline().expect("held sessions have a deadline");
        assert!(deadline <= Duration::from_secs(60));

        session.sync_to(PhaseStep::Outside).await;
        assert_eq!(session.deadline(), None);
    }

    #[tokio::test]
    async fn deadline_expires_to_zero() {
        let phase = MoneroLockPhase::new(Duration::from_millis(5));
        let mut session = phase.session();

        session.sync_to(PhaseStep::Selecting).await;
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(session.deadline(), Some(Duration::ZERO));
    }

    /// A hold running out in `XmrReadyToLock` must not leave the swap building its lock
    /// unserialized: it releases the permit, the swap that was waiting goes first, then the
    /// expired one gets the permit back.
    #[tokio::test]
    async fn hold_expiring_while_selecting_requeues_behind_waiting_swaps() {
        let phase = MoneroLockPhase::new(Duration::from_secs(60));

        let mut expired = phase.session();
        expired.sync_to(PhaseStep::Selecting).await;

        let mut waiting = phase.session();
        {
            let queued = waiting.sync_to(PhaseStep::Selecting);
            tokio::pin!(queued);
            assert!(
                tokio::time::timeout(WAIT, queued.as_mut()).await.is_err(),
                "the second swap queues while the first holds the permit"
            );

            assert_eq!(
                expired.hold_expired(PhaseStep::Selecting),
                HoldExpired::Requeued
            );
            assert!(!expired.holds_permit());
            assert_eq!(expired.deadline(), None);

            assert!(
                tokio::time::timeout(WAIT, expired.sync_to(PhaseStep::Selecting))
                    .await
                    .is_err(),
                "a requeued swap waits behind the swaps already queued, it never continues \
                 without the permit"
            );
            assert!(!expired.holds_permit());

            tokio::time::timeout(WAIT, queued)
                .await
                .expect("the swap that was waiting gets the permit first");
        }
        assert!(waiting.holds_permit());

        waiting.sync_to(PhaseStep::Outside).await;
        tokio::time::timeout(WAIT, expired.sync_to(PhaseStep::Selecting))
            .await
            .expect("the requeued swap gets the permit back once it is free");
        assert!(expired.holds_permit());
    }

    #[tokio::test]
    async fn requeued_swap_gets_a_fresh_hold() {
        let phase = MoneroLockPhase::new(Duration::from_millis(200));
        let mut session = phase.session();

        session.sync_to(PhaseStep::Selecting).await;
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(session.deadline(), Some(Duration::ZERO));

        assert_eq!(
            session.hold_expired(PhaseStep::Selecting),
            HoldExpired::Requeued
        );
        tokio::time::timeout(WAIT, session.sync_to(PhaseStep::Selecting))
            .await
            .expect("nobody else waits, the permit is free");
        assert!(
            session.deadline().expect("held again") > Duration::ZERO,
            "a requeued swap starts a new hold instead of timing out again at once"
        );
    }

    /// A swap that already built or relayed its lock still gives the permit up, as before.
    #[tokio::test]
    async fn hold_expiring_with_a_built_or_relayed_lock_abandons_the_phase() {
        let phase = MoneroLockPhase::new(Duration::from_secs(60));

        let mut wedged = phase.session();
        wedged.sync_to(PhaseStep::Selecting).await;
        wedged.sync_to(PhaseStep::Committed).await;
        assert_eq!(
            wedged.hold_expired(PhaseStep::Committed),
            HoldExpired::Abandoned
        );
        assert!(!wedged.holds_permit());
        assert_eq!(wedged.deadline(), None);

        // Still publishing or confirming: the session must not re-acquire...
        tokio::time::timeout(WAIT, wedged.sync_to(PhaseStep::Committed))
            .await
            .expect("an abandoned session never blocks");
        assert!(!wedged.holds_permit());

        // ...so another swap is free to take the permit meanwhile.
        let mut other = phase.session();
        tokio::time::timeout(WAIT, other.sync_to(PhaseStep::Selecting))
            .await
            .expect("the permit is free after an abandon");
        assert!(other.holds_permit());
        other.release();

        // Leaving the phase re-arms the abandoned session for the next one.
        wedged.sync_to(PhaseStep::Outside).await;
        tokio::time::timeout(WAIT, wedged.sync_to(PhaseStep::Selecting))
            .await
            .expect("a re-armed session acquires again");
        assert!(wedged.holds_permit());
    }

    /// With a trusted daemon, a publish that gave the permit up can return to
    /// `XmrReadyToLock` to rebuild the lock: it must queue before selecting outputs again.
    #[tokio::test]
    async fn abandoned_swap_queues_again_before_rebuilding_its_lock() {
        let phase = MoneroLockPhase::new(Duration::from_secs(60));

        let mut rebuilding = phase.session();
        rebuilding.sync_to(PhaseStep::Committed).await;
        assert_eq!(
            rebuilding.hold_expired(PhaseStep::Committed),
            HoldExpired::Abandoned
        );

        let mut other = phase.session();
        other.sync_to(PhaseStep::Selecting).await;

        assert!(
            tokio::time::timeout(WAIT, rebuilding.sync_to(PhaseStep::Selecting))
                .await
                .is_err(),
            "a rebuild waits for the permit like any other selection"
        );
        assert!(!rebuilding.holds_permit());

        other.release();
        tokio::time::timeout(WAIT, rebuilding.sync_to(PhaseStep::Selecting))
            .await
            .expect("the rebuild acquires once the permit is free");
        assert!(rebuilding.holds_permit());
    }

    #[tokio::test]
    async fn dropping_a_session_frees_the_permit() {
        let phase = MoneroLockPhase::new(Duration::from_secs(60));

        {
            let mut held = phase.session();
            held.sync_to(PhaseStep::Selecting).await;
            assert!(held.holds_permit());
        }

        let mut next = phase.session();
        tokio::time::timeout(WAIT, next.sync_to(PhaseStep::Selecting))
            .await
            .expect("dropping a holding session frees the permit");
        assert!(next.holds_permit());
    }

    /// The `XmrReadyToLock` step runs the capped construction and then the capped scan under
    /// one hold, so a failing step always reaches its early refund (or the wait for the cancel
    /// timelock) instead of being cut and requeued with fresh budgets forever.
    #[test]
    fn in_phase_budgets_fit_in_one_hold() {
        assert!(
            LOCK_CONSTRUCTION_BUDGET + LOCK_FAILURE_SCAN_BUDGET + LOCK_SELECTION_SLACK
                <= MONERO_LOCK_PHASE_MAX_HOLD,
            "a failing XmrReadyToLock step must end inside one hold"
        );
        // A lock built at the very end of the construction budget still has half the hold to
        // be published and confirmed before the swap would continue unserialized.
        assert!(LOCK_CONSTRUCTION_BUDGET * 2 <= MONERO_LOCK_PHASE_MAX_HOLD);
    }
}
