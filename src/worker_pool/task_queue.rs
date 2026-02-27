//! Task queue for distributing solve requests to workers.
//!
//! The queue sits between the HTTP handlers and the worker pool.
//! It provides backpressure and allows the HTTP layer to remain
//! responsive even when workers are busy.

use tokio::sync::oneshot;
use uuid::Uuid;

use crate::{
    types::{internal::SolveTask, SingleOrderSolution, SolveError},
    Order,
};

/// Configuration for the task queue.
#[derive(Debug, Clone)]
pub(crate) struct TaskQueueConfig {
    /// Maximum number of pending tasks.
    pub capacity: usize,
}

impl Default for TaskQueueConfig {
    fn default() -> Self {
        Self { capacity: 1000 }
    }
}

/// Handle for enqueueing tasks.
///
/// This is cloned and shared with HTTP handlers.
#[derive(Clone)]
pub struct TaskQueueHandle {
    sender: async_channel::Sender<SolveTask>,
}

impl TaskQueueHandle {
    /// Enqueues a solve request and returns a future that resolves to the result.
    ///
    /// Returns an error if the queue is full.
    pub async fn enqueue(&self, order: Order) -> Result<SingleOrderSolution, SolveError> {
        // Create response channel
        let (response_tx, response_rx) = oneshot::channel();

        // Generate task ID
        let task_id = Uuid::new_v4();

        // Create task
        let task = SolveTask::new(task_id, order, response_tx);

        // Try to send
        self.sender
            .send(task)
            .await
            .map_err(|_| SolveError::QueueFull)?;

        // Wait for response
        response_rx
            .await
            .map_err(|_| SolveError::Internal("worker dropped response channel".to_string()))?
    }

    /// Returns the current approximate queue depth.
    ///
    /// Note: This is not exact due to the async nature of the queue.
    #[cfg(test)]
    pub fn approximate_depth(&self) -> usize {
        self.sender.len()
    }

    /// Returns true if the queue is likely full.
    #[cfg(test)]
    pub fn is_full(&self) -> bool {
        self.sender.is_full()
    }

    /// Creates a TaskQueueHandle from an existing sender.
    ///
    /// This is primarily useful for testing with mock channels.
    #[cfg(test)]
    pub fn from_sender(sender: async_channel::Sender<SolveTask>) -> Self {
        Self { sender }
    }
}

/// The task queue itself.
///
/// This is consumed when creating the worker pool.
pub(crate) struct TaskQueue {
    receiver: async_channel::Receiver<SolveTask>,
    handle: TaskQueueHandle,
}

impl TaskQueue {
    /// Creates a new task queue with the given configuration.
    pub fn new(config: TaskQueueConfig) -> Self {
        let (sender, receiver) = async_channel::bounded(config.capacity);
        let handle = TaskQueueHandle { sender };

        Self { receiver, handle }
    }

    /// Splits the queue into handle and receiver.
    pub fn split(self) -> (TaskQueueHandle, async_channel::Receiver<SolveTask>) {
        (self.handle, self.receiver)
    }

    /// Returns a handle for enqueueing tasks.
    #[cfg(test)]
    pub fn handle(&self) -> TaskQueueHandle {
        self.handle.clone()
    }

    /// Consumes the queue and returns the receiver.
    ///
    /// This is called when setting up the worker pool.
    #[cfg(test)]
    pub fn into_receiver(self) -> async_channel::Receiver<SolveTask> {
        self.receiver
    }
}

#[cfg(test)]
mod tests {
    use num_bigint::BigUint;
    use rstest::rstest;
    use tycho_simulation::tycho_core::models::Address;

    use super::*;
    use crate::types::{
        solution::{BlockInfo, Order, OrderSide, OrderSolution, SolutionStatus},
        SingleOrderSolution,
    };

    // -------------------------------------------------------------------------
    // Test Helpers
    // -------------------------------------------------------------------------

    fn make_address(byte: u8) -> Address {
        Address::from([byte; 20])
    }

    fn make_order() -> Order {
        Order {
            id: "test-order".to_string(),
            token_in: make_address(0x01),
            token_out: make_address(0x02),
            amount: BigUint::from(1000u64),
            side: OrderSide::Sell,
            sender: make_address(0xAA),
            receiver: None,
        }
    }

    fn make_single_solution() -> SingleOrderSolution {
        SingleOrderSolution {
            order: OrderSolution {
                order_id: "test-order".to_string(),
                status: SolutionStatus::Success,
                route: None,
                amount_in: BigUint::from(1000u64),
                amount_out: BigUint::from(990u64),
                gas_estimate: BigUint::from(100_000u64),
                price_impact_bps: None,
                amount_out_net_gas: BigUint::from(990u64),
                block: BlockInfo { number: 1, hash: "0x123".to_string(), timestamp: 1000 },
                encoding: None,
                algorithm: "test".to_string(),
            },
            solve_time_ms: 5,
        }
    }

