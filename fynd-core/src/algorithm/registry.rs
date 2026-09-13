//! Custom [`Algorithm`] implementations, held by name.
//!
//! The built-in algorithms are looked up by a fixed list of names that a crate outside this one
//! cannot add to. A registry carries the ones a caller brought, so a pool configuration naming one
//! resolves the same way it resolves a built-in.
//!
//! ```ignore
//! // In the deployment's binary:
//! let algorithms = AlgorithmRegistry::new().with_algorithm("my_algo", MyAlgorithm::new)?;
//! let solver = FyndBuilder::new(..).with_algorithms(algorithms).build()?;
//! ```
//! ```toml
//! # In worker_pools.toml, exactly as a built-in is named:
//! [pools.mine]
//! algorithm = "my_algo"
//! ```

use std::{collections::HashMap, sync::Arc};

use crate::{
    algorithm::{Algorithm, AlgorithmConfig},
    feed::events::MarketEventHandler,
    graph::EdgeWeightUpdaterWithDerived,
    worker_pool::{
        pool::WorkerPoolBuilder,
        registry::{UnknownAlgorithmError, AVAILABLE_ALGORITHMS},
    },
};

/// Points a pool at one registered algorithm.
///
/// Behind an `Arc` so the registry can be cloned: a deployment that both serves and benchmarks the
/// same algorithms registers them once.
type Configure = Arc<dyn Fn(WorkerPoolBuilder) -> WorkerPoolBuilder + Send + Sync>;

/// Registering an algorithm under a name that is already taken.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum RegisterAlgorithmError {
    /// The name belongs to an algorithm that ships with this crate.
    #[error("'{name}' is a built-in algorithm; registering it would replace the shipped one")]
    ShadowsBuiltIn {
        /// The name that clashes.
        name: String,
    },

    /// The name was already registered by an earlier call.
    #[error("'{name}' is already registered")]
    AlreadyRegistered {
        /// The name that clashes.
        name: String,
    },
}

/// Algorithms a caller brought, keyed by the name a pool configuration uses to ask for one.
///
/// Empty by default, which is every deployment running only the built-ins.
#[derive(Default, Clone)]
pub struct AlgorithmRegistry {
    by_name: HashMap<String, Configure>,
}

impl AlgorithmRegistry {
    /// A registry holding nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `factory` under `name`, so a pool asking for that algorithm is served by it.
    ///
    /// The factory is called once per worker thread.
    ///
    /// # Errors
    ///
    /// [`RegisterAlgorithmError::ShadowsBuiltIn`] when `name` is one this crate ships, and
    /// [`RegisterAlgorithmError::AlreadyRegistered`] when an earlier call took it. Both are
    /// refused rather than resolved silently: which algorithm a pool runs is not something to
    /// decide by registration order.
    pub fn with_algorithm<A, F>(
        mut self,
        name: impl Into<String>,
        factory: F,
    ) -> Result<Self, RegisterAlgorithmError>
    where
        A: Algorithm + 'static,
        A::GraphManager: MarketEventHandler + EdgeWeightUpdaterWithDerived + 'static,
        F: Fn(AlgorithmConfig) -> A + Clone + Send + Sync + 'static,
    {
        let name = name.into();
        if AVAILABLE_ALGORITHMS.contains(&name.as_str()) {
            return Err(RegisterAlgorithmError::ShadowsBuiltIn { name });
        }
        if self.by_name.contains_key(&name) {
            return Err(RegisterAlgorithmError::AlreadyRegistered { name });
        }
        let registered = name.clone();
        self.by_name.insert(
            name,
            Arc::new(move |builder: WorkerPoolBuilder| {
                builder.with_algorithm(registered.clone(), factory.clone())
            }),
        );
        Ok(self)
    }

