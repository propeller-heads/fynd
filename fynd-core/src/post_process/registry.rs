//! Post-processor factories, registered by name.

use std::{collections::HashMap, sync::Arc};

use crate::post_process::{
    exclusive::ExclusiveSwapReplacer, rfq::RfqSwapReplacer, RoutePostProcessor,
};

/// Builds one post-processor. Called once per worker.
type Factory = Arc<dyn Fn() -> Box<dyn RoutePostProcessor> + Send + Sync>;

/// Builds one built-in post-processor.
type BuiltInFactory = fn() -> Box<dyn RoutePostProcessor>;

/// The post-processors that ship with fynd-core, each with its name and its factory. A registry
/// never holds them: `resolve` finds them.
const BUILT_INS: &[(&str, BuiltInFactory)] = &[
    (RfqSwapReplacer::NAME, || Box::<RfqSwapReplacer>::default()),
    (ExclusiveSwapReplacer::NAME, || Box::<ExclusiveSwapReplacer>::default()),
];

/// The factory of the built-in post-processor `name`, if there is one.
fn find_built_in(name: &str) -> Option<Factory> {
    BUILT_INS
        .iter()
        .find(|(built_in_name, _)| *built_in_name == name)
        .map(|(_, factory)| Arc::new(*factory) as Factory)
}

/// Registering a post-processor under a name that is already taken, or that a built-in uses.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
#[error("post-processor '{name}' is already registered or built in")]
pub struct PostProcessorAlreadyRegistered {
    /// The name that clashes.
    pub name: String,
}

/// A pool configuration listed a post-processor that no registry holds.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
#[error(
    "worker pool '{worker_pool}' lists unknown post-processor '{post_processor_name}'. \
     Available: {}",
    available_post_processors.join(", ")
)]
pub struct UnknownPostProcessorError {
    worker_pool: String,
    post_processor_name: String,
    available_post_processors: Vec<String>,
}

impl UnknownPostProcessorError {
    /// The post-processor name that was not found.
    pub fn post_processor_name(&self) -> &str {
        &self.post_processor_name
    }

    /// The worker pool that listed the name.
    pub fn worker_pool(&self) -> &str {
        &self.worker_pool
    }
}

/// A pool configuration listed `exclusive_swap_replacer` before another post-processor.
///
/// The replacer commits the route to a price. A post-processor that runs after it builds a new
/// route without the commitment, and the worker then rejects the processed route.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
#[error(
    "worker pool '{worker_pool}' lists post-processor '{post_processor_name}' before \
     '{followed_by}'. List {post_processor_name} last"
)]
pub struct PostProcessorOrderError {
    worker_pool: String,
    post_processor_name: String,
    followed_by: String,
}

impl PostProcessorOrderError {
    /// The post-processor that must come last.
    pub fn post_processor_name(&self) -> &str {
        &self.post_processor_name
    }

    /// The worker pool that listed the names.
    pub fn worker_pool(&self) -> &str {
        &self.worker_pool
    }
}

/// Checks that `names`, the post-processors `worker_pool` lists, put `exclusive_swap_replacer`
/// last.
///
/// # Errors
///
/// [`PostProcessorOrderError`] when any name follows `exclusive_swap_replacer`, which includes
/// listing it twice.
pub(crate) fn check_post_processor_order(
    worker_pool: &str,
    names: &[String],
) -> Result<(), PostProcessorOrderError> {
    for (name, followed_by) in names.iter().zip(names.iter().skip(1)) {
        if name == ExclusiveSwapReplacer::NAME {
            return Err(PostProcessorOrderError {
                worker_pool: worker_pool.to_string(),
                post_processor_name: name.clone(),
                followed_by: followed_by.clone(),
            });
        }
    }
    Ok(())
}

/// The factories behind the post-processor names of one pool, in config order.
#[derive(Clone, Default)]
pub(crate) struct PostProcessorFactories(Vec<(String, Factory)>);

impl PostProcessorFactories {
    /// One new instance of each post-processor with its registered name, in config order.
    pub(crate) fn build(&self) -> Vec<(String, Box<dyn RoutePostProcessor>)> {
        self.0
            .iter()
            .map(|(name, factory)| (name.clone(), factory()))
            .collect()
    }
}

