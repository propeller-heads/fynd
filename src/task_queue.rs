//! Task queue for distributing solve requests to workers.
//!
//! The queue sits between the HTTP handlers and the worker pool.
//! It provides backpressure and allows the HTTP layer to remain
//! responsive even when workers are busy.

use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::types::{Solution, SolveError, SolveTask};
use crate::types::solution::SolutionRequest;

/// Configuration for the task queue.
#[derive(Debug, Clone)]
pub struct TaskQueueConfig {
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
    sender: mpsc::Sender<SolveTask>,
}

impl TaskQueueHandle {
    /// Enqueues a solve request and returns a future that resolves to the result.
    ///
    /// Returns an error if the queue is full.
    pub async fn enqueue(&self, request: SolutionRequest) -> Result<Solution, SolveError> {
        // Create response channel
        let (response_tx, response_rx) = oneshot::channel();

        // Generate task ID
        let task_id = Uuid::new_v4();

        // Create task
        let task = SolveTask::new(task_id, request, response_tx);

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
    pub fn approximate_depth(&self) -> usize {
        self.sender.max_capacity() - self.sender.capacity()
    }

    /// Returns true if the queue is likely full.
    pub fn is_full(&self) -> bool {
        self.sender.capacity() == 0
    }
}

/// The task queue itself.
///
/// This is consumed when creating the worker pool.
pub struct TaskQueue {
    receiver: mpsc::Receiver<SolveTask>,
    handle: TaskQueueHandle,
}

impl TaskQueue {
    /// Creates a new task queue with the given configuration.
    pub fn new(config: TaskQueueConfig) -> Self {
        let (sender, receiver) = mpsc::channel(config.capacity);
        let handle = TaskQueueHandle { sender };

        Self { receiver, handle }
    }

    /// Returns a handle for enqueueing tasks.
    pub fn handle(&self) -> TaskQueueHandle {
        self.handle.clone()
    }

    /// Consumes the queue and returns the receiver.
    ///
    /// This is called when setting up the worker pool.
    pub fn into_receiver(self) -> mpsc::Receiver<SolveTask> {
        self.receiver
    }

    /// Splits the queue into handle and receiver.
    pub fn split(self) -> (TaskQueueHandle, mpsc::Receiver<SolveTask>) {
        (self.handle, self.receiver)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_task_queue_creation() {
        let config = TaskQueueConfig { capacity: 10 };
        let queue = TaskQueue::new(config);
        let handle = queue.handle();

        assert!(!handle.is_full());
    }
}
