//! Tycho feed for keeping market data synchronized.
//!
//! The TychoFeed connects to Tycho's WebSocket API and:
//! - Receives component/state updates
//! - Updates MarketState (exclusive write access)
//! - Broadcasts MarketEvents to Solvers

use std::{
    collections::{HashMap, HashSet},
    pin::Pin,
    time::Instant,
};

use metrics::{gauge, histogram};
use rustc_hash::FxHashMap;
use tokio::sync::{broadcast, oneshot};
use tokio_stream::{Stream, StreamExt};
use tracing::{debug, info, instrument, span, trace, warn, Instrument, Level};
use tycho_simulation::{
    evm::{
        pending::PendingBlockProcessor,
        stream::{BlockStepController, ProtocolStreamBuilder},
    },
    protocol::models::{ProtocolComponent, Update},
    tycho_client::feed::component_tracker::ComponentFilter,
    tycho_common::{simulation::protocol_sim::ProtocolSim, traits::TxDeltaIndexer, Bytes},
    utils::load_all_tokens,
};

use crate::{
    feed::{
        book_stream::{next_book_update, BookStream, BookUpdate},
        events::MarketEvent,
        market_data::{MarketData, MarketState},
        protocol_registry::{has_tycho_protocols, open_price_level_stream, register_exchanges},
        when_configured, DataFeedError, TychoFeedConfig,
    },
    types::{ComponentId, ComponentTopology},
};

/// The Tycho indexer that keeps market data synchronized.
///
/// # Responsibilities
///
/// - Connect to Tycho WebSocket and maintain connection
/// - Process incoming component/state updates
/// - Update MarketState (holds exclusive write access)
/// - Broadcast MarketEvents to all subscribed Solvers
/// - Periodically refresh gas prices from RPC
pub(crate) struct TychoFeed {
    /// Configuration.
    config: TychoFeedConfig,
    /// Shared market data (we have write access).
    market_data: MarketData,
    /// Event broadcaster.
    event_tx: broadcast::Sender<MarketEvent>,
}

/// Client-metadata entries Fynd reports to Tycho via the `X-Tycho-Client-Metadata` header.
fn fynd_client_metadata() -> [(&'static str, &'static str); 1] {
    [("fynd_version", env!("CARGO_PKG_VERSION"))]
}

/// Yields the next update from the pAMM price level stream, pending forever when none is
/// configured.
///
/// The stream reconnects on its own for as long as it is polled, so it ending means it gave up
/// for good. That is a feed error rather than a clean stop, the same way a book feed's task
/// returning is.
async fn next_price_level_update<S>(
    stream: &mut Option<Pin<Box<S>>>,
) -> Result<Update, DataFeedError>
where
    S: Stream<Item = Update> + Send + ?Sized,
{
    when_configured(
        stream
            .as_mut()
            .map(|stream| stream.next()),
    )
    .await
    .ok_or_else(|| DataFeedError::StreamError("price level stream ended unexpectedly".to_string()))
}

/// The handles a caller of [`TychoFeed::run_with_pending`] waits on while the feed sets up.
///
/// Every setup failure is fanned out to every handle, so the caller learns the root cause from
/// whichever one it awaits first.
pub(crate) struct PendingFeedSetup {
    pending_tx: oneshot::Sender<Result<PendingBlockProcessor, String>>,
    step_tx: Option<oneshot::Sender<Result<BlockStepController, String>>>,
}

impl PendingFeedSetup {
    /// A setup that delivers the pending processor alone; blocks flow freely.
    pub(crate) fn new(pending_tx: oneshot::Sender<Result<PendingBlockProcessor, String>>) -> Self {
        Self { pending_tx, step_tx: None }
    }

    /// A setup that also delivers a [`BlockStepController`], which holds each block until
    /// the caller releases it.
    #[cfg(feature = "experimental")]
    pub(crate) fn new_with_step_controller(
        pending_tx: oneshot::Sender<Result<PendingBlockProcessor, String>>,
        step_tx: oneshot::Sender<Result<BlockStepController, String>>,
    ) -> Self {
        Self { pending_tx, step_tx: Some(step_tx) }
    }

    fn needs_step_controller(&self) -> bool {
        self.step_tx.is_some()
    }

    fn fail(self, message: &str) {
        let Self { pending_tx, step_tx } = self;
        let _ = pending_tx.send(Err(message.to_string()));
        if let Some(step_tx) = step_tx {
            let _ = step_tx.send(Err(message.to_string()));
        }
    }

    fn deliver(self, pending: PendingBlockProcessor, controller: Option<BlockStepController>) {
        let Self { pending_tx, step_tx } = self;
        if pending_tx.send(Ok(pending)).is_err() {
            tracing::warn!(
                "PendingBlockProcessor receiver dropped before send; continuing without pending \
                 updates"
            );
        }
        if let (Some(step_tx), Some(controller)) = (step_tx, controller) {
            if step_tx.send(Ok(controller)).is_err() {
                tracing::warn!(
                    "BlockStepController receiver dropped before send; the dropped controller \
                     lets blocks flow freely"
                );
            }
        }
    }
}

impl TychoFeed {
    /// Creates a new TychoFeed.
    ///
    /// # Arguments
    ///
    /// * `config` - Indexer configuration
    /// * `market_data` - Shared market data reference
    pub(crate) fn new(config: TychoFeedConfig, market_data: MarketData) -> Self {
        let (event_tx, _event_rx) = broadcast::channel(1024);

        Self { config, market_data, event_tx }
    }

    /// Returns a new subscriber for market events.
    pub(crate) fn subscribe(&self) -> broadcast::Receiver<MarketEvent> {
        self.event_tx.subscribe()
    }

    /// Returns a clone of the event sender.
    pub(crate) fn event_sender(&self) -> broadcast::Sender<MarketEvent> {
        self.event_tx.clone()
    }

    /// Creates a Tycho stream builder with the configured per-subscription delta buffer.
    /// Keeping this construction in one place ensures normal, pending, and step-controlled feeds
    /// use the same setting.
    fn protocol_stream_builder(&self) -> ProtocolStreamBuilder {
        let builder = ProtocolStreamBuilder::new(&self.config.tycho_url, self.config.chain);
        match self.config.subscription_buffer_size {
            Some(size) => builder.subscription_buffer_size(size),
            None => builder,
        }
    }

