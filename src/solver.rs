use std::{collections::HashMap, time::Duration};

use num_bigint::BigUint;
use tokio::{sync::mpsc, task::JoinHandle};
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
    algorithm: A,
    chain: Chain,
    tycho_url: String,
    tycho_api_key: String,
    protocols: Option<Vec<String>>,
    tokens: HashMap<Bytes, Token>,
    gas_price_fetcher: GasPriceFetcher,
    current_gas_price: Option<GasPrice>,
    tvl_threshold: (f64, f64), // (min_tvl, max_tvl) for protocol stream builder
}

impl<A: Algorithm> Solver<A> {
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
            algorithm: A::new(max_hops),
            chain,
            tycho_url,
            tycho_api_key,
            protocols,
            tokens,
            gas_price_fetcher: GasPriceFetcher::new(),
            current_gas_price: None,
            tvl_threshold,
        })
    }

    /// Find the best route for an Order (delegates to algorithm)
    pub fn solve_order(&self, order: &Order) -> Option<Route> {
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

        let route = self
            .algorithm
            .get_best_route(&native_order, self.current_gas_price.as_ref(), None)
            .unwrap();

        // then use the price from the previous route in the actual solve
        self.algorithm.get_best_route(
            order,
            self.current_gas_price.as_ref(),
            Some(native_token.one().clone() / route.amount_out()),
        )
    }

    /// Add market data - delegates to algorithm
    pub fn add_market_data(
        &mut self,
        state_id: Bytes,
        component: ProtocolComponent,
        state: Box<dyn ProtocolSim>,
    ) {
        self.algorithm
            .add_market_data(state_id, component, state);
    }

    /// Remove market data - delegates to algorithm
    pub fn remove_market_data(&mut self, state_id: Bytes, component: ProtocolComponent) {
        self.algorithm
            .remove_market_data(state_id, component);
    }

    /// Update an existing state with new data - delegates to algorithm
    pub fn update_market_state(&mut self, state_id: Bytes, new_state: Box<dyn ProtocolSim>) {
        self.algorithm
            .update_market_state(state_id, new_state);
    }

    /// Get current gas price
    pub fn get_gas_price(&self) -> Option<&GasPrice> {
        self.current_gas_price.as_ref()
    }

    pub fn get_tokens(&self) -> &HashMap<Bytes, Token> {
        &self.tokens
    }

    /// Get the TVL threshold configuration
    pub fn get_tvl_threshold(&self) -> (f64, f64) {
        self.tvl_threshold
    }

    /// Run the solver with background tasks for market data updates
    pub async fn run(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("Starting Tycho Router Solver...");

        // Create channels for inter-task communication
        let (indexer_tx, mut indexer_rx) = mpsc::channel(100);
        let (gas_tx, mut gas_rx) = mpsc::channel(10);

        // 1. Spawn task to connect to Tycho indexer
        let indexer_handle = self
            .spawn_tycho_indexer_task(indexer_tx)
            .await?;

        // 2. Spawn task to get gas prices periodically
        let gas_handle = self.spawn_gas_price_task(gas_tx);

        println!("Background tasks spawned, starting main loop...");

        // 3. Main event loop to handle updates
        loop {
            tokio::select! {
                // Handle indexer updates
                indexer_msg = indexer_rx.recv() => {
                    match indexer_msg {
                        Some(update) => {
                            println!("Processing indexer update for block/timestamp: {}",
                                update.block_number_or_timestamp);

                            self.process_indexer_update(update).await?;
                        }
                        None => {
                            println!("Indexer stream closed");
                            break;
                        }
                    }
                }

                // Handle gas price updates
                gas_msg = gas_rx.recv() => {
                    if let Some(gas_price) = gas_msg {
                        println!("Updated gas price: {}", gas_price);
                        self.current_gas_price = Some(gas_price);
                    }
                }
            }
        }

        // Clean up background tasks
        indexer_handle.abort();
        gas_handle.abort();

        Ok(())
    }

    async fn spawn_tycho_indexer_task(
        &self,
        _tx: mpsc::Sender<tycho_simulation::protocol::models::Update>,
    ) -> Result<JoinHandle<()>, Box<dyn std::error::Error + Send + Sync>> {
        // Capture the fields needed for the indexer task
        let tycho_url = self.tycho_url.clone();
        let tycho_api_key = self.tycho_api_key.clone();
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
        &mut self,
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

        // Process new pairs
        for (state_id, component) in update.new_pairs {
            if let Some(state) = update.states.get(&state_id) {
                self.algorithm.add_market_data(
                    Bytes::from(state_id.as_bytes()),
                    component.into(),
                    state.clone_box(),
                );
            }
        }

        // Process removed pairs
        for (state_id, component) in update.removed_pairs {
            self.algorithm
                .remove_market_data(Bytes::from(state_id.as_bytes()), component.into());
        }

        // Process state updates
        for (state_id, state) in update.states {
            self.algorithm
                .update_market_state(Bytes::from(state_id.as_bytes()), state.clone_box());
        }

        Ok(())
    }
}
