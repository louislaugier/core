pub mod harness;

use harness::SlowCancelConfig;
use swap::asb::FixedRate;
use swap::protocol::alice::AliceState;
use swap::protocol::bob::BobState;
use swap::protocol::{alice, bob};

/// Fork (05/10/2026): two swaps run at once and the maker holds three 1-XMR outputs, so both
/// can be funded, but not at once. Each swap needs ~1.02 XMR: the first lock spends two
/// outputs and leaves its ~0.98 XMR change in the 10-block lock. The second swap then sees
/// 1 XMR unlocked for ~1.02 needed while the total (~1.98 XMR) covers it. It must wait for that
/// change to unlock and then lock its Monero, not refund the Bitcoin early: both swaps redeem.
#[tokio::test]
async fn concurrent_bobs_wait_for_locked_change() {
    harness::setup_test_funded(SlowCancelConfig, None, None, 3, |mut ctx| async move {
        let (bob_swap_1, bob_join_handle_1) = ctx.bob_swap().await;
        let bob_swap_1 = tokio::spawn(bob::run(bob_swap_1));
        let alice_swap_1 = ctx.alice_next_swap().await;
        let alice_swap_1 = tokio::spawn(alice::run(alice_swap_1, FixedRate::default()));

        let (bob_swap_2, bob_join_handle_2) = ctx.bob_swap().await;
        let bob_swap_2 = tokio::spawn(bob::run(bob_swap_2));
        let alice_swap_2 = ctx.alice_next_swap().await;
        let alice_swap_2 = tokio::spawn(alice::run(alice_swap_2, FixedRate::default()));

        let bob_state_1 = bob_swap_1.await??;
        let bob_state_2 = bob_swap_2.await??;
        let alice_state_1 = alice_swap_1.await??;
        let alice_state_2 = alice_swap_2.await??;

        bob_join_handle_1.abort();
        bob_join_handle_2.abort();

        for alice_state in [&alice_state_1, &alice_state_2] {
            assert!(
                matches!(alice_state, AliceState::BtcRedeemed),
                "both maker swaps must lock and redeem (the second after our change unlocked); \
                 got {alice_state_1} / {alice_state_2}"
            );
        }
        for bob_state in [&bob_state_1, &bob_state_2] {
            assert!(
                matches!(bob_state, BobState::XmrRedeemed { .. }),
                "both takers must redeem their Monero; got {bob_state_1} / {bob_state_2}"
            );
        }

        Ok(())
    })
    .await;
}