    /// Runs the indexer event loop until the underlying Tycho stream ends or errors.
    ///
    /// This method does not itself reconnect. Transient transport failures are absorbed
    /// by tycho-client's internal reconnection, but a hard stream error propagates out as
    /// an `Err` and a clean stream end returns `Ok(())`. In either case the feed stops,
    /// which tears down the solver and exits the process (crash-only design — the
    /// orchestrator is expected to restart it). It is recommended to call this in a
    /// dedicated tokio task.
    pub(crate) async fn run(self) -> Result<(), DataFeedError> {
        info!(
            tycho_url = %self.config.tycho_url,
            protocols = ?self.config.protocols,
            "Starting Data Feed..."
        );

        let tycho_api_key = self
            .config
            .tycho_api_key
            .clone()
            .or_else(|| std::env::var("TYCHO_API_KEY").ok());

        let all_tokens = load_all_tokens(
            self.config.tycho_url.as_str(),
            !self.config.use_tls,
            tycho_api_key.as_deref(),
            true,
            self.config.chain,
            Some(self.config.min_token_quality),
            self.config.traded_n_days_ago,
        )
        .await
        .map_err(|e| DataFeedError::StreamError(e.to_string()))?;

        debug!("Loaded {} tokens from Tycho", all_tokens.len());

        // Opened before the Tycho stream so a bad venue, chain or credential fails on the
        // configuration rather than after the connection work.
        let mut price_level_stream =
            open_price_level_stream(self.config.chain, &self.config.protocols, &all_tokens)?
                .map(Box::pin);
        let mut book_stream = BookStream::open(&self.config, all_tokens.clone())?;

        let mut protocol_stream = if has_tycho_protocols(&self.config.protocols) {
            let tvl_filter = ComponentFilter::with_tvl_range(
                self.config.min_tvl / self.config.tvl_buffer_ratio,
                self.config.min_tvl,
            )
            .blocklist(
                self.config
                    .blocklisted_components
                    .clone(),
            );

            let mut stream_builder = register_exchanges(
                self.protocol_stream_builder()
                    .skip_state_decode_failures(true),
                tvl_filter,
                &self.config.protocols,
            )?
            .auth_key(self.config.tycho_api_key.clone())
            .no_tls(!self.config.use_tls)
            .skip_state_decode_failures(true)
            .min_token_quality(self.config.min_token_quality as u32)
            .add_client_metadata(fynd_client_metadata());

            if self.config.partial_blocks {
                stream_builder = stream_builder.enable_partial_blocks();
            }

            Some(Box::pin(
                stream_builder
                    .set_tokens(all_tokens.clone())
                    .await
                    .build()
                    .await
                    .map_err(|e| DataFeedError::StreamError(e.to_string()))?,
            ))
        } else {
            None
        };

        // Loop through block updates from both streams
        loop {
            tokio::select! {
                // Handle protocol stream messages
                msg = when_configured(protocol_stream.as_mut().map(|stream| stream.next())) => {
                    match msg {
                        Some(msg) => {
                            trace!("Received message from protocol stream: {:?}", msg);
                            let msg = msg.map_err(|e| DataFeedError::StreamError(e.to_string()))?;
                            self.handle_tycho_message(msg).await?;
                        }
                        None => {
                            info!("Protocol stream ended");
                            break;
                        }
                    }
                }
                update = next_book_update(&mut book_stream) => {
                    self.handle_book_update(update?).await?;
                }
                msg = next_price_level_update(&mut price_level_stream) => {
                    let msg = msg?;
                    trace!("Received message from price level stream: {:?}", msg);
                    self.handle_tycho_message(msg).await?;
                }
            }
        }

        Ok(())
    }

    /// Like [`run`](Self::run) but calls [`ProtocolStreamBuilder::build_with_pending`]
    /// and delivers the [`PendingBlockProcessor`] via `setup` before entering the stream
    /// loop.
    ///
    /// When `setup` carries a step channel, every block is held by the
    /// [`BlockStepController`] delivered on it: the caller must call
    /// [`BlockStepController::trigger_next_block`] for each block to be processed, and
    /// dropping the controller lets blocks flow freely. The controller needs at least one
    /// Tycho-streamed protocol; a configuration without one fails with
    /// [`DataFeedError::Config`].
    ///
    /// If setup fails before the handles can be created, the error message is sent through
    /// every channel so the caller can surface the root cause instead of seeing only
    /// "channel closed". Handles whose receiver has already been dropped are discarded and
    /// the feed continues normally.
    ///
    /// Book feeds are handled alongside the EVM stream, identical to [`run`](Self::run).
    /// The `PendingBlockProcessor` only covers EVM on-chain state.
    pub(crate) async fn run_with_pending(
        self,
        setup: PendingFeedSetup,
        pending_indexers: Vec<(String, Box<dyn TxDeltaIndexer>)>,
    ) -> Result<(), DataFeedError> {
        info!(
            tycho_url = %self.config.tycho_url,
            protocols = ?self.config.protocols,
            step_controlled = setup.needs_step_controller(),
            "Starting Data Feed (with pending)..."
        );

        if setup.needs_step_controller() && !has_tycho_protocols(&self.config.protocols) {
            let msg = "step controller requires at least one Tycho-streamed protocol".to_string();
            setup.fail(&msg);
            return Err(DataFeedError::Config(msg));
        }

        let tycho_api_key = self
            .config
            .tycho_api_key
            .clone()
            .or_else(|| std::env::var("TYCHO_API_KEY").ok());

        let all_tokens = match load_all_tokens(
            self.config.tycho_url.as_str(),
            !self.config.use_tls,
            tycho_api_key.as_deref(),
            true,
            self.config.chain,
            Some(self.config.min_token_quality),
            self.config.traded_n_days_ago,
        )
        .await
        {
            Ok(t) => t,
            Err(e) => {
                let e = DataFeedError::StreamError(e.to_string());
                setup.fail(&e.to_string());
                return Err(e);
            }
        };

        debug!("Loaded {} tokens from Tycho", all_tokens.len());

        // Opened before `setup` is answered: a bad venue, chain or credential has to reach the
        // caller as an error, not as a processor for a feed that dies on the next line.
        let mut price_level_stream =
            match open_price_level_stream(self.config.chain, &self.config.protocols, &all_tokens) {
                Ok(stream) => stream.map(Box::pin),
                Err(e) => {
                    setup.fail(&e.to_string());
                    return Err(e);
                }
            };
        let mut book_stream = match BookStream::open(&self.config, all_tokens.clone()) {
            Ok(book_stream) => book_stream,
            Err(e) => {
                setup.fail(&e.to_string());
                return Err(e);
            }
        };

        let mut stream_builder = match register_exchanges(
            self.protocol_stream_builder()
                .skip_state_decode_failures(true),
            ComponentFilter::with_tvl_range(
                self.config.min_tvl / self.config.tvl_buffer_ratio,
                self.config.min_tvl,
            )
            .blocklist(
                self.config
                    .blocklisted_components
                    .clone(),
            ),
            &self.config.protocols,
        ) {
            Ok(sb) => sb,
            Err(e) => {
                setup.fail(&e.to_string());
                return Err(e);
            }
        }
        .auth_key(self.config.tycho_api_key.clone())
        .skip_state_decode_failures(true)
        .min_token_quality(self.config.min_token_quality as u32)
        .add_client_metadata(fynd_client_metadata());

        if self.config.partial_blocks {
            stream_builder = stream_builder.enable_partial_blocks();
        }

        let mut stream_builder = stream_builder
            .set_tokens(all_tokens.clone())
            .await;

        for (extractor, indexer) in pending_indexers {
            stream_builder = match stream_builder.with_pending_indexer(&extractor, indexer) {
                Ok(sb) => sb,
                Err(e) => {
                    let e = DataFeedError::StreamError(e.to_string());
                    setup.fail(&e.to_string());
                    return Err(e);
                }
            };
        }

        // `build_with_pending` hands blocks to the step controller once `with_step_controller`
        // has been called, so one build serves both the stepped and the free-running feed.
        let (stream_builder, controller) = if setup.needs_step_controller() {
            let (stream_builder, controller) = stream_builder.with_step_controller();
            (stream_builder, Some(controller))
        } else {
            (stream_builder, None)
        };

        let (protocol_stream, pending) = match stream_builder
            .build_with_pending()
            .await
        {
            Ok(pair) => pair,
            Err(e) => {
                let e = DataFeedError::StreamError(e.to_string());
                setup.fail(&e.to_string());
                return Err(e);
            }
        };
        let mut protocol_stream = Box::pin(protocol_stream);

        setup.deliver(pending, controller);

        loop {
            tokio::select! {
                msg = protocol_stream.next() => {
                    match msg {
                        Some(msg) => {
                            trace!("Received message from protocol stream: {:?}", msg);
                            let msg = msg.map_err(|e| DataFeedError::StreamError(e.to_string()))?;
                            self.handle_tycho_message(msg).await?;
                        }
                        None => {
                            info!("Protocol stream ended");
                            break;
                        }
                    }
                }
                update = next_book_update(&mut book_stream) => {
                    self.handle_book_update(update?).await?;
                }
                msg = next_price_level_update(&mut price_level_stream) => {
                    let msg = msg?;
                    trace!("Received message from price level stream: {:?}", msg);
                    self.handle_tycho_message(msg).await?;
                }
            }
        }

        Ok(())
    }

