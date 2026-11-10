use std::{sync::Arc, time::Duration};

use fynd_core::{
    derived::{types::TokenGasPrices, DerivedData, DerivedDataEvent, SharedDerivedDataRef},
    nexus::derived::{DerivedKind, DerivedSnapshot, PoolDepthEntry, SpotPriceEntry},
};
use fynd_nexus::derived::{snapshot::DerivedSnapshots, writer::DerivedSnapshotWriter};
use serde::de::DeserializeOwned;
use tokio::{sync::broadcast, time::timeout};
use tycho_simulation::{
    evm::tycho_models::Chain,
    tycho_core::{simulation::protocol_sim::Price, Bytes},
};

const DEADLINE: Duration = Duration::from_secs(5);

fn key(component: &str) -> (String, Bytes, Bytes) {
    (component.to_string(), Bytes::from([1; 20]), Bytes::from([2; 20]))
}

fn completion_event(computation_id: &'static str, block: u64) -> DerivedDataEvent {
    DerivedDataEvent::ComputationComplete { computation_id, block, failed_items: vec![] }
}

async fn set_depths(store: &SharedDerivedDataRef, block: u64) {
    let depths = [(key("a"), 5u8.into())]
        .into_iter()
        .collect();
    store
        .write()
        .await
        .set_component_depths(depths, vec![], block);
}

async fn fill_store(store: &SharedDerivedDataRef, block: u64) {
    let mut token_prices = TokenGasPrices::default();
    token_prices
        .insert(Bytes::from([1; 20]), Price { numerator: 3u8.into(), denominator: 7u8.into() });
    let spot_prices = [(key("a"), 1.5), (key("b"), f64::NAN)]
        .into_iter()
        .collect();
    store
        .write()
        .await
        .set_spot_prices(spot_prices, vec![], block);
    set_depths(store, block).await;
    store
        .write()
        .await
        .set_token_prices(token_prices, vec![], block);
}

/// A writer over `store` and the store it writes to.
fn writer_over(
    store: &SharedDerivedDataRef,
    capacity: usize,
) -> (DerivedSnapshotWriter, Arc<DerivedSnapshots>, broadcast::Sender<DerivedDataEvent>, Receiver) {
    let snapshots = Arc::new(DerivedSnapshots::new(Chain::Ethereum));
    let (events, receiver) = broadcast::channel(capacity);
    (
        DerivedSnapshotWriter::new(Arc::clone(store), Arc::clone(&snapshots)),
        snapshots,
        events,
        receiver,
    )
}

type Receiver = broadcast::Receiver<DerivedDataEvent>;

async fn wait_for_block(snapshots: &DerivedSnapshots, kind: DerivedKind, block: u64) {
    timeout(DEADLINE, async {
        while snapshots.block(kind) != Some(block) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the snapshot should reach the block before the timeout");
}

fn decode<E: DeserializeOwned>(
    snapshots: &DerivedSnapshots,
    kind: DerivedKind,
) -> DerivedSnapshot<E> {
    let current = snapshots.current(kind).unwrap();
    serde_json::from_slice(&current.json).unwrap()
}

/// Writes a store filled at block 7 and returns once every kind holds it.
async fn start_at_block_7(
    store: &SharedDerivedDataRef,
) -> (Arc<DerivedSnapshots>, broadcast::Sender<DerivedDataEvent>) {
    fill_store(store, 7).await;
    let (writer, snapshots, events, receiver) = writer_over(store, 16);
    writer.spawn(receiver);
    for kind in DerivedKind::ALL {
        wait_for_block(&snapshots, kind, 7).await;
    }
    (snapshots, events)
}

fn blocks(snapshots: &DerivedSnapshots) -> Vec<Option<u64>> {
    DerivedKind::ALL
        .map(|kind| snapshots.block(kind))
        .to_vec()
}

#[tokio::test]
async fn test_startup_writes_what_the_store_holds_without_non_finite_prices() {
    let store = DerivedData::new_shared();
    set_depths(&store, 3).await;
    let (writer, snapshots, _events, receiver) = writer_over(&store, 16);
    writer.spawn(receiver);
    wait_for_block(&snapshots, DerivedKind::PoolDepths, 3).await;
    assert_eq!(blocks(&snapshots), [None, Some(3), None], "only what was computed is written");

    fill_store(&store, 4).await;
    let (writer, snapshots, _events, receiver) = writer_over(&store, 16);
    writer.spawn(receiver);
    wait_for_block(&snapshots, DerivedKind::SpotPrices, 4).await;

    let spot = decode::<SpotPriceEntry>(&snapshots, DerivedKind::SpotPrices);
    assert_eq!((spot.entries.len(), spot.entries[0].price), (1, 1.5));
}

#[tokio::test]
async fn test_completed_computation_writes_its_own_kind_only() {
    let store = DerivedData::new_shared();
    let (snapshots, events) = start_at_block_7(&store).await;
    fill_store(&store, 9).await;

    events
        .send(completion_event("token_classification", 9))
        .unwrap();
    events
        .send(completion_event("spot_prices", 9))
        .unwrap();

    wait_for_block(&snapshots, DerivedKind::SpotPrices, 9).await;
    assert_eq!(blocks(&snapshots), [Some(9), Some(7), Some(7)]);
    let depths = decode::<PoolDepthEntry>(&snapshots, DerivedKind::PoolDepths);
    assert_eq!(depths.block, 7);
}

#[tokio::test]
async fn test_event_for_an_already_written_block_writes_nothing() {
    let store = DerivedData::new_shared();
    let (snapshots, events) = start_at_block_7(&store).await;
    let before = snapshots
        .current(DerivedKind::SpotPrices)
        .unwrap();
    set_depths(&store, 8).await;

    events
        .send(completion_event("spot_prices", 7))
        .unwrap();
    events
        .send(completion_event("pool_depths", 8))
        .unwrap();

    wait_for_block(&snapshots, DerivedKind::PoolDepths, 8).await;
    let after = snapshots
        .current(DerivedKind::SpotPrices)
        .unwrap();
    assert_eq!(
        before.json.as_ptr(),
        after.json.as_ptr(),
        "the block 7 snapshot was not re-encoded"
    );
}

#[tokio::test]
async fn test_lagged_receiver_writes_every_kind_again() {
    let store = DerivedData::new_shared();
    fill_store(&store, 7).await;
    let (writer, snapshots, events, receiver) = writer_over(&store, 2);
    writer.spawn(receiver);
    for kind in DerivedKind::ALL {
        wait_for_block(&snapshots, kind, 7).await;
    }
    fill_store(&store, 9).await;

    for _ in 0..5 {
        events
            .send(DerivedDataEvent::NewBlock { block: 9 })
            .unwrap();
    }

    for kind in DerivedKind::ALL {
        wait_for_block(&snapshots, kind, 9).await;
    }
}

#[tokio::test]
async fn test_writer_ends_when_the_event_channel_closes() {
    let store = DerivedData::new_shared();
    let (writer, _snapshots, events, receiver) = writer_over(&store, 16);
    let task = writer.spawn(receiver);

    drop(events);

    timeout(DEADLINE, task)
        .await
        .expect("the writer should end before the timeout")
        .unwrap();
}