    // -------------------------------------------------------------------------
    // TaskQueueConfig Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_config_default() {
        let config = TaskQueueConfig::default();
        assert_eq!(config.capacity, 1000);
    }

    #[rstest]
    #[case::small(1)]
    #[case::medium(100)]
    #[case::large(10_000)]
    fn test_config_custom_capacity(#[case] capacity: usize) {
        let config = TaskQueueConfig { capacity };
        assert_eq!(config.capacity, capacity);
    }

    // -------------------------------------------------------------------------
    // TaskQueue Creation Tests
    // -------------------------------------------------------------------------

    #[rstest]
    #[case::capacity_1(1)]
    #[case::capacity_10(10)]
    #[case::capacity_100(100)]
    fn test_queue_creation(#[case] capacity: usize) {
        let config = TaskQueueConfig { capacity };
        let queue = TaskQueue::new(config);
        let handle = queue.handle();

        assert!(!handle.is_full());
        assert_eq!(handle.approximate_depth(), 0);
    }

    #[test]
    fn test_queue_handle_is_cloneable() {
        let queue = TaskQueue::new(TaskQueueConfig { capacity: 10 });
        let handle1 = queue.handle();
        let handle2 = handle1.clone();

        // Both handles should report same state
        assert_eq!(handle1.approximate_depth(), handle2.approximate_depth());
        assert_eq!(handle1.is_full(), handle2.is_full());
    }

    #[test]
    fn test_queue_into_receiver() {
        let queue = TaskQueue::new(TaskQueueConfig { capacity: 10 });
        let _handle = queue.handle();
        let _receiver = queue.into_receiver();
        // Queue is consumed - receiver is ready for worker pool
    }

    #[test]
    fn test_queue_split() {
        let queue = TaskQueue::new(TaskQueueConfig { capacity: 10 });
        let (handle, _receiver) = queue.split();

        assert!(!handle.is_full());
        assert_eq!(handle.approximate_depth(), 0);
    }

    // -------------------------------------------------------------------------
    // TaskQueueHandle Tests
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_enqueue_and_receive_response() {
        let queue = TaskQueue::new(TaskQueueConfig { capacity: 10 });
        let handle = queue.handle();
        let receiver = queue.into_receiver();

        // Spawn a "worker" that responds to the task
        let worker = tokio::spawn(async move {
            let task = receiver
                .recv()
                .await
                .expect("should receive task");
            assert_eq!(task.order.id, "test-order");
            task.respond(Ok(make_single_solution()));
        });

        // Enqueue an order
        let result = handle.enqueue(make_order()).await;

        worker
            .await
            .expect("worker should complete");
        let solution = result.expect("should get solution");
        assert_eq!(solution.solve_time_ms, 5);
    }

    #[tokio::test]
    async fn test_enqueue_receives_error_response() {
        let queue = TaskQueue::new(TaskQueueConfig { capacity: 10 });
        let handle = queue.handle();
        let receiver = queue.into_receiver();

        let worker = tokio::spawn(async move {
            let task = receiver
                .recv()
                .await
                .expect("should receive task");
            task.respond(Err(SolveError::NoRouteFound { order_id: "test".to_string() }));
        });

        let result = handle.enqueue(make_order()).await;

        worker
            .await
            .expect("worker should complete");
        assert!(matches!(result, Err(SolveError::NoRouteFound { .. })));
    }

    #[tokio::test]
    async fn test_enqueue_error_when_receiver_dropped() {
        let queue = TaskQueue::new(TaskQueueConfig { capacity: 10 });
        let handle = queue.handle();
        let receiver = queue.into_receiver();

        // Worker receives task but drops it without responding
        let worker = tokio::spawn(async move {
            let task = receiver
                .recv()
                .await
                .expect("should receive task");
            drop(task); // Drop without responding
        });

        let result = handle.enqueue(make_order()).await;

        worker
            .await
            .expect("worker should complete");
        assert!(matches!(result, Err(SolveError::Internal(_))));
    }

    #[tokio::test]
    async fn test_enqueue_queue_full_error() {
        let queue = TaskQueue::new(TaskQueueConfig { capacity: 10 });
        let handle = queue.handle();
        let receiver = queue.into_receiver();

        // Drop receiver to close channel
        drop(receiver);

        let result = handle.enqueue(make_order()).await;
        assert!(matches!(result, Err(SolveError::QueueFull)));
    }

    // -------------------------------------------------------------------------
    // Queue Depth and Full Detection Tests
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_approximate_depth_increases_with_pending_tasks() {
        let queue = TaskQueue::new(TaskQueueConfig { capacity: 10 });
        let handle = queue.handle();
        let _receiver = queue.into_receiver(); // Keep receiver alive but don't consume

        // Create a oneshot and send a task
        let (response_tx, _response_rx) = oneshot::channel();
        let task = SolveTask::new(Uuid::new_v4(), make_order(), response_tx);

        handle
            .sender
            .send(task)
            .await
            .expect("should send");

        assert_eq!(handle.approximate_depth(), 1);

        // Send another
        let (response_tx2, _response_rx2) = oneshot::channel();
        let task2 = SolveTask::new(Uuid::new_v4(), make_order(), response_tx2);
        handle
            .sender
            .send(task2)
            .await
            .expect("should send");

        assert_eq!(handle.approximate_depth(), 2);
    }

    #[rstest]
    #[case::capacity_1(1)]
    #[case::capacity_5(5)]
    #[case::capacity_10(10)]
    #[tokio::test]
    async fn test_is_full_when_at_capacity(#[case] capacity: usize) {
        let queue = TaskQueue::new(TaskQueueConfig { capacity });
        let handle = queue.handle();
        let _receiver = queue.into_receiver();

        // Fill the queue
        for _ in 0..capacity {
            let (response_tx, _response_rx) = oneshot::channel();
            let task = SolveTask::new(Uuid::new_v4(), make_order(), response_tx);
            handle
                .sender
                .send(task)
                .await
                .expect("should send");
        }

        assert!(handle.is_full());
        assert_eq!(handle.approximate_depth(), capacity);
    }

    #[tokio::test]
    async fn test_is_full_becomes_false_after_task_consumed() {
        let queue = TaskQueue::new(TaskQueueConfig { capacity: 2 });
        let handle = queue.handle();
        let receiver = queue.into_receiver();

        // Fill queue
        let (tx1, _rx1) = oneshot::channel();
        let (tx2, _rx2) = oneshot::channel();
        handle
            .sender
            .send(SolveTask::new(Uuid::new_v4(), make_order(), tx1))
            .await
            .unwrap();
        handle
            .sender
            .send(SolveTask::new(Uuid::new_v4(), make_order(), tx2))
            .await
            .unwrap();

        assert!(handle.is_full());

        // Consume one task
        let _task = receiver.recv().await.unwrap();

        // Queue should no longer be full
        assert!(!handle.is_full());
        assert_eq!(handle.approximate_depth(), 1);
    }

    // -------------------------------------------------------------------------
    // Concurrent Operation Tests
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_multiple_handles_can_enqueue_concurrently() {
        let queue = TaskQueue::new(TaskQueueConfig { capacity: 10 });
        let handle1 = queue.handle();
        let handle2 = queue.handle();
        let receiver = queue.into_receiver();

        // Spawn worker that processes multiple tasks
        let worker = tokio::spawn(async move {
            for _ in 0..2 {
                let task = receiver
                    .recv()
                    .await
                    .expect("should receive task");
                task.respond(Ok(make_single_solution()));
            }
        });

        // Enqueue from both handles concurrently
        let (result1, result2) =
            tokio::join!(handle1.enqueue(make_order()), handle2.enqueue(make_order()),);

        worker
            .await
            .expect("worker should complete");

        assert!(result1.is_ok());
        assert!(result2.is_ok());
    }

    #[tokio::test]
    async fn test_task_id_is_unique_per_enqueue() {
        let queue = TaskQueue::new(TaskQueueConfig { capacity: 10 });
        let handle = queue.handle();
        let receiver = queue.into_receiver();

        // Spawn workers to collect task IDs
        let collector = tokio::spawn(async move {
            let task1 = receiver.recv().await.unwrap();
            let id1 = task1.id;
            task1.respond(Ok(make_single_solution()));

            let task2 = receiver.recv().await.unwrap();
            let id2 = task2.id;
            task2.respond(Ok(make_single_solution()));

            (id1, id2)
        });

        // Enqueue two orders
        let _ = handle.enqueue(make_order()).await;
        let _ = handle.enqueue(make_order()).await;

        let (id1, id2) = collector
            .await
            .expect("collector should complete");
        assert_ne!(id1, id2, "Task IDs should be unique");
    }

    // -------------------------------------------------------------------------
    // SolveTask Tests (internal type used by queue)
    // -------------------------------------------------------------------------

    #[test]
    fn test_solve_task_wait_time_increases() {
        let (response_tx, _response_rx) = oneshot::channel();
        let task = SolveTask::new(Uuid::new_v4(), make_order(), response_tx);

        let wait1 = task.wait_time();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let wait2 = task.wait_time();

        assert!(wait2 > wait1);
    }

    #[tokio::test]
    async fn test_solve_task_respond_delivers_result() {
        let (response_tx, response_rx) = oneshot::channel();
        let task = SolveTask::new(Uuid::new_v4(), make_order(), response_tx);

        task.respond(Ok(make_single_solution()));

        let result = response_rx
            .await
            .expect("should receive response");
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_solve_task_respond_delivers_error() {
        let (response_tx, response_rx) = oneshot::channel();
        let task = SolveTask::new(Uuid::new_v4(), make_order(), response_tx);

        task.respond(Err(SolveError::Timeout { elapsed_ms: 100 }));

        let result = response_rx
            .await
            .expect("should receive response");
        assert!(matches!(result, Err(SolveError::Timeout { elapsed_ms: 100 })));
    }
}