    /// Handles a message from Tycho stream.
    #[instrument(skip(self, msg))]
    pub(crate) async fn handle_tycho_message(&self, msg: Update) -> Result<(), DataFeedError> {
        // Collect variables for market shared data update
        let Update {
            new_pairs: added_components,
            removed_pairs: removed_components,
            states: updated_or_new_states,
            sync_states,
            ..
        } = msg;
        let removed_components: HashMap<ComponentId, ProtocolComponent> = removed_components
            .into_iter()
            .map(|(id, component)| (market_component_id(id), component))
            .collect();
        let updated_or_new_states: HashMap<ComponentId, Box<dyn ProtocolSim>> =
            updated_or_new_states
                .into_iter()
                .map(|(id, state)| (market_component_id(id), state))
                .collect();

        // Tycho re-sends an aggregated RFQ venue as a new pair on every poll. It stays here, as a
        // state update, unless the upsert finds it new or reshaped.
        let mut updated_components_ids: HashSet<ComponentId> = updated_or_new_states
            .keys()
            .cloned()
            .collect();
        updated_components_ids.extend(
            added_components
                .values()
                .map(|component| component.id.to_string()),
        );

        let maybe_new_tokens = added_components
            .values()
            .flat_map(|component| component.tokens.iter().cloned());
        info!(
            "received block/timestamp {} with {} new components, {} removed, {} updated",
            msg.block_number,
            added_components.len(),
            removed_components.len(),
            updated_or_new_states.len()
        );
        trace!("Updating market data");
        let new_block_number = msg.block_number;
        let update_start = Instant::now();
        let mut latest_component_count = 0;
        let mut token_count = 0;
        let mut unsynced_protocol_count = 0;
        let mut latest_block_fields = None;
        let mut added_topology: FxHashMap<ComponentId, ComponentTopology> = FxHashMap::default();
        self.market_data
            .apply_block_update(new_block_number, |market_data| {
                let upserted = market_data.upsert_components(
                    added_components
                        .clone()
                        .into_values()
                        .map(market_component),
                );
                market_data.remove_components(removed_components.keys());
                market_data.upsert_tokens(maybe_new_tokens);
                market_data.update_states(updated_or_new_states);
                market_data.update_sync_status(sync_states, self.config.max_sync_lag);

                added_topology = upserted
                    .new
                    .into_iter()
                    .chain(upserted.reshaped)
                    .filter_map(|id| {
                        let topology = market_data.topology_of(&id)?;
                        Some((id, topology))
                    })
                    .collect();
                latest_component_count = market_data.component_count();
                token_count = market_data.token_count();
                unsynced_protocol_count = market_data.unsynced_protocols().len();
                latest_block_fields = market_data
                    .last_updated()
                    .map(|block_info| (block_info.number(), block_info.timestamp()));
            })
            .instrument(span!(Level::DEBUG, "data_feed_write_lock"))
            .await;
        trace!("Market data updated");

        histogram!("market_update_duration_seconds").record(update_start.elapsed().as_secs_f64());
        // Counts components; the legacy "pools" metric name is kept so existing
        // dashboards and alerts keep working.
        gauge!("market_pools").set(latest_component_count as f64);
        gauge!("market_tokens").set(token_count as f64);
        gauge!("market_unsynced_protocols").set(unsynced_protocol_count as f64);
        if let Some((block_number, block_timestamp)) = latest_block_fields {
            gauge!("market_current_block").set(block_number as f64);
            gauge!("market_last_update_timestamp_seconds").set(block_timestamp as f64);
        }

        updated_components_ids.retain(|id| !added_topology.contains_key(id));

        // Only broadcast event if there are actual changes
        if !added_topology.is_empty() ||
            !removed_components.is_empty() ||
            !updated_components_ids.is_empty()
        {
            let market_update_event = MarketEvent::MarketUpdated {
                added_components: added_topology,
                removed_components: removed_components.into_keys().collect(),
                updated_components: updated_components_ids
                    .into_iter()
                    .collect(),
            };

            self.event_tx
                .send(market_update_event)
                .map_err(|e| DataFeedError::EventChannelError(e.to_string()))?;
        }

        Ok(())
    }

    /// Applies one complete set of books published by a venue to the market.
    ///
    /// A book feed prices off chain, so its updates belong to no block: they neither advance the
    /// market's block label nor invalidate a simulation overlay built against the current one.
    #[instrument(skip(self, update), fields(protocol_system = %update.protocol_system))]
    async fn handle_book_update(&self, update: BookUpdate) -> Result<(), DataFeedError> {
        let BookUpdate { protocol_system, mut added, mut removed, mut updated, mut states } =
            update;

        let update_start = Instant::now();
        let (refreshed_ids, added_topology, component_count, token_count) = {
            let mut market_data = self
                .market_data
                .write()
                .instrument(span!(Level::DEBUG, "data_feed_write_lock"))
                .await;
            drop_foreign_components(
                &market_data,
                &protocol_system,
                BookUpdateParts {
                    added: &mut added,
                    removed: &mut removed,
                    updated: &mut updated,
                    states: &mut states,
                },
            );
            info!(
                "received {} books from {protocol_system}: {} new components, {} removed, {} \
                 updated",
                states.len(),
                added.len(),
                removed.len(),
                updated.len()
            );
            let refreshed_ids: Vec<ComponentId> = updated
                .iter()
                .map(|component| component.id.to_string())
                .collect();
            market_data.upsert_tokens(
                added
                    .iter()
                    .flat_map(|component| component.tokens.iter().cloned()),
            );
            let upserted = market_data.upsert_components(
                added
                    .into_iter()
                    .chain(updated)
                    .map(market_component),
            );
            market_data.remove_components(removed.iter());
            market_data.update_states(states);
            let added_topology: FxHashMap<ComponentId, ComponentTopology> = upserted
                .new
                .into_iter()
                .chain(upserted.reshaped)
                .filter_map(|id| {
                    let topology = market_data.topology_of(&id)?;
                    Some((id, topology))
                })
                .collect();
            (
                refreshed_ids,
                added_topology,
                market_data.component_count(),
                market_data.token_count(),
            )
        };
        // A refreshed component whose swap directions changed is reported as added.
        let updated_components = refreshed_ids
            .into_iter()
            .filter(|id| !added_topology.contains_key(id))
            .collect();

        histogram!("market_update_duration_seconds").record(update_start.elapsed().as_secs_f64());
        gauge!("market_pools").set(component_count as f64);
        gauge!("market_tokens").set(token_count as f64);

        self.event_tx
            .send(MarketEvent::MarketUpdated {
                added_components: added_topology,
                removed_components: removed,
                updated_components,
            })
            .map_err(|e| DataFeedError::EventChannelError(e.to_string()))?;

        Ok(())
    }
}

/// The parts of a [`BookUpdate`] that name components.
struct BookUpdateParts<'a> {
    added: &'a mut Vec<ProtocolComponent>,
    removed: &'a mut Vec<ComponentId>,
    updated: &'a mut Vec<ProtocolComponent>,
    states: &'a mut HashMap<ComponentId, Box<dyn ProtocolSim>>,
}

