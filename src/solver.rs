use std::{collections::HashMap, sync::Arc, time::Duration};

use num_bigint::BigUint;
use tokio::{sync::{mpsc, Mutex}, task::JoinHandle};
use tycho_simulation::{
    tycho_common::{
        models::{protocol::ProtocolComponent, token::Token, Chain},
        simulation::protocol_sim::ProtocolSim,
    },
    tycho_core::Bytes,
};

use crate::{
    models::{GasPrice, Order, Route},
    modules::{algorithm::algorithm::Algorithm, gas_price_fetcher::GasPriceFetcher},
};

pub struct Solver<A: Algorithm> {
    algorithm: Arc<Mutex<A>>,
    chain: Chain,
    tycho_url: String,
    tycho_api_key: String,
    protocols: Option<Vec<String>>,
    tokens: HashMap<Bytes, Token>,
    gas_price_fetcher: GasPriceFetcher,
    current_gas_price: Arc<Mutex<Option<GasPrice>>>,
    tvl_threshold: (f64, f64), // (min_tvl, max_tvl) for protocol stream builder
}

impl<A: Algorithm + Send + 'static> Solver<A> {
    pub async fn new(
        max_hops: usize,
        chain: Chain,
        tycho_url: String,
        tycho_api_key: String,
        protocols: Option<Vec<String>>,
        tokens: Option<HashMap<Bytes, Token>>,
        tvl_threshold: (f64, f64),
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        // Load tokens from Tycho if not provided
        let tokens = match tokens {
            Some(provided_tokens) => {
                println!("Using {} provided tokens", provided_tokens.len());
                provided_tokens
            }
            None => {
                println!("Loading tokens from Tycho...");
                let all_tokens = tycho_simulation::utils::load_all_tokens(
                    &tycho_url,
                    false,
                    Some(tycho_api_key.as_str()),
                    true,
                    chain,
                    None,
                    None,
                )
                .await
                .map_err(|e| format!("Failed to load tokens from Tycho: {e:?}"))?;

                println!("Loaded {} tokens from Tycho", all_tokens.len());
                all_tokens
            }
        };

        Ok(Self {
            algorithm: Arc::new(Mutex::new(A::new(max_hops))),
            chain,
            tycho_url,
            tycho_api_key,
            protocols,
            tokens,
            gas_price_fetcher: GasPriceFetcher::new(),
            current_gas_price: Arc::new(Mutex::new(None)),
            tvl_threshold,
        })
    }

    /// Find the best route for an Order (delegates to algorithm)
    pub async fn solve_order(&self, order: &Order) -> Option<Route> {
        // Lock the shared state
        let algorithm = self.algorithm.lock().await;
        let gas_price_guard = self.current_gas_price.lock().await;
        let gas_price_ref = gas_price_guard.as_ref();

        // First do a 1 ETH -> token out order to get the token price
        let native_token = self.chain.native_token();
        let native_order = Order::new(
            "".to_string(),
            native_token.clone(),
            order.token_out().clone(),
            Some(native_token.one().clone()),
            None,
            false,
            BigUint::ZERO,
            Bytes::zero(20),
            None,
        );

        let route = algorithm
            .get_best_route(&native_order, gas_price_ref, None)
            .unwrap();

        // then use the price from the previous route in the actual solve
        algorithm.get_best_route(
            order,
            gas_price_ref,
            Some(native_token.one().clone() / route.amount_out()),
        )
    }

    /// Add market data - delegates to algorithm
    pub async fn add_market_data(
        &self,
        state_id: Bytes,
        component: ProtocolComponent,
        state: Box<dyn ProtocolSim>,
    ) {
        let mut algorithm = self.algorithm.lock().await;
        algorithm.add_market_data(state_id, component, state);
    }

    /// Remove market data - delegates to algorithm
    pub async fn remove_market_data(&self, state_id: Bytes, component: ProtocolComponent) {
        let mut algorithm = self.algorithm.lock().await;
        algorithm.remove_market_data(state_id, component);
    }

    /// Update an existing state with new data - delegates to algorithm
    pub async fn update_market_state(&self, state_id: Bytes, new_state: Box<dyn ProtocolSim>) {
        let mut algorithm = self.algorithm.lock().await;
        algorithm.update_market_state(state_id, new_state);
    }

    /// Get current gas price
    pub async fn get_gas_price(&self) -> Option<GasPrice> {
        let gas_price = self.current_gas_price.lock().await;
        gas_price.clone()
    }

    pub fn get_tokens(&self) -> &HashMap<Bytes, Token> {
        &self.tokens
    }

    /// Get the TVL threshold configuration
    pub fn get_tvl_threshold(&self) -> (f64, f64) {
        self.tvl_threshold
    }

    /// Start background tasks for market data updates
    /// 
    /// This starts independent background tasks that will continuously update
    /// the solver's market data and gas prices. The tasks run independently
    /// and don't block the solver from being used for quote/solve operations.
    /// 
    /// Returns handles to the background tasks for graceful shutdown if needed.
    pub async fn start_background_updates(&self) -> Result<(JoinHandle<()>, JoinHandle<()>), Box<dyn std::error::Error + Send + Sync>> {
        println!("Starting background market data updates...");

        // Clone shared state references for background tasks
        let algorithm = Arc::clone(&self.algorithm);
        let current_gas_price = Arc::clone(&self.current_gas_price);
        let tokens = self.tokens.clone();

        // Clone configuration for background tasks
        let tycho_url = self.tycho_url.clone();
        let _tycho_api_key = self.tycho_api_key.clone();
        let chain = self.chain;
        let protocols = self.protocols.clone();
        let tvl_threshold = self.tvl_threshold;

        // 1. Spawn indexer task that updates algorithm state
        let indexer_handle = tokio::spawn(async move {
            println!("Tycho indexer task started");
            println!("Configuration:");
            println!("  - Tycho URL: {}", tycho_url);
            println!("  - Chain: {:?}", chain);
            println!("  - Protocols: {:?}", protocols);
            println!("  - TVL threshold: ({}, {})", tvl_threshold.0, tvl_threshold.1);
            println!("  - Tracking {} tokens", tokens.len());

            // Create channels for updates  
            let (_update_tx, mut update_rx) = mpsc::channel::<tycho_simulation::protocol::models::Update>(100);

            // Spawn the actual indexer connection task
            let _indexer_task = tokio::spawn(async move {
                // TODO: Implement actual Tycho indexer connection
                // For now, simulate periodic updates
                loop {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    println!("Would connect to Tycho indexer and send updates");
                    
                    // Simulate sending updates - in real implementation:
                    // 1. Create ProtocolStreamBuilder with tycho_url and chain
                    // 2. Add specified protocols and set authentication
                    // 3. Set tokens filter and TVL threshold  
                    // 4. Build stream and listen for updates
                    // 5. Send updates through update_tx channel
                }
            });

            // Process indexer updates and apply to algorithm
            while let Some(update) = update_rx.recv().await {
                let mut algo = algorithm.lock().await;
                
                // Process new pairs
                for (state_id, component) in update.new_pairs {
                    if let Some(state) = update.states.get(&state_id) {
                        algo.add_market_data(
                            Bytes::from(state_id.as_bytes()),
                            component.into(),
                            state.clone_box(),
                        );
                    }
                }

                // Process removed pairs  
                for (state_id, component) in update.removed_pairs {
                    algo.remove_market_data(Bytes::from(state_id.as_bytes()), component.into());
                }

                // Process state updates
                for (state_id, state) in update.states {
                    algo.update_market_state(Bytes::from(state_id.as_bytes()), state.clone_box());
                }
            }
        });

        // 2. Spawn gas price task that updates gas price state
        let gas_handle = tokio::spawn(async move {
            println!("Gas price fetcher task started");
            let mut interval = tokio::time::interval(Duration::from_secs(30));

            loop {
                interval.tick().await;

                // TODO: Implement actual gas price fetching from chain
                // For now, use mock gas price
                let mock_gas_price = GasPrice::from_legacy(BigUint::from(20_000_000_000u64)); // 20 gwei
                
                println!("Updating gas price: {}", mock_gas_price);
                let mut gas_price = current_gas_price.lock().await;
                *gas_price = Some(mock_gas_price);
            }
        });

        println!("Background tasks spawned successfully");
        Ok((indexer_handle, gas_handle))
    }

    async fn spawn_tycho_indexer_task(
        &self,
        _tx: mpsc::Sender<tycho_simulation::protocol::models::Update>,
    ) -> Result<JoinHandle<()>, Box<dyn std::error::Error + Send + Sync>> {
        // Capture the fields needed for the indexer task
        let tycho_url = self.tycho_url.clone();
        let _tycho_api_key = self.tycho_api_key.clone();
        let chain = self.chain;
        let protocols = self.protocols.clone();
        let tvl_threshold = self.tvl_threshold;
        let tokens: Vec<_> = self.tokens.keys().cloned().collect();

        let handle = tokio::spawn(async move {
            println!("Tycho indexer task started");
            println!("Configuration:");
            println!("  - Tycho URL: {}", tycho_url);
            println!("  - Chain: {:?}", chain);
            println!("  - Protocols: {:?}", protocols);
            println!("  - TVL threshold: ({}, {})", tvl_threshold.0, tvl_threshold.1);
            println!("  - Tracking {} tokens", tokens.len());

            // TODO: Implement Tycho indexer connection
            // 1. Create ProtocolStreamBuilder with tycho_url and chain
            // 2. Add specified protocols to the stream builder
            // 3. Set authentication key with tycho_api_key
            // 4. Configure stream options (skip decode failures, timeout, etc.)
            // 5. Set tokens filter using the tokens vec
            // 6. Set TVL threshold using tvl_threshold (min_tvl: tvl_threshold.0, max_tvl:
            //    tvl_threshold.1)
            // 7. Build the stream
            // 8. Listen for updates in a loop
            // 9. Send updates through the tx channel to main event loop

            // TODO: Note that we also want RFQ updates here. Most likely we will need something
            // like what is done in the integration test. To merge both streams.

            // For now, keep task alive as placeholder
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                println!("Indexer task placeholder - would connect to Tycho here with TVL threshold ({}, {})", 
                    tvl_threshold.0, tvl_threshold.1);
            }
        });

        Ok(handle)
    }

    fn spawn_gas_price_task(&self, tx: mpsc::Sender<GasPrice>) -> JoinHandle<()> {
        tokio::spawn(async move {
            println!("Gas price fetcher task started");
            let mut interval = tokio::time::interval(Duration::from_secs(30)); // Every 30 seconds

            loop {
                interval.tick().await;

                // TODO: Implement actual gas price fetching from chain
                let mock_gas_price = GasPrice::from_legacy(BigUint::from(20_000_000_000u64)); // 20 gwei

                if tx.send(mock_gas_price).await.is_err() {
                    println!("Main loop receiver dropped, stopping gas price fetcher");
                    break;
                }
            }
        })
    }

    async fn process_indexer_update(
        &self,
        update: tycho_simulation::protocol::models::Update,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Check if update contains any of our target tokens
        let should_process = update
            .new_pairs
            .values()
            .any(|component| {
                component
                    .tokens
                    .iter()
                    .any(|token| self.tokens.contains_key(&token.address))
            });

        if !should_process {
            return Ok(());
        }

        let mut algorithm = self.algorithm.lock().await;

        // Process new pairs
        for (state_id, component) in update.new_pairs {
            if let Some(state) = update.states.get(&state_id) {
                algorithm.add_market_data(
                    Bytes::from(state_id.as_bytes()),
                    component.into(),
                    state.clone_box(),
                );
            }
        }

        // Process removed pairs
        for (state_id, component) in update.removed_pairs {
            algorithm.remove_market_data(Bytes::from(state_id.as_bytes()), component.into());
        }

        // Process state updates
        for (state_id, state) in update.states {
            algorithm.update_market_state(Bytes::from(state_id.as_bytes()), state.clone_box());
        }

        Ok(())
    }
}