impl std::fmt::Debug for PostProcessorFactories {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(self.0.iter().map(|(name, _)| name))
            .finish()
    }
}

/// Custom post-processor factories, keyed by the name a pool configuration lists. Empty by
/// default. The built-in post-processors need no registration.
#[derive(Default, Clone)]
pub struct PostProcessorRegistry {
    by_name: HashMap<String, Factory>,
}

impl PostProcessorRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `factory` under `name`, so a pool listing that name runs the post-processor.
    ///
    /// The factory runs once per worker, on the worker thread.
    ///
    /// # Errors
    ///
    /// [`PostProcessorAlreadyRegistered`] when an earlier call took `name`, or a built-in
    /// post-processor uses it.
    pub fn with_post_processor<P, F>(
        mut self,
        name: impl Into<String>,
        factory: F,
    ) -> Result<Self, PostProcessorAlreadyRegistered>
    where
        P: RoutePostProcessor + 'static,
        F: Fn() -> P + Send + Sync + 'static,
    {
        let name = name.into();
        if find_built_in(&name).is_some() || self.by_name.contains_key(&name) {
            return Err(PostProcessorAlreadyRegistered { name });
        }
        self.by_name
            .insert(name, Arc::new(move || Box::new(factory())));
        Ok(self)
    }

    /// The custom names this registry holds, in no fixed order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.by_name.keys().map(String::as_str)
    }

    /// The factories behind `names`, in the same order. `worker_pool` is the pool that listed them.
    ///
    /// # Errors
    ///
    /// [`UnknownPostProcessorError`] for the first name that is neither built in nor registered.
    /// Raised when the solver is built, so a misspelled name fails at start and not at the first
    /// quote.
    pub(crate) fn resolve(
        &self,
        worker_pool: &str,
        names: &[String],
    ) -> Result<PostProcessorFactories, UnknownPostProcessorError> {
        let mut factories = Vec::with_capacity(names.len());
        for name in names {
            let Some(factory) = self
                .by_name
                .get(name)
                .cloned()
                .or_else(|| find_built_in(name))
            else {
                let mut available_post_processors: Vec<String> = self
                    .names()
                    .chain(BUILT_INS.iter().map(|(name, _)| *name))
                    .map(str::to_string)
                    .collect();
                available_post_processors.sort();
                return Err(UnknownPostProcessorError {
                    worker_pool: worker_pool.to_string(),
                    post_processor_name: name.clone(),
                    available_post_processors,
                });
            };
            factories.push((name.clone(), factory));
        }
        Ok(PostProcessorFactories(factories))
    }
}

impl std::fmt::Debug for PostProcessorRegistry {
    /// Names only: the factories behind them cannot be printed.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut names: Vec<&str> = self.names().collect();
        names.sort_unstable();
        f.debug_struct("PostProcessorRegistry")
            .field("names", &names)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;

    use super::*;
    use crate::{
        feed::events::{EventError, MarketEvent, MarketEventHandler},
        post_process::PostProcessRequest,
        types::RouteResult,
    };

    struct Noop;

    #[async_trait]
    impl MarketEventHandler for Noop {
        async fn handle_event(&mut self, _event: &MarketEvent) -> Result<(), EventError> {
            Ok(())
        }
    }

    #[async_trait]
    impl RoutePostProcessor for Noop {
        async fn process(
            &self,
            _request: &PostProcessRequest<'_>,
            _route: &RouteResult,
        ) -> Option<RouteResult> {
            None
        }
    }

    fn registry_with_post_processor(name: &str) -> PostProcessorRegistry {
        PostProcessorRegistry::new()
            .with_post_processor(name, || Noop)
            .expect("the name is free")
    }

    #[test]
    fn test_registry_duplicate_name() {
        let error = registry_with_post_processor("mine")
            .with_post_processor("mine", || Noop)
            .expect_err("the name is taken");

        assert_eq!(error, PostProcessorAlreadyRegistered { name: "mine".to_string() });
    }