/// Removes from a book update every component the market holds under another protocol system.
///
/// A book feed owns only the components of its own protocol system, so it must not replace,
/// refresh or remove a pool that Tycho or another feed serves under the same id.
fn drop_foreign_components(
    market_data: &MarketState,
    protocol_system: &str,
    BookUpdateParts { added, removed, updated, states }: BookUpdateParts<'_>,
) {
    let held_elsewhere = |id: &str| {
        market_data
            .get_component(id)
            .map(|held| held.protocol_system.as_str())
            .filter(|held_system| *held_system != protocol_system)
    };
    for components in [added, updated] {
        components.retain(|component| {
            let id = component.id.to_string();
            let Some(held_system) = held_elsewhere(&id) else {
                return true;
            };
            warn!(
                "Book feed {protocol_system} published component {id}, which the market holds \
                 under {held_system}; dropping it"
            );
            states.remove(&id);
            false
        });
    }
    removed.retain(|id| held_elsewhere(id).is_none());
}

/// The id the market stores a Tycho component under: `component.id.to_string()`, which is
/// lowercase hex.
///
/// An [`Update`] keys `states` and `removed_pairs` by the id string exactly as the indexer emitted
/// it, and some indexers emit checksum-cased ids (rocketpool's is
/// `0xae78736Cd615f374D3085123A210448E74Fc6393`). Without this, such a component's state is
/// stored under a key that its topology never looks up.
fn market_component_id(tycho_id: String) -> ComponentId {
    match tycho_id.parse::<Bytes>() {
        Ok(id) => id.to_string(),
        // tycho-simulation cannot build a component from a non-hex id, so no topology entry
        // exists to match; keep the key as given.
        Err(_) => tycho_id,
    }
}

