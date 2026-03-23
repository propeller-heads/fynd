//! Regression test for the block update race condition.
//!
//! These tests assert the DESIRED behavior: reads should never be blocked by
//! pending writes. They currently FAIL because `SharedMarketData` uses a
//! write-preferring `tokio::RwLock` that blocks new readers when a writer
//! is waiting.
//!
//! Once the fix is applied (e.g., ArcSwap or double-buffer pattern), these
//! tests should PASS.

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        time::{Duration, Instant},
    };

    use crate::feed::market_data::{SharedMarketData, SharedMarketDataRef};

    fn shared_market_data() -> SharedMarketDataRef {
        SharedMarketData::new_shared()
    }

    /// A new read() must complete within 10ms even when a write() is pending.
    ///
    /// Scenario: a worker is mid-solve (holding read lock), a new block arrives
    /// (TychoFeed requests write lock), and another worker starts a new solve
    /// (needs read lock). The new solve should NOT be blocked by the pending write.
    ///
    /// Currently FAILS: the write-preferring RwLock blocks new readers behind
    /// the pending writer, stalling the solve for ~2.8s in production.
    #[tokio::test]
    async fn read_not_blocked_by_pending_write() {
        let data = shared_market_data();

        // Worker 1 is mid-solve (holds read lock).
        let existing_reader = data.read().await;

        // Block update arrives — TychoFeed requests write lock.
        // Writer can't proceed yet (reader active), but is now queued.
        let data_for_writer = data.clone();
        tokio::spawn(async move {
            let _write_guard = data_for_writer.write().await;
            // Simulate 100ms state update (production takes ~2.8s).
            tokio::time::sleep(Duration::from_millis(100)).await;
        });

        // Give the writer time to enqueue.
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Worker 2 starts a new solve — needs read lock.
        let data_for_new_reader = data.clone();
        let reader_completed = Arc::new(AtomicBool::new(false));
        let reader_completed_clone = reader_completed.clone();

        tokio::spawn(async move {
            let _read_guard = data_for_new_reader.read().await;
            reader_completed_clone.store(true, Ordering::SeqCst);
        });

        // The new reader should complete within 20ms (not blocked by writer).
        tokio::time::sleep(Duration::from_millis(20)).await;

        assert!(
            reader_completed.load(Ordering::SeqCst),
            "New read() was blocked by a pending write(). \
             In production, this stalls solve requests for ~2.8s during block updates, \
             causing HTTP connections to drop (IncompleteMessage). \
             Fix: replace RwLock with ArcSwap or snapshot pattern."
        );

        drop(existing_reader);
    }

    /// Concurrent solve requests must not be serialized behind block updates.
    ///
    /// Scenario: 3 workers are mid-solve, a block update arrives, then 5 more
    /// solve requests come in. The new requests should start immediately, not
    /// wait for the block update to finish.
    ///
    /// Currently FAILS: all 5 new readers are blocked behind the pending writer,
    /// creating a latency spike on every block (~12s interval).
    #[tokio::test]
    async fn concurrent_reads_not_serialized_by_write() {
        let data = shared_market_data();

        // 3 workers mid-solve.
        let r1 = data.read().await;
        let r2 = data.read().await;
        let r3 = data.read().await;

        // Block update arrives.
        let data_for_writer = data.clone();
        tokio::spawn(async move {
            let _guard = data_for_writer.write().await;
            tokio::time::sleep(Duration::from_millis(50)).await;
        });

        tokio::time::sleep(Duration::from_millis(5)).await;

        // 5 new solve requests arrive.
        let new_reader_handles: Vec<_> = (0..5)
            .map(|_| {
                let d = data.clone();
                tokio::spawn(async move {
                    let t = Instant::now();
                    let _guard = d.read().await;
                    t.elapsed()
                })
            })
            .collect();

        // Release existing readers (lets the writer proceed).
        drop(r1);
        drop(r2);
        drop(r3);

        // Wait for everything to complete.
        let wait_times: Vec<Duration> = futures::future::join_all(new_reader_handles)
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect();

        // New readers should all complete in under 10ms (not blocked by writer).
        let blocked_count = wait_times
            .iter()
            .filter(|t| **t > Duration::from_millis(10))
            .count();

        assert_eq!(
            blocked_count, 0,
            "{} of 5 new read() calls were blocked by the pending write(). \
             Wait times: {:?}. \
             In production, this serializes all solve requests behind every block update. \
             Fix: replace RwLock with ArcSwap or snapshot pattern.",
            blocked_count, wait_times
        );
    }
}
