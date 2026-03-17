pub mod pool;
pub mod registry;
pub(crate) mod task_queue;
pub(crate) mod worker;

// Re-export only the task queue handle (users need this for WorkerPoolRouter)
pub use task_queue::TaskQueueHandle;