/// Converts a streamed component into the market's representation without stripping the `0x`
/// prefix from its id, which the generic `From<ProtocolComponent>` conversion does.
fn market_component(
    component: ProtocolComponent,
) -> tycho_simulation::tycho_common::models::protocol::ProtocolComponent {
    tycho_simulation::tycho_common::models::protocol::ProtocolComponent {
        id: component.id.to_string(),
        protocol_system: component.protocol_system,
        protocol_type_name: component.protocol_type_name,
        chain: component.chain,
        tokens: component
            .tokens
            .into_iter()
            .map(|token| token.address)
            .collect(),
        static_attributes: component.static_attributes,
        change: Default::default(),
        creation_tx: component.creation_tx,
        created_at: component.created_at,
        contract_addresses: component.contract_ids,
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, env};

    use num_bigint::BigUint;
    use tycho_simulation::{
        book::SWAP_DIRECTIONS_ATTRIBUTE,
        protocol::models::{ProtocolComponent, Update},
        tycho_client::feed::{BlockHeader, SynchronizerState},
        tycho_common::{
            models::{token::Token, Chain},
            Bytes,
        },
        tycho_core::simulation::{
            errors::{SimulationError, TransitionError},
            protocol_sim::{Balances, GetAmountOutResult, ProtocolSim},
        },
    };

    use super::*;
    use crate::{
        algorithm::test_utils::encode_swap_directions,
        feed::{market_data::MarketData, TychoFeedConfig},
    };

    #[tokio::test]
    async fn test_pending_feed_setup_fail_reaches_every_handle() {
        let (pending_tx, pending_rx) = oneshot::channel();
        let (step_tx, step_rx) = oneshot::channel();
        let setup = PendingFeedSetup { pending_tx, step_tx: Some(step_tx) };

        setup.fail("tokens did not load");

        assert_eq!(
            pending_rx
                .await
                .unwrap()
                .err()
                .as_deref(),
            Some("tokens did not load")
        );
        assert_eq!(step_rx.await.unwrap().err().as_deref(), Some("tokens did not load"));
    }

    #[tokio::test]
    async fn test_pending_feed_setup_fail_without_step_channel() {
        let (pending_tx, pending_rx) = oneshot::channel();
        let setup = PendingFeedSetup::new(pending_tx);

        setup.fail("tokens did not load");

        assert_eq!(
            pending_rx
                .await
                .unwrap()
                .err()
                .as_deref(),
            Some("tokens did not load")
        );
    }

    /// Creates a new shared market data instance.
    fn new_shared_market_data() -> MarketData {
        MarketData::new_shared()
    }

    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    struct FeedMockProtocolSim {
        id: f64,
    }

    impl FeedMockProtocolSim {
        fn new(id: f64) -> Self {
            Self { id }
        }
    }

    #[typetag::serde]
    impl ProtocolSim for FeedMockProtocolSim {
        fn get_amount_out(
            &self,
            amount_in: BigUint,
            _token_in: &Token,
            _token_out: &Token,
        ) -> Result<GetAmountOutResult, SimulationError> {
            Ok(GetAmountOutResult {
                amount: amount_in,
                gas: BigUint::ZERO,
                new_state: Box::new(self.clone()),
            })
        }

        fn fee(&self) -> f64 {
            // We use .fee() to get the id of the FeedMockProtocolSim in the tests for our
            // assertions.
            self.id
        }

        fn spot_price(&self, _base: &Token, _quote: &Token) -> Result<f64, SimulationError> {
            Ok(0.0)
        }

        fn get_limits(
            &self,
            _sell_token: Bytes,
            _buy_token: Bytes,
        ) -> Result<(BigUint, BigUint), SimulationError> {
            Ok((BigUint::ZERO, BigUint::ZERO))
        }

        fn delta_transition(
            &mut self,
            _delta: tycho_simulation::tycho_core::dto::ProtocolStateDelta,
            _tokens: &std::collections::HashMap<Bytes, Token>,
            _balances: &Balances,
        ) -> Result<(), TransitionError> {
            Ok(())
        }

        fn clone_box(&self) -> Box<dyn ProtocolSim> {
            Box::new(self.clone())
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }

        fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
            self
        }

        fn eq(&self, _other: &dyn ProtocolSim) -> bool {
            true
        }
    }

    // Helper function to create a test config
    fn create_test_config() -> TychoFeedConfig {
        TychoFeedConfig::new(
            "ws://test.tycho.io".to_string(),
            Chain::Ethereum,
            Some("test_api_key".to_string()),
            false, // no TLS for test
            vec!["uniswap_v2".to_string()],
            10.0,
        )
    }

    #[tokio::test]
    async fn test_subscription_buffer_size_rejects_oversized_config_before_network_io() {
        let config = create_test_config().subscription_buffer_size(usize::MAX);
        let feed = TychoFeed::new(config, new_shared_market_data());

        let result = feed
            .protocol_stream_builder()
            .build()
            .await;
        let Err(error) = result else {
            panic!("an oversized subscription buffer size must be rejected during setup");
        };

        assert!(
            error
                .to_string()
                .contains("subscription buffer size must not exceed"),
            "Tycho should reject an oversized subscription buffer size before network I/O: {error}"
        );
    }

    // Helper to create a test token
    fn create_test_token(address: &str, symbol: &str) -> Token {
        Token {
            address: Bytes::from(address),
            symbol: symbol.to_string(),
            decimals: 18,
            tax: Default::default(),
            gas: vec![],
            chain: Chain::Ethereum,
            quality: 100,
        }
    }

    // Helper to create a test component
    fn create_test_component(id: &str, tokens: Vec<Token>) -> ProtocolComponent {
        let id_bytes = Bytes::from(id);

        ProtocolComponent::new(
            id_bytes.clone(),
            "uniswap_v2".to_string(),
            "uniswap_v2_pool".to_string(),
            Chain::Ethereum,
            tokens,
            vec![],
            HashMap::new(),
            Bytes::from(vec![0x12, 0x34]),
            chrono::DateTime::from_timestamp(1234567890, 0)
                .unwrap()
                .naive_utc(),
        )
    }

    #[tokio::test]
    async fn test_event_resubscription() {
        let config = create_test_config();
        let market_data = new_shared_market_data();

        let feed = TychoFeed::new(config, market_data);

        // Subscribe multiple times to verify multiple subscribers can be created
        let mut sub1 = feed.subscribe();
        let mut sub2 = feed.subscribe();

        // Get event sender
        let sender = feed.event_sender();

        sender
            .send(MarketEvent::MarketUpdated {
                added_components: FxHashMap::default(),
                removed_components: Vec::new(),
                updated_components: Vec::new(),
            })
            .expect("Failed to send event");

        let event_1 = sub1.recv().await.unwrap();
        let event_2 = sub2.recv().await.unwrap();
        assert_eq!(event_1, event_2);
        assert_eq!(
            event_1,
            MarketEvent::MarketUpdated {
                added_components: FxHashMap::default(),
                removed_components: Vec::new(),
                updated_components: Vec::new(),
            }
        );
    }

    #[tokio::test]
    async fn test_handle_message_adds_new_components() {
        let market_data = new_shared_market_data();
        let feed = TychoFeed::new(create_test_config(), market_data.clone());
        let mut event_rx = feed.subscribe();

        // Create a new component
        let component_id = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let token1 = create_test_token("0x1111111111111111111111111111111111111111", "TKN1");
        let token2 = create_test_token("0x2222222222222222222222222222222222222222", "TKN2");
        let test_component =
            create_test_component(component_id, vec![token1.clone(), token2.clone()]);

        let mut new_pairs = HashMap::new();
        new_pairs.insert(component_id.to_string(), test_component.clone());

        let update = Update::new(12345, HashMap::new(), new_pairs);

        // Handle the message
        feed.handle_tycho_message(update)
            .await
            .expect("Failed to handle message");

        // Verify component was added to market data
        let data = market_data.read().await;

        let component = data
            .get_component(component_id)
            .expect("Component should be in market data");
        assert_eq!(
            component.clone(),
            tycho_simulation::tycho_common::models::protocol::ProtocolComponent {
                id: component_id.to_string(),
                protocol_system: "uniswap_v2".to_string(),
                protocol_type_name: "uniswap_v2_pool".to_string(),
                chain: Chain::Ethereum,
                tokens: vec![token1.address.clone(), token2.address.clone()],
                static_attributes: HashMap::new(),
                contract_addresses: vec![],
                change: Default::default(),
                creation_tx: Bytes::from(vec![0x12, 0x34]),
                created_at: chrono::DateTime::from_timestamp(1234567890, 0)
                    .unwrap()
                    .naive_utc(),
            }
        );
        drop(data);

        // Verify event was broadcast
        let event = event_rx
            .try_recv()
            .expect("Should receive event");
        assert_eq!(
            event,
            MarketEvent::MarketUpdated {
                added_components: FxHashMap::from_iter([(
                    component_id.to_string(),
                    vec![token1.address, token2.address].into()
                )]),
                removed_components: Vec::new(),
                updated_components: Vec::new(),
            }
        );
    }

    #[tokio::test]
    async fn handle_message_errs_when_no_subscribers() {
        // A broadcast send fails when no receivers are subscribed. The feed surfaces that as an
        // error rather than silently dropping the market event, which could leave worker graphs
        // stale; the underlying subscriber-exit is debugged separately.
        let market_data = new_shared_market_data();
        let feed = TychoFeed::new(create_test_config(), market_data.clone());
        drop(feed.subscribe()); // leaves zero live receivers

        let component_id = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let token1 = create_test_token("0x1111111111111111111111111111111111111111", "TKN1");
        let token2 = create_test_token("0x2222222222222222222222222222222222222222", "TKN2");
        let mut new_pairs = HashMap::new();
        new_pairs.insert(
            component_id.to_string(),
            create_test_component(component_id, vec![token1, token2]),
        );
        let update = Update::new(12345, HashMap::new(), new_pairs);

        // There are changes, so this reaches the broadcast send, which errors with no receivers.
        assert!(feed
            .handle_tycho_message(update)
            .await
            .is_err());

        // The market state is still applied before the (failed) notification.
        assert!(market_data
            .read()
            .await
            .get_component(component_id)
            .is_some());
    }

    #[tokio::test]
    async fn test_handle_message_removes_components() {
        let market_data = new_shared_market_data();

        let feed = TychoFeed::new(create_test_config(), market_data.clone());
        let mut event_rx = feed.subscribe();

        let component_id = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let token1 = create_test_token("0x1111111111111111111111111111111111111111", "TKN1");
        let token2 = create_test_token("0x2222222222222222222222222222222222222222", "TKN2");

        // First, add a component
        let mut new_pairs = HashMap::new();
        new_pairs.insert(
            component_id.to_string(),
            create_test_component(component_id, vec![token1.clone(), token2.clone()]),
        );

        let update = Update::new(12345, HashMap::new(), new_pairs);
        feed.handle_tycho_message(update)
            .await
            .expect("Failed to add component");

        // Verify it was added
        {
            let data = market_data.read().await;
            assert!(
                data.get_component(component_id)
                    .is_some(),
                "Component should exist before removal"
            );
        }

        let mut removed_pairs = HashMap::new();
        removed_pairs.insert(
            component_id.to_string(),
            create_test_component(component_id, vec![token1.clone(), token2.clone()]),
        );

        let update =
            Update::new(12345, HashMap::new(), HashMap::new()).set_removed_pairs(removed_pairs);

        feed.handle_tycho_message(update)
            .await
            .expect("Failed to handle removal");

        // Verify component was removed
        let data = market_data.read().await;
        assert!(
            data.get_component(component_id)
                .is_none(),
            "Component should be removed from market data"
        );
        drop(data);

        // Verify both events were broadcast
        let event_1 = event_rx
            .try_recv()
            .expect("Should receive event");
        let event_2 = event_rx
            .try_recv()
            .expect("Should receive event");
        assert_eq!(
            event_1,
            MarketEvent::MarketUpdated {
                added_components: FxHashMap::from_iter([(
                    component_id.to_string(),
                    vec![token1.address, token2.address].into()
                )]),
                removed_components: Vec::new(),
                updated_components: Vec::new(),
            }
        );
        assert_eq!(
            event_2,
            MarketEvent::MarketUpdated {
                added_components: FxHashMap::default(),
                removed_components: vec![component_id.to_string()],
                updated_components: Vec::new(),
            }
        );
    }

    #[tokio::test]
    async fn test_handle_message_checksum_cased_component_id() {
        let market_data = new_shared_market_data();
        let feed = TychoFeed::new(create_test_config(), market_data.clone());
        let mut event_rx = feed.subscribe();

        let tycho_id = "0xae78736Cd615f374D3085123A210448E74Fc6393";
        let market_id = "0xae78736cd615f374d3085123a210448e74fc6393";
        let token1 = create_test_token("0x1111111111111111111111111111111111111111", "TKN1");
        let token2 = create_test_token("0x2222222222222222222222222222222222222222", "TKN2");
        let component = create_test_component(tycho_id, vec![token1.clone(), token2.clone()]);
        let state_with_fee =
            |fee: f64| Box::new(FeedMockProtocolSim::new(fee)) as Box<dyn ProtocolSim>;

        let update = Update::new(
            12345,
            HashMap::from([(tycho_id.to_string(), state_with_fee(1.0))]),
            HashMap::from([(tycho_id.to_string(), component.clone())]),
        );
        feed.handle_tycho_message(update)
            .await
            .expect("Failed to add component");
        {
            let data = market_data.read().await;
            assert!(data.get_component(market_id).is_some());
            let state = data
                .get_simulation_state(market_id)
                .expect("the state should be stored under the component's market id");
            assert_eq!(state.fee(), 1.0);
        }

        let update = Update::new(
            12346,
            HashMap::from([(tycho_id.to_string(), state_with_fee(2.0))]),
            HashMap::new(),
        );
        feed.handle_tycho_message(update)
            .await
            .expect("Failed to update state");
        {
            let data = market_data.read().await;
            let state = data
                .get_simulation_state(market_id)
                .expect("the updated state should replace the stored one");
            assert_eq!(state.fee(), 2.0);
            assert!(data
                .get_simulation_state(tycho_id)
                .is_none());
        }

        let update = Update::new(12347, HashMap::new(), HashMap::new())
            .set_removed_pairs(HashMap::from([(tycho_id.to_string(), component)]));
        feed.handle_tycho_message(update)
            .await
            .expect("Failed to remove component");
        assert!(market_data
            .read()
            .await
            .get_component(market_id)
            .is_none());

        let events: Vec<MarketEvent> = std::iter::from_fn(|| event_rx.try_recv().ok()).collect();
        assert_eq!(
            events,
            vec![
                MarketEvent::MarketUpdated {
                    added_components: FxHashMap::from_iter([(
                        market_id.to_string(),
                        vec![token1.address, token2.address].into()
                    )]),
                    removed_components: Vec::new(),
                    updated_components: Vec::new(),
                },
                MarketEvent::MarketUpdated {
                    added_components: FxHashMap::default(),
                    removed_components: Vec::new(),
                    updated_components: vec![market_id.to_string()],
                },
                MarketEvent::MarketUpdated {
                    added_components: FxHashMap::default(),
                    removed_components: vec![market_id.to_string()],
                    updated_components: Vec::new(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn test_handle_message_updates_states() {
        let market_data = new_shared_market_data();
        let feed = TychoFeed::new(create_test_config(), market_data.clone());
        let mut event_rx = feed.subscribe();

        let component_id = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let token1 = create_test_token("0x1111111111111111111111111111111111111111", "TKN1");
        let token2 = create_test_token("0x2222222222222222222222222222222222222222", "TKN2");

        // First, add a component
        let mut new_pairs = HashMap::new();
        new_pairs.insert(
            component_id.to_string(),
            create_test_component(component_id, vec![token1.clone(), token2.clone()]),
        );

        // Create an update with state information
        let mut states = HashMap::new();
        states.insert(
            component_id.to_string(),
            Box::new(FeedMockProtocolSim::new(1.0)) as Box<dyn ProtocolSim>,
        );

        let update = Update::new(12345, states.clone(), new_pairs);
        feed.handle_tycho_message(update)
            .await
            .expect("Failed to add component");

        // Verify state was updated
        {
            let data = market_data.read().await;
            assert_eq!(
                data.get_component(component_id)
                    .expect("Component should be in market data")
                    .clone(),
                tycho_simulation::tycho_common::models::protocol::ProtocolComponent {
                    id: component_id.to_string(),
                    protocol_system: "uniswap_v2".to_string(),
                    protocol_type_name: "uniswap_v2_pool".to_string(),
                    chain: Chain::Ethereum,
                    tokens: vec![token1.address.clone(), token2.address.clone()],
                    static_attributes: HashMap::new(),
                    contract_addresses: vec![],
                    change: Default::default(),
                    creation_tx: Bytes::from(vec![0x12, 0x34]),
                    created_at: chrono::DateTime::from_timestamp(1234567890, 0)
                        .unwrap()
                        .naive_utc(),
                },
                "Component should be in market data"
            );
            assert_eq!(
                data.get_simulation_state(component_id)
                    .expect("Component should be in market data")
                    .fee(),
                1.0,
                "Component state fee should be 1.0"
            );
        }

        // Now update its state

        // Create an update with state information
        let new_state = Box::new(FeedMockProtocolSim::new(2.0)) as Box<dyn ProtocolSim>;
        let update = Update::new(
            12345,
            HashMap::from([(component_id.to_string(), new_state)]),
            HashMap::new(),
        );
        feed.handle_tycho_message(update)
            .await
            .expect("Failed to add component");

        // Verify state was updated
        {
            let data = market_data.read().await;
            assert_eq!(
                data.get_simulation_state(component_id)
                    .expect("Component should be in market data")
                    .fee(),
                2.0,
                "Component state fee should be 2.0"
            );
        }

        // Verify event was broadcast
        let event_1 = event_rx
            .try_recv()
            .expect("Should receive event");
        let event_2 = event_rx
            .try_recv()
            .expect("Should receive event");
        assert_eq!(
            event_1,
            MarketEvent::MarketUpdated {
                added_components: FxHashMap::from_iter([(
                    component_id.to_string(),
                    vec![token1.address, token2.address].into()
                )]),
                removed_components: Vec::new(),
                updated_components: vec![],
            }
        );
        assert_eq!(
            event_2,
            MarketEvent::MarketUpdated {
                added_components: FxHashMap::default(),
                removed_components: Vec::new(),
                updated_components: vec![component_id.to_string()],
            }
        );
    }

    const VENUE: &str = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    /// The feed that delivers a venue to the market.
    #[derive(Clone, Copy)]
    enum VenueSource {
        Tycho,
        Book,
    }

    /// Sends a venue that lists `directions`, given as indexes into `tokens`. Tycho re-sends an
    /// aggregated RFQ venue as a new pair; a book feed lists it as refreshed once it serves it.
    async fn send_venue(
        feed: &TychoFeed,
        source: VenueSource,
        tokens: &[Token],
        directions: &[(usize, usize)],
    ) {
        let directions: std::collections::BTreeSet<_> = directions
            .iter()
            .map(|&(token_in, token_out)| {
                (tokens[token_in].address.clone(), tokens[token_out].address.clone())
            })
            .collect();
        let mut venue = create_test_component(VENUE, tokens.to_vec());
        venue
            .static_attributes
            .insert(SWAP_DIRECTIONS_ATTRIBUTE.to_string(), encode_swap_directions(&directions));
        let states = HashMap::from([(
            VENUE.to_string(),
            Box::new(FeedMockProtocolSim::new(1.0)) as Box<dyn ProtocolSim>,
        )]);
        match source {
            VenueSource::Tycho => {
                let update =
                    Update::new(12345, states, HashMap::from([(VENUE.to_string(), venue)]));
                feed.handle_tycho_message(update)
                    .await
                    .expect("Failed to handle message");
            }
            VenueSource::Book => {
                let served = feed
                    .market_data
                    .read()
                    .await
                    .topology_of(VENUE)
                    .is_some();
                venue.protocol_system = "rfq:test".to_string();
                let (added, updated) =
                    if served { (vec![], vec![venue]) } else { (vec![venue], vec![]) };
                let update = BookUpdate {
                    protocol_system: "rfq:test".to_string(),
                    added,
                    removed: vec![],
                    updated,
                    states,
                };
                feed.handle_book_update(update)
                    .await
                    .expect("Failed to handle book update");
            }
        }
    }

    /// A re-sent venue with the same swap directions is a state update. With other swap
    /// directions it is added again.
    #[rstest::rstest]
    #[case::tycho(VenueSource::Tycho)]
    #[case::book(VenueSource::Book)]
    #[tokio::test]
    async fn test_handle_resent_component(#[case] source: VenueSource) {
        let feed = TychoFeed::new(create_test_config(), new_shared_market_data());
        let mut event_rx = feed.subscribe();
        let tokens = [
            create_test_token("0x1111111111111111111111111111111111111111", "TKN1"),
            create_test_token("0x2222222222222222222222222222222222222222", "TKN2"),
        ];

        send_venue(&feed, source, &tokens, &[(0, 1)]).await;
        send_venue(&feed, source, &tokens, &[(0, 1)]).await;
        send_venue(&feed, source, &tokens, &[(0, 1), (1, 0)]).await;

        let MarketEvent::MarketUpdated { added_components, .. } = event_rx.try_recv().unwrap();
        assert_eq!(
            added_components[VENUE]
                .swap_directions
                .len(),
            1
        );
        let MarketEvent::MarketUpdated { added_components, updated_components, .. } =
            event_rx.try_recv().unwrap();
        assert!(added_components.is_empty());
        assert_eq!(updated_components, vec![VENUE.to_string()]);
        let MarketEvent::MarketUpdated { added_components, updated_components, .. } =
            event_rx.try_recv().unwrap();
        assert_eq!(
            added_components[VENUE]
                .swap_directions
                .len(),
            2
        );
        assert!(updated_components.is_empty());
    }

    #[tokio::test]
    async fn test_handle_book_update_keeps_other_protocol_systems() {
        let market_data = new_shared_market_data();
        let feed = TychoFeed::new(create_test_config(), market_data.clone());
        let _event_rx = feed.subscribe();
        let tokens = vec![
            create_test_token("0x1111111111111111111111111111111111111111", "TKN1"),
            create_test_token("0x2222222222222222222222222222222222222222", "TKN2"),
        ];
        let pool_states = HashMap::from([(
            VENUE.to_string(),
            Box::new(FeedMockProtocolSim::new(1.0)) as Box<dyn ProtocolSim>,
        )]);
        let pool =
            HashMap::from([(VENUE.to_string(), create_test_component(VENUE, tokens.clone()))]);
        feed.handle_tycho_message(Update::new(12345, pool_states, pool))
            .await
            .unwrap();
        let mut book_component = create_test_component(VENUE, tokens);
        book_component.protocol_system = "rfq:test".to_string();
        let book_update = |added: Vec<ProtocolComponent>, removed: Vec<String>| {
            let mut states = HashMap::new();
            if !added.is_empty() {
                states.insert(
                    VENUE.to_string(),
                    Box::new(FeedMockProtocolSim::new(2.0)) as Box<dyn ProtocolSim>,
                );
            }
            BookUpdate {
                protocol_system: "rfq:test".to_string(),
                added,
                removed,
                updated: vec![],
                states,
            }
        };

        feed.handle_book_update(book_update(vec![book_component], vec![]))
            .await
            .unwrap();
        feed.handle_book_update(book_update(vec![], vec![VENUE.to_string()]))
            .await
            .unwrap();

        let data = market_data.read().await;
        let held = data.get_component(VENUE).unwrap();
        assert_eq!(held.protocol_system, "uniswap_v2");
        assert_eq!(
            data.get_simulation_state(VENUE)
                .unwrap()
                .fee(),
            1.0
        );
    }

    #[tokio::test]
    async fn test_handle_message_multiple_operations() {
        let market_data = new_shared_market_data();

        let feed = TychoFeed::new(create_test_config(), market_data.clone());
        let mut event_rx = feed.subscribe();

        let old_component_id = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let new_component_id = "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let old_token1 = create_test_token("0x0000000000000000000000000000000000000001", "OLD1");
        let old_token2 = create_test_token("0x0000000000000000000000000000000000000002", "OLD2");
        let new_token1 = create_test_token("0x1111111111111111111111111111111111111111", "NEW1");
        let new_token2 = create_test_token("0x2222222222222222222222222222222222222222", "NEW2");

        // First, add an old component
        let mut new_pairs = HashMap::new();
        new_pairs.insert(
            old_component_id.to_string(),
            create_test_component(old_component_id, vec![old_token1.clone(), old_token2.clone()]),
        );

        let update = Update::new(12345, HashMap::new(), new_pairs);
        feed.handle_tycho_message(update)
            .await
            .expect("Failed to add old component");

        // Verify the old component was added
        {
            let data = market_data.read().await;
            assert!(
                data.get_component(old_component_id)
                    .is_some(),
                "Old component should exist before removal"
            );
        }

        // Now add a new one and remove the old one in the same message
        let mut new_pairs = HashMap::new();
        new_pairs.insert(
            new_component_id.to_string(),
            create_test_component(new_component_id, vec![new_token1.clone(), new_token2.clone()]),
        );

        let mut removed_pairs = HashMap::new();
        removed_pairs.insert(
            old_component_id.to_string(),
            create_test_component(old_component_id, vec![old_token1.clone(), old_token2.clone()]),
        );

        let update = Update::new(12345, HashMap::new(), new_pairs).set_removed_pairs(removed_pairs);

        feed.handle_tycho_message(update)
            .await
            .expect("Failed to handle complex update");

        // Verify both operations succeeded
        {
            let data = market_data.read().await;
            assert!(
                data.get_component(new_component_id)
                    .is_some(),
                "New component should be added"
            );
            assert!(
                data.get_component(old_component_id)
                    .is_none(),
                "Old component should be removed"
            );
        }

        // Verify we receive both events in the correct order
        let event_1 = event_rx
            .try_recv()
            .expect("Should receive first event");
        let event_2 = event_rx
            .try_recv()
            .expect("Should receive second event");

        // First event: old component added
        assert_eq!(
            event_1,
            MarketEvent::MarketUpdated {
                added_components: FxHashMap::from_iter([(
                    old_component_id.to_string(),
                    vec![old_token1.address.clone(), old_token2.address.clone()].into()
                )]),
                removed_components: Vec::new(),
                updated_components: Vec::new(),
            }
        );

        // Second event: new component added AND old component removed
        assert_eq!(
            event_2,
            MarketEvent::MarketUpdated {
                added_components: FxHashMap::from_iter([(
                    new_component_id.to_string(),
                    vec![new_token1.address, new_token2.address].into()
                )]),
                removed_components: vec![old_component_id.to_string()],
                updated_components: Vec::new(),
            }
        );

        // Verify no more events
        match event_rx.try_recv() {
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {
                // Expected - no more events
            }
            Ok(event) => panic!("Unexpected extra event: {:?}", event),
            Err(e) => panic!("Unexpected error: {:?}", e),
        }
    }

    /// A `Delayed` protocol is measured against the newest `Ready` block in the same update.
    #[tokio::test]
    async fn test_handle_message_delayed_protocol() {
        let market_data = new_shared_market_data();
        let feed = TychoFeed::new(create_test_config(), market_data.clone());
        let _event_rx = feed.subscribe();
        let component_id = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let token1 = create_test_token("0x1111111111111111111111111111111111111111", "TKN1");
        let token2 = create_test_token("0x2222222222222222222222222222222222222222", "TKN2");
        let new_pairs = HashMap::from([(
            component_id.to_string(),
            create_test_component(component_id, vec![token1, token2]),
        )]);
        let sync_states = HashMap::from([
            (
                "uniswap_v2".to_string(),
                SynchronizerState::Delayed(BlockHeader {
                    number: 98,
                    timestamp: 1176,
                    ..Default::default()
                }),
            ),
            (
                "uniswap_v3".to_string(),
                SynchronizerState::Ready(BlockHeader {
                    number: 100,
                    timestamp: 1200,
                    ..Default::default()
                }),
            ),
        ]);
        let update = Update::new(100, HashMap::new(), new_pairs).set_sync_states(sync_states);

        feed.handle_tycho_message(update)
            .await
            .expect("message handled");

        assert!(market_data
            .read()
            .await
            .base_market_state()
            .is_unsynced_pool(component_id));
    }

    #[tokio::test]
    async fn test_handle_message_empty_update() {
        let config = create_test_config();
        let market_data = new_shared_market_data();

        let feed = TychoFeed::new(config, market_data.clone());
        let mut event_rx = feed.subscribe();

        // Send an empty update
        let update = Update::new(12345, HashMap::new(), HashMap::new());

        feed.handle_tycho_message(update)
            .await
            .expect("Failed to handle empty update");

        // Verify no event was broadcast (empty updates should not trigger events)
        match event_rx.try_recv() {
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {
                // Expected - no event should be broadcast for empty updates
            }
            Ok(_) => panic!("Should not broadcast event for empty update"),
            Err(e) => panic!("Unexpected error: {:?}", e),
        }
    }

    #[test]
    fn fynd_client_metadata_reports_version() {
        // Pins the wire key the server telemetry keys off and guarantees a non-empty version.
        let [(key, value)] = fynd_client_metadata();
        assert_eq!(key, "fynd_version");
        assert!(!value.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")] // Multi-thread needed because tycho decoder does some blocking operations
    #[ignore]
    async fn test_real_protocol_feed() {
        let tycho_api_key = env::var("TYCHO_API_KEY").expect("TYCHO_API_KEY must be set");
        let tycho_url = env::var("TYCHO_URL").expect("TYCHO_URL must be set");
        let config = TychoFeedConfig::new(
            tycho_url,
            Chain::Ethereum,
            Some(tycho_api_key),
            true, // Use TLS for real feed test
            vec!["uniswap_v2".to_string()],
            100.0,
        );

        let mut message_count = 5;

        let market_data = new_shared_market_data();

        let feed = TychoFeed::new(config, market_data.clone());
        let mut event_rx = feed.subscribe();

        // Start Tycho feed in background
        let feed_handle = tokio::spawn(async move {
            if let Err(e) = feed.run().await {
                panic!("Failed to run feed: {:?}", e);
            }
        });

        while let Ok(event) = event_rx.recv().await {
            message_count -= 1;
            if message_count == 0 {
                break;
            }
            dbg!(&event);
        }

        feed_handle.abort();
    }

    #[tokio::test(flavor = "multi_thread")] // Multi-thread needed because tycho decoder does some blocking operations
    #[ignore]
    async fn test_real_rfq_feed() {
        let tycho_api_key = env::var("TYCHO_API_KEY").expect("TYCHO_API_KEY must be set");
        let tycho_url = env::var("TYCHO_URL").expect("TYCHO_URL must be set");
        let config = TychoFeedConfig::new(
            tycho_url,
            Chain::Ethereum,
            Some(tycho_api_key),
            true, // Use TLS for real feed test
            vec!["rfq:bebop".to_string(), "rfq:hashflow".to_string()],
            100.0,
        );

        let mut message_count = 5;

        let market_data = new_shared_market_data();

        let feed = TychoFeed::new(config, market_data.clone());
        let mut event_rx = feed.subscribe();

        // Start Tycho feed in background
        let feed_handle = tokio::spawn(async move {
            if let Err(e) = feed.run().await {
                panic!("Failed to run feed: {:?}", e);
            }
        });

        while let Ok(event) = event_rx.recv().await {
            message_count -= 1;
            if message_count == 0 {
                break;
            }
            dbg!(&event);
        }

        feed_handle.abort();
    }

    #[tokio::test(flavor = "multi_thread")] // Multi-thread needed because tycho decoder does some blocking operations
    #[ignore]
    async fn test_real_combined_feed() {
        let tycho_api_key = env::var("TYCHO_API_KEY").expect("TYCHO_API_KEY must be set");
        let tycho_url = env::var("TYCHO_URL").expect("TYCHO_URL must be set");
        let config = TychoFeedConfig::new(
            tycho_url,
            Chain::Ethereum,
            Some(tycho_api_key),
            true, // Use TLS for real feed test
            vec!["rfq:bebop".to_string(), "rfq:hashflow".to_string(), "uniswap_v2".to_string()],
            100.0,
        );

        let mut message_count = 5;

        let market_data = new_shared_market_data();

        let feed = TychoFeed::new(config, market_data.clone());
        let mut event_rx = feed.subscribe();

        // Start Tycho feed in background
        let feed_handle = tokio::spawn(async move {
            if let Err(e) = feed.run().await {
                panic!("Failed to run feed: {:?}", e);
            }
        });

        while let Ok(event) = event_rx.recv().await {
            message_count -= 1;
            if message_count == 0 {
                break;
            }
            dbg!(&event);
        }

        feed_handle.abort();
    }

    #[test]
    fn handle_message_records_market_metrics() {
        use metrics_util::debugging::DebugValue;

        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime builds");

        let block_number = 12345u64;
        let block_timestamp = 1_700_000_000u64;

        metrics::with_local_recorder(&recorder, || {
            rt.block_on(async {
                let market_data = new_shared_market_data();
                let feed = TychoFeed::new(create_test_config(), market_data);
                let _event_rx = feed.subscribe();
                let token1 =
                    create_test_token("0x1111111111111111111111111111111111111111", "TKN1");
                let token2 =
                    create_test_token("0x2222222222222222222222222222222222222222", "TKN2");
                let component_id = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
                let mut new_pairs = HashMap::new();
                new_pairs.insert(
                    component_id.to_string(),
                    create_test_component(component_id, vec![token1, token2]),
                );
                // A Ready sync state sets the last updated block, which the
                // `market_current_block`/`market_last_update_timestamp_seconds` gauges report.
                let mut sync_states = HashMap::new();
                sync_states.insert(
                    "uniswap_v2".to_string(),
                    SynchronizerState::Ready(BlockHeader {
                        number: block_number,
                        timestamp: block_timestamp,
                        ..Default::default()
                    }),
                );
                let update = Update::new(block_number, HashMap::new(), new_pairs)
                    .set_sync_states(sync_states);
                feed.handle_tycho_message(update)
                    .await
                    .expect("message handled");
            })
        });

        let recorded = snapshotter.snapshot().into_vec();
        let find_value = |name: &str| -> &DebugValue {
            recorded
                .iter()
                .find(|(key, _, _, _)| key.key().name() == name)
                .map(|(_, _, _, value)| value)
                .unwrap_or_else(|| panic!("missing {name}, got {recorded:?}"))
        };
        let gauge_value = |name: &str| -> f64 {
            match find_value(name) {
                DebugValue::Gauge(value) => value.0,
                other => panic!("{name} is not a gauge: {other:?}"),
            }
        };

        assert_eq!(gauge_value("market_pools"), 1.0);
        assert_eq!(gauge_value("market_tokens"), 2.0);
        assert_eq!(gauge_value("market_current_block"), block_number as f64);
        assert_eq!(gauge_value("market_last_update_timestamp_seconds"), block_timestamp as f64);

        match find_value("market_update_duration_seconds") {
            DebugValue::Histogram(samples) => {
                assert!(!samples.is_empty(), "expected at least one recorded sample");
            }
            other => panic!("market_update_duration_seconds is not a histogram: {other:?}"),
        }
    }
}
