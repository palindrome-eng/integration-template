#[cfg(test)]
mod simulations {
    //! Quoting tests for Titan-compatible AMM venues.
    //!
    //! The tests ensure:
    //! - The venue loads on-chain state correctly
    //! - It exposes valid token info
    //! - It establishes valid quoting boundaries for both swap directions
    //! - Its off-chain quote matches on-chain execution on and off the boundaries
    //! - Its quoting speed is sufficient for integration
    //!
    //! Any AMM integrator must pass these quoting tests to ensure their pool
    //! is safe, consistent, and suitable for Titan routing.

    use litesvm::LiteSVM;
    use rand::Rng;
    use rstest::rstest;

    use solana_account::Account;
    use solana_account::WritableAccount;
    use solana_client::nonblocking::rpc_client::RpcClient;
    use solana_compute_budget::compute_budget::ComputeBudget;
    use solana_program::native_token::LAMPORTS_PER_SOL;
    use solana_program_pack::Pack;
    use solana_pubkey::Pubkey;
    use solana_sdk::signature::Keypair;
    use solana_sdk::signer::Signer;
    use solana_sysvar::clock::{self, Clock};
    use solana_transaction::Transaction;
    use std::str::FromStr;
    use std::time::Instant;

    use spl_associated_token_account::get_associated_token_address_with_program_id;
    use spl_token::state::{Account as TokenAccount, AccountState};

    use std::env;

    use titan_integration_template::reflect_whitelabel::REFLECT_PROXY_PROGRAM_ID;
    use titan_integration_template::trading_venue::SwapType;

    use titan_integration_template::{
        account_caching::AccountsCache, reflect_whitelabel::ReflectWhitelabelVenue, trading_venue::QuoteRequest,
    };
    use titan_integration_template::{
        account_caching::rpc_cache::RpcClientCache,
        trading_venue::{FromAccount, TradingVenue, error::TradingVenueError},
    };

    fn init_test_logger() {
        let _ = dotenvy::dotenv();
        let _ = env_logger::builder().is_test(false).try_init();
    }

    pub fn setup_litesvm() -> (LiteSVM, Keypair) {
        let mut litesvm = LiteSVM::new().with_compute_budget(ComputeBudget {
            compute_unit_limit: 1_400_000,
            ..Default::default()
        });

        let keypair = Keypair::new();
        let account = Account {
            lamports: 10_000 * LAMPORTS_PER_SOL,
            data: vec![],
            owner: solana_sdk::system_program::id(),
            executable: false,
            rent_epoch: 0,
        };
        litesvm
            .set_account(keypair.pubkey(), account.into())
            .unwrap();

        (litesvm, keypair)
    }

