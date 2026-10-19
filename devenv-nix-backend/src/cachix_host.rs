//! What the cachix integration asks of a backend: read its settings,
//! register the caches as substituters, and hear about every path it
//! realises so they can be pushed.

use std::sync::Arc;

use async_trait::async_trait;
use devenv_activity::Activity;
use devenv_core::StoreSettings;
use devenv_core::evaluator::Evaluator;
use devenv_core::realized::RealizedPathsObserver;
use miette::Result;

#[async_trait(?Send)]
pub trait CachixHost: Evaluator {
    async fn eval_attr(&self, attr_path: &str, activity: &Activity) -> Result<String>;
    fn apply_store_settings(&self, settings: &StoreSettings);
    fn add_realized_observer(&self, observer: Arc<dyn RealizedPathsObserver>);
}

#[async_trait(?Send)]
impl CachixHost for crate::NixCBackend {
    async fn eval_attr(&self, attr_path: &str, activity: &Activity) -> Result<String> {
        crate::NixCBackend::eval_attr(self, attr_path, activity).await
    }

    fn apply_store_settings(&self, settings: &StoreSettings) {
        crate::NixCBackend::apply_store_settings(self, settings)
    }

    fn add_realized_observer(&self, observer: Arc<dyn RealizedPathsObserver>) {
        crate::NixCBackend::add_realized_observer(self, observer)
    }
}