    /// The names this registry can serve, for an error that has to say what was available.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.by_name.keys().map(String::as_str)
    }

    /// Points `builder` at the algorithm `name` asks for.
    ///
    /// A registered name is served from here, a built-in one from the fixed list.
    ///
    /// # Errors
    ///
    /// [`UnknownAlgorithmError`] when neither holds the name. Raised here rather than at spawn
    /// time because this is where both sets are known, so the message can list what a caller
    /// registered as well as what ships.
    pub(crate) fn configure(
        &self,
        name: &str,
        builder: WorkerPoolBuilder,
    ) -> Result<WorkerPoolBuilder, UnknownAlgorithmError> {
        if let Some(configure) = self.by_name.get(name) {
            return Ok(configure(builder));
        }
        if AVAILABLE_ALGORITHMS.contains(&name) {
            return Ok(builder.algorithm(name));
        }
        Err(UnknownAlgorithmError::of(
            name,
            self.names()
                .map(str::to_string)
                .collect(),
        ))
    }
}

impl std::fmt::Debug for AlgorithmRegistry {
    /// Names only: the factories behind them cannot be printed.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AlgorithmRegistry")
            .field("names", &self.by_name.keys().collect::<Vec<_>>())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithm::{most_liquid::MostLiquidAlgorithm, AlgorithmConfig};

    /// `MostLiquidAlgorithm::with_config` is fallible; a factory is not.
    fn most_liquid(config: AlgorithmConfig) -> MostLiquidAlgorithm {
        MostLiquidAlgorithm::with_config(config).expect("the default config is valid")
    }

    fn registry_with(name: &str) -> AlgorithmRegistry {
        AlgorithmRegistry::new()
            .with_algorithm(name, most_liquid)
            .expect("the name is neither built in nor taken")
    }

    #[test]
    fn test_registry_is_empty_by_default() {
        assert_eq!(AlgorithmRegistry::new().names().count(), 0);
    }

    /// Which algorithm a pool runs is not something to settle by registration order.
    #[test]
    fn test_registry_refuses_a_name_already_registered() {
        let error = registry_with("mine")
            .with_algorithm("mine", most_liquid)
            .expect_err("the name is taken");

        assert_eq!(error, RegisterAlgorithmError::AlreadyRegistered { name: "mine".to_string() });
    }

    /// Shadowing a shipped algorithm would change what a production pool runs, invisibly.
    #[test]
    fn test_registry_refuses_a_built_in_name() {
        let error = AlgorithmRegistry::new()
            .with_algorithm("most_liquid", most_liquid)
            .expect_err("the name ships with this crate");

        assert_eq!(
            error,
            RegisterAlgorithmError::ShadowsBuiltIn { name: "most_liquid".to_string() }
        );
    }

    #[test]
    fn test_configure_serves_a_registered_name() {
        let Ok(builder) = registry_with("brought_from_outside")
            .configure("brought_from_outside", WorkerPoolBuilder::new())
        else {
            panic!("a registered name is served");
        };

        assert!(builder.serves_custom_algorithm(), "a registered name is served by its factory");
    }

    #[test]
    fn test_configure_leaves_a_built_in_name_to_the_built_in() {
        let Ok(builder) = registry_with("mine").configure("water_fill", WorkerPoolBuilder::new())
        else {
            panic!("a built-in name is served");
        };

        assert!(
            !builder.serves_custom_algorithm(),
            "a built-in name is not served from the registry"
        );
    }

    /// The message has to name what the deployment could actually have served, or an operator who
    /// mistypes a registered name goes looking in the wrong place.
    #[test]
    fn test_configure_rejects_a_name_neither_side_holds() {
        let Err(error) = registry_with("brought_from_outside")
            .configure("brought_from_outsid", WorkerPoolBuilder::new())
        else {
            panic!("a name neither side holds must be refused");
        };

        let message = error.to_string();
        assert!(message.contains("brought_from_outside"), "lists the registered name: {message}");
        assert!(message.contains("water_fill"), "lists the built-ins too: {message}");
    }
}
