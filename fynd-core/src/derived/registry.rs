//! Registry support for driving derived computations dynamically.
//!
//! `ComputationManager` keeps its computations as a `Vec<Box<dyn ErasedComputation>>`
//! rather than named fields, so a computation can be added without touching the
//! manager's control flow (the computation analogue of the worker `with_algorithm`
//! seam).
//!
//! [`DerivedComputation`] is the trait authors implement. It carries an associated
//! `Output` type and a `const ID`, so it is not object-safe. [`ErasedComputation`] is
//! the object-safe view the manager stores; the blanket impl below provides it for
//! free to every [`DerivedComputation`], so authors only ever write the typed trait.
//!
//! # Adding a New Computation
//!
//! 1. Implement [`DerivedComputation`] for your type, declaring
//!    [`requirements`](DerivedComputation::requirements) for any upstream computations it reads and
//!    [`persist`](DerivedComputation::persist) if it needs storage beyond the default slot.
//! 2. Register it with `ComputationManager::register`.
//!
//! The manager orders it by its requirements and runs it each block; this module
//! needs no change.

use async_trait::async_trait;
use tracing::{debug, warn};

use super::{
    computation::{ComputationId, ComputationRequirements, DerivedComputation, FailedItem},
    error::ComputationError,
    manager::{ChangedComponents, SharedDerivedDataRef},
    store::DerivedData,
};
use crate::feed::market_data::MarketData;

/// A computed result ready to be written to the store.
///
/// `failed_items` is surfaced for event emission; `persist` writes the typed output
/// into the store, capturing the computation's `Output` type so the manager can
/// store it without naming the type.
pub(crate) struct ComputedWrite {
    /// Partial failures from this run, for `ComputationComplete` events.
    pub(crate) failed_items: Vec<FailedItem>,
    /// Writes the computed output into the store under the computation's id.
    pub(crate) persist: Box<dyn FnOnce(&mut DerivedData) + Send>,
}

/// Object-safe form of [`DerivedComputation`].
///
/// Lets heterogeneous computations live in one `Vec<Box<dyn ErasedComputation>>`
/// and be driven uniformly by the manager. Obtained for free for any
/// [`DerivedComputation`] via the blanket impl below.
#[async_trait]
pub(crate) trait ErasedComputation: Send + Sync {
    /// The wrapped computation's [`DerivedComputation::ID`].
    fn id(&self) -> ComputationId;

    /// The wrapped computation's declared upstream requirements, for ordering.
    fn requirements(&self) -> ComputationRequirements;

    /// Runs the computation and returns its result paired with a typed persister.
    ///
    /// The output type is erased into the `persist` closure, so the caller can drive
    /// computation and storage without naming the concrete `Output`.
    async fn compute_erased(
        &self,
        market: &MarketData,
        store: &SharedDerivedDataRef,
        changed: &ChangedComponents,
        block: u64,
    ) -> Result<ComputedWrite, ComputationError>;
}

#[async_trait]
impl<C> ErasedComputation for C
where
    C: DerivedComputation,
{
    fn id(&self) -> ComputationId {
        C::ID
    }

    fn requirements(&self) -> ComputationRequirements {
        DerivedComputation::requirements(self)
    }

    async fn compute_erased(
        &self,
        market: &MarketData,
        store: &SharedDerivedDataRef,
        changed: &ChangedComponents,
        block: u64,
    ) -> Result<ComputedWrite, ComputationError> {
        let output = C::compute(self, market, store, changed).await?;
        if output.has_failures() {
            warn!(
                computation = C::ID,
                failed = output.failed_items.len(),
                "computation completed with partial failures"
            );
            for item in &output.failed_items {
                debug!(computation = C::ID, key = %item.key, error = %item.error, "failed item");
            }
        }
        let failed_items = output.failed_items.clone();
        let is_full_recompute = changed.is_full_recompute;
        Ok(ComputedWrite {
            failed_items,
            persist: Box::new(move |store: &mut DerivedData| {
                C::persist(store, output, block, is_full_recompute);
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::derived::computation::ComputationOutput;

    #[derive(Clone, Debug, PartialEq)]
    struct DummyData(u32);

    struct DummyComputation;

    #[async_trait]
    impl DerivedComputation for DummyComputation {
        type Output = DummyData;
        const ID: ComputationId = "dummy";

        async fn compute(
            &self,
            _market: &MarketData,
            _store: &SharedDerivedDataRef,
            _changed: &ChangedComponents,
        ) -> Result<ComputationOutput<Self::Output>, ComputationError> {
            Ok(ComputationOutput::success(DummyData(42)))
        }
    }

    // The associated-type, associated-const trait coerces into a boxed object-safe
    // trait object via the blanket impl, and the registry holds it heterogeneously.
    #[test]
    fn typed_computation_coerces_into_erased_registry() {
        let registry: Vec<Box<dyn ErasedComputation>> = vec![Box::new(DummyComputation)];

        assert_eq!(registry[0].id(), "dummy");
        assert!(!registry[0]
            .requirements()
            .has_requirements());
    }

    // The default `persist` writes the output into the generic slot, so a computation
    // with no bespoke storage needs no change to `DerivedData` (the downstream path).
    #[test]
    fn default_persist_writes_to_generic_slot() {
        let mut store = DerivedData::new();

        <DummyComputation as DerivedComputation>::persist(
            &mut store,
            ComputationOutput::success(DummyData(7)),
            100,
            true,
        );

        assert_eq!(store.output::<DummyData>(DummyComputation::ID), Some(&DummyData(7)));
        assert_eq!(store.output_block(DummyComputation::ID), Some(100));
    }
}