    #[test]
    fn test_registry_debug() {
        let printed = format!("{:?}", registry_with_post_processor("mine"));

        assert_eq!(printed, r#"PostProcessorRegistry { names: ["mine"] }"#);
    }

    #[test]
    fn test_resolve_empty_names() {
        let factories = PostProcessorRegistry::new()
            .resolve("pool", &[])
            .expect("nothing to look up");

        assert!(factories.build().is_empty(), "expected no post-processors");
    }

    #[test]
    fn test_resolve_keeps_config_order() {
        let registry = registry_with_post_processor("a")
            .with_post_processor("b", || Noop)
            .expect("the name is free");

        let factories = registry
            .resolve("pool", &["b".to_string(), "a".to_string()])
            .expect("both names are registered");

        let names: Vec<String> = factories
            .build()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, ["b", "a"]);
    }

    #[test]
    fn test_resolve_builds_an_instance_per_build_call() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&calls);
        let registry = PostProcessorRegistry::new()
            .with_post_processor("a", move || {
                counted.fetch_add(1, Ordering::SeqCst);
                Noop
            })
            .expect("the name is free");
        let factories = registry
            .resolve("pool", &["a".to_string()])
            .expect("the name is registered");

        factories.build();
        factories.build();

        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn test_resolve_unknown_name() {
        let error = registry_with_post_processor("zeta")
            .with_post_processor("alpha", || Noop)
            .expect("the name is free")
            .resolve("main", &["zeta".to_string(), "missing".to_string()])
            .expect_err("the second name is not registered");

        assert_eq!(error.post_processor_name(), "missing");
        assert_eq!(error.worker_pool(), "main");
        assert_eq!(
            error.to_string(),
            "worker pool 'main' lists unknown post-processor 'missing'. Available: alpha, \
             exclusive_swap_replacer, rfq_swap_replacer, zeta"
        );
    }

    #[test]
    fn test_resolve_unknown_name_empty_registry() {
        let error = PostProcessorRegistry::new()
            .resolve("main", &["x".to_string()])
            .expect_err("nothing is registered");

        assert!(
            error
                .to_string()
                .ends_with("Available: exclusive_swap_replacer, rfq_swap_replacer"),
            "expected the message to list the built-in, got: {error}"
        );
    }

    #[rstest::rstest]
    #[case::rfq(RfqSwapReplacer::NAME)]
    #[case::exclusive(ExclusiveSwapReplacer::NAME)]
    fn test_resolve_built_in_with_empty_registry(#[case] name: &str) {
        let factories = PostProcessorRegistry::new()
            .resolve("pool", &[name.to_string()])
            .expect("the name is built in");

        let names: Vec<String> = factories
            .build()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, [name]);
    }

    #[rstest::rstest]
    #[case::rfq(RfqSwapReplacer::NAME)]
    #[case::exclusive(ExclusiveSwapReplacer::NAME)]
    fn test_registry_refuses_built_in_name(#[case] name: &str) {
        let error = PostProcessorRegistry::new()
            .with_post_processor(name, || Noop)
            .expect_err("a built-in uses the name");

        assert_eq!(error, PostProcessorAlreadyRegistered { name: name.to_string() });
    }

    #[rstest::rstest]
    #[case::last(&["rfq_swap_replacer", "exclusive_swap_replacer"], None)]
    #[case::alone(&["exclusive_swap_replacer"], None)]
    #[case::absent(&["rfq_swap_replacer", "custom"], None)]
    #[case::not_last(&["exclusive_swap_replacer", "rfq_swap_replacer"], Some("rfq_swap_replacer"))]
    #[case::listed_twice(
        &["exclusive_swap_replacer", "exclusive_swap_replacer"],
        Some("exclusive_swap_replacer")
    )]
    fn test_check_post_processor_order(#[case] names: &[&str], #[case] followed_by: Option<&str>) {
        let names: Vec<String> = names
            .iter()
            .map(|name| name.to_string())
            .collect();

        let result = check_post_processor_order("main", &names);

        let expected = followed_by.map(|followed_by| PostProcessorOrderError {
            worker_pool: "main".to_string(),
            post_processor_name: ExclusiveSwapReplacer::NAME.to_string(),
            followed_by: followed_by.to_string(),
        });
        assert_eq!(result.err(), expected);
    }
}