    async fn sim_quote_request(
        venue: &dyn TradingVenue,
        cache: &dyn AccountsCache,
        request: QuoteRequest,
        litesvm: &mut LiteSVM,
        keypair: &Keypair,
    ) -> u64 {
        let tradable_mints = venue.get_token_info();

        let idx_0 = tradable_mints
            .iter()
            .position(|x| x.pubkey == request.input_mint)
            .unwrap();
        let idx_1 = (idx_0 + 1) % 2;

        let (token_a, token_a_program) = (
            tradable_mints[idx_0].pubkey,
            tradable_mints[idx_0].get_token_program(),
        );
        let (token_b, token_b_program) = (
            tradable_mints[idx_1].pubkey,
            tradable_mints[idx_1].get_token_program(),
        );

        let token_account_a = get_associated_token_address_with_program_id(
            &keypair.pubkey(),
            &token_a,
            &token_a_program,
        );
        let token_account_b = get_associated_token_address_with_program_id(
            &keypair.pubkey(),
            &token_b,
            &token_b_program,
        );

        let mut account_a = Account::new(LAMPORTS_PER_SOL, TokenAccount::LEN, &spl_token::ID);
        let mut account_a_data = TokenAccount::default();
        account_a_data.mint = token_a;
        account_a_data.owner = keypair.pubkey();
        account_a_data.state = AccountState::Initialized;
        account_a_data.amount = u64::MAX;
        account_a_data.pack_into_slice(account_a.data_as_mut_slice());

        let mut account_b = Account::new(LAMPORTS_PER_SOL, TokenAccount::LEN, &spl_token::ID);
        let mut account_b_data = TokenAccount::default();
        account_b_data.mint = token_b;
        account_b_data.owner = keypair.pubkey();
        account_b_data.state = AccountState::Initialized;
        account_b_data.amount = 0;
        account_b_data.pack_into_slice(account_b.data_as_mut_slice());

        litesvm.set_account(token_account_a, account_a).unwrap();
        litesvm.set_account(token_account_b, account_b).unwrap();

        let ix = venue
            .generate_swap_instruction(request, keypair.pubkey())
            .unwrap();

        let pks: Vec<Pubkey> = ix.accounts.iter().map(|acc| acc.pubkey).collect();
        let accounts_to_load = cache.get_accounts(&pks).await.unwrap();
        for (account, key) in accounts_to_load.iter().zip(pks) {
            if let Some(acc) = account {
                if acc.executable {
                    continue;
                }
                litesvm.set_account(key, acc.clone()).unwrap();
            }
        }

        let blockhash = litesvm.latest_blockhash();
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&keypair.pubkey()),
            &[keypair],
            blockhash,
        );

        log::debug!(
            "Sending transaction with {} instructions and with blockhash: {}",
            tx.message.instructions.len(),
            blockhash
        );

        litesvm.send_transaction(tx).unwrap();

        let account_b = litesvm.get_account(&token_account_b).unwrap();
        let post_b = TokenAccount::unpack_from_slice(&account_b.data)
            .expect("Failed to unpack token B account");
        post_b.amount
    }

    /// Returns a log-uniformly sampled u64 in `[lo, hi]`.
    fn sample_log_uniform_u64(lo: u64, hi: u64) -> u64 {
        assert!(lo >= 1, "log-uniform sampling requires lo >= 1");
        assert!(lo <= hi);

        let lo_f = lo as f64;
        let hi_f = hi as f64;

        let log_lo = lo_f.ln();
        let log_hi = hi_f.ln();

        let r: f64 = rand::rng().random();
        let log_val = log_lo + r * (log_hi - log_lo);

        (log_val.exp() as u64).clamp(lo, hi)
    }

    #[rstest]
    #[tokio::test]
    #[case("9GKYXhPf7XF2yVHRwzVWpeLxRJszp4Jf7zF19hbfE1Ah")]
    async fn test_bound_simulation(#[case] proxy_state_key: Pubkey) {
        init_test_logger();

        let rpc_url = env::var("SOLANA_RPC_URL").unwrap();
        let rpc = RpcClient::new(rpc_url);
        let venue_account = rpc.get_account(&proxy_state_key).await.unwrap();

        let cache = RpcClientCache::new(rpc);
        let mut venue = ReflectWhitelabelVenue::from_account(&proxy_state_key, &venue_account).unwrap();
        venue.update_state(&cache).await.unwrap();

        let (mut litesvm, keypair) = setup_litesvm();

        litesvm
            .add_program_from_file(
                REFLECT_PROXY_PROGRAM_ID,
                "reflect-proxy-program/target/deploy/reflect_companion_program.so",
            )
            .unwrap();

        let latest_clock = cache.get_account(&clock::ID).await.unwrap();
        let latest_clock: Clock = latest_clock
            .as_ref()
            .ok_or(TradingVenueError::NoAccountFound(clock::ID.into()))
            .unwrap()
            .deserialize_data()
            .unwrap();

        litesvm.set_sysvar::<Clock>(&latest_clock);

        let tradable_mints = venue.get_token_info();
        assert_eq!(tradable_mints.len(), 2);

        for (in_idx, out_idx) in [(0, 1), (1, 0)] {
            let (lower, upper) = venue.bounds(in_idx as u8, out_idx as u8).unwrap();

            for bound in [lower, upper] {
                let request = QuoteRequest {
                    input_mint: venue.get_token(in_idx).unwrap().pubkey,
                    output_mint: venue.get_token(out_idx).unwrap().pubkey,
                    amount: bound,
                    swap_type: SwapType::ExactIn,
                };

                let sim =
                    sim_quote_request(&venue, &cache, request.clone(), &mut litesvm, &keypair)
                        .await;
                let quote = venue.quote(request).unwrap();

                log::debug!(
                    "Boundary = {}\nSimulated = {}\nOff-chain quote = {}\nDelta = {}",
                    bound,
                    sim,
                    quote.expected_output,
                    quote.expected_output.abs_diff(sim)
                );

                assert_eq!(quote.expected_output.abs_diff(sim), 0)
            }
        }
    }

    #[rstest]
    #[tokio::test]
    #[case("9GKYXhPf7XF2yVHRwzVWpeLxRJszp4Jf7zF19hbfE1Ah")]
    async fn test_random_samples(#[case] proxy_state_key: Pubkey) {
        init_test_logger();

        let rpc_url = env::var("SOLANA_RPC_URL").unwrap();
        let rpc = RpcClient::new(rpc_url);
        let venue_account = rpc.get_account(&proxy_state_key).await.unwrap();

        let cache = RpcClientCache::new(rpc);
        let mut venue = ReflectWhitelabelVenue::from_account(&proxy_state_key, &venue_account).unwrap();
        venue.update_state(&cache).await.unwrap();

        let (mut litesvm, keypair) = setup_litesvm();
        litesvm
            .add_program_from_file(
                REFLECT_PROXY_PROGRAM_ID,
                "reflect-proxy-program/target/deploy/reflect_companion_program.so",
            )
            .unwrap();

        let latest_clock = cache.get_account(&clock::ID).await.unwrap();
        let latest_clock: Clock = latest_clock
            .as_ref()
            .ok_or(TradingVenueError::NoAccountFound(clock::ID.into()))
            .unwrap()
            .deserialize_data()
            .unwrap();
        litesvm.set_sysvar::<Clock>(&latest_clock);

        for (in_idx, out_idx) in [(0, 1), (1, 0)] {
            let (lb, ub) = venue.bounds(in_idx, out_idx).unwrap();

            for _ in 0..50 {
                let amount = sample_log_uniform_u64(lb, ub);

                let request = QuoteRequest {
                    input_mint: venue.get_token(in_idx as usize).unwrap().pubkey,
                    output_mint: venue.get_token(out_idx as usize).unwrap().pubkey,
                    amount,
                    swap_type: SwapType::ExactIn,
                };

                let sim =
                    sim_quote_request(&venue, &cache, request.clone(), &mut litesvm, &keypair)
                        .await;
                let quote = venue.quote(request).unwrap();

                log::debug!(
                    "Random sim: {}\nQuote: {}\nDelta: {}",
                    sim,
                    quote.expected_output,
                    quote.expected_output.abs_diff(sim)
                );

                assert_eq!(quote.expected_output.abs_diff(sim), 0)
            }
        }
    }

    #[rstest]
    #[tokio::test]
    #[case("9GKYXhPf7XF2yVHRwzVWpeLxRJszp4Jf7zF19hbfE1Ah")]
    async fn test_monotone(#[case] proxy_state_key: String) -> () {
        init_test_logger();

        let proxy_state_key = Pubkey::from_str(&proxy_state_key).expect("Invalid test pubkey");
        let rpc_url = env::var("SOLANA_RPC_URL").expect("SOLANA_RPC_URL must be set");
        let rpc = RpcClient::new(rpc_url);

        let venue_account = rpc.get_account(&proxy_state_key).await.unwrap();
        let mut venue = ReflectWhitelabelVenue::from_account(&proxy_state_key, &venue_account).unwrap();

        let cache = RpcClientCache::new(rpc);
        venue.update_state(&cache).await.unwrap();

        let token_info = venue.get_token_info();
        log::debug!("Loaded token info: {:#?}", token_info);
        assert_eq!(token_info.len(), 2);

        for (in_idx, out_idx) in [(0, 1), (1, 0)] {
            let (lb, ub) = venue.bounds(in_idx, out_idx).unwrap();
            let mut test_amounts = Vec::with_capacity(50);

            for _ in 0..50 {
                test_amounts.push(sample_log_uniform_u64(lb, ub));
            }
            test_amounts.sort();

            let mut prev = 0;
            for amount in test_amounts {
                let input_mint = token_info[in_idx as usize].pubkey;
                let output_mint = token_info[out_idx as usize].pubkey;

                let result = venue
                    .quote(QuoteRequest {
                        input_mint,
                        output_mint,
                        amount: amount,
                        swap_type: SwapType::ExactIn,
                    })
                    .expect("Lower-bound quote failed");

                log::debug!("quote: {:#?}", result);

                assert!(
                    prev <= result.expected_output,
                    "Swap function is not monotone (prev: {}) > (output: {})",
                    prev,
                    result.expected_output
                );

                prev = result.expected_output;
            }
        }
    }

    #[rstest]
    #[tokio::test]
    #[case("9GKYXhPf7XF2yVHRwzVWpeLxRJszp4Jf7zF19hbfE1Ah", 10_000)]
    async fn test_quoting_speed(#[case] proxy_state_key: String, #[case] iterations: usize) -> () {
        init_test_logger();

        let proxy_state_key = Pubkey::from_str(&proxy_state_key).expect("Invalid test pubkey");
        let rpc_url = env::var("SOLANA_RPC_URL").expect("SOLANA_RPC_URL must be set");
        let rpc = RpcClient::new(rpc_url);

        let venue_account = rpc.get_account(&proxy_state_key).await.unwrap();
        let mut venue = ReflectWhitelabelVenue::from_account(&proxy_state_key, &venue_account).unwrap();

        let cache = RpcClientCache::new(rpc);
        venue.update_state(&cache).await.unwrap();

        let token_info = venue.get_token_info();
        log::debug!("Loaded token info: {:#?}", token_info);
        assert_eq!(token_info.len(), 2);

        for (in_idx, out_idx) in [(0, 1), (1, 0)] {
            let input_mint = token_info[in_idx as usize].pubkey;
            let output_mint = token_info[out_idx as usize].pubkey;

            let (lb, ub) = venue.bounds(in_idx, out_idx).unwrap();
            let mut test_amounts = Vec::with_capacity(iterations);

            for _ in 0..iterations {
                test_amounts.push(sample_log_uniform_u64(lb, ub));
            }

            let start = Instant::now();
            for amount in test_amounts {
                let result = venue
                    .quote(QuoteRequest {
                        input_mint,
                        output_mint,
                        amount: amount,
                        swap_type: SwapType::ExactIn,
                    })
                    .expect("Lower-bound quote failed");

                log::debug!("quote: {:#?}", result);
            }
            let elapsed = start.elapsed().as_secs_f64();
            let avg_time = elapsed / iterations as f64;

            log::info!("Average quoting speed: {}", avg_time);

            assert!(
                avg_time < 0.0001,
                "Failed quoting speed test swapping ({}) -> ({})",
                input_mint,
                output_mint
            );
        }
    }
}
