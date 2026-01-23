#[cfg(test)]
mod test_construction {
    //! Integration test ensuring that a venue:
    //! - can be constructed from on-chain account data,
    //! - can load its required state via the AccountsCache,
    //! - returns valid token info,
    //! - supports quoting for both swap directions,
    //! - and exposes sane quoting boundaries.
    //!
    //! Any AMM implementer integrating with Titan should ensure their venue
    //! passes this style of test, as it verifies the critical invariants that
    //! Titan relies on for routing.

    use std::{env, str::FromStr};

    use rstest::rstest;
    use solana_client::nonblocking::rpc_client::RpcClient;
    use solana_pubkey::Pubkey;
    use titan_integration_template::account_caching::rpc_cache::RpcClientCache;
    use titan_integration_template::trading_venue::{QuoteRequest, SwapType};
    use titan_integration_template::{
        reflect_whitelabel::ReflectWhitelabelVenue,
        trading_venue::{FromAccount, TradingVenue},
    };

    use assert_no_alloc::*;

    #[cfg(debug_assertions)] // required when disable_release is set (default)
    #[global_allocator]
    static A: AllocDisabler = AllocDisabler;

    fn init_test_logger() {
        let _ = dotenvy::dotenv();
        let _ = env_logger::builder().is_test(true).try_init();
    }

    #[rstest]
    #[tokio::test]
    #[case("9GKYXhPf7XF2yVHRwzVWpeLxRJszp4Jf7zF19hbfE1Ah")]
    async fn test_construction(#[case] proxy_state_key: String) {
        init_test_logger();

        let proxy_state_key = Pubkey::from_str(&proxy_state_key).expect("Invalid test pubkey");
        let rpc_url = env::var("SOLANA_RPC_URL").expect("SOLANA_RPC_URL must be set");
        let rpc = RpcClient::new(rpc_url);

        let venue_account = rpc.get_account(&proxy_state_key).await.unwrap();
        let mut venue = ReflectWhitelabelVenue::from_account(&proxy_state_key, &venue_account).unwrap();

        let cache = RpcClientCache::new(rpc);
        venue.update_state(&cache).await.unwrap();

        let token_info = venue.get_token_info();
        log::info!("Loaded token info: {:#?}", token_info);
        assert!(token_info.len() > 0);
        assert_eq!(token_info.len(), 2);

        for (input_idx, output_idx) in [(0, 1), (1, 0)] {
            log::info!("Checking bounds for pair ({}, {})", input_idx, output_idx);

            let (lower_bound, upper_bound) =
                assert_no_alloc(|| venue.bounds(input_idx, output_idx))
                    .expect("Boundary search failed");

            assert!(
                lower_bound < upper_bound,
                "Lower bound must be strictly less than upper bound"
            );

            let input_mint = token_info[input_idx as usize].pubkey;
            let output_mint = token_info[output_idx as usize].pubkey;

            let lb_result = assert_no_alloc(|| {
                venue.quote(QuoteRequest {
                    input_mint,
                    output_mint,
                    amount: lower_bound,
                    swap_type: SwapType::ExactIn,
                })
            })
            .expect("Lower-bound quote failed");

            log::info!("Lower-bound quote: {:#?}", lb_result);

            assert!(
                !lb_result.not_enough_liquidity,
                "Lower bound indicates insufficient liquidity"
            );
            assert!(
                lb_result.expected_output > 0,
                "Lower bound produced zero output"
            );

            let ub_result = assert_no_alloc(|| {
                venue.quote(QuoteRequest {
                    input_mint,
                    output_mint,
                    amount: upper_bound,
                    swap_type: SwapType::ExactIn,
                })
            })
            .expect("Upper-bound quote failed");

            log::info!("Upper-bound quote: {:#?}", ub_result);

            assert!(
                !ub_result.not_enough_liquidity,
                "Upper bound indicates insufficient liquidity"
            );
            assert!(
                ub_result.expected_output > 0,
                "Upper bound produced zero output"
            );
        }
    }
}
