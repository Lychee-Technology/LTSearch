#[test]
fn crate_exposes_name_constant() {
    assert_eq!(ltsearch::CRATE_NAME, "ltsearch");
}

#[test]
fn library_exposes_top_level_modules() {
    let module_boundaries = [
        core::any::type_name::<ltsearch::adapters::ModuleBoundary>(),
        core::any::type_name::<ltsearch::models::ModuleBoundary>(),
        core::any::type_name::<ltsearch::query::ModuleBoundary>(),
        core::any::type_name::<ltsearch::indexing::ModuleBoundary>(),
        core::any::type_name::<ltsearch::storage::ModuleBoundary>(),
        core::any::type_name::<ltsearch::write::ModuleBoundary>(),
        core::any::type_name::<ltsearch::embedding::ModuleBoundary>(),
        core::any::type_name::<ltsearch::config::ModuleBoundary>(),
        core::any::type_name::<ltsearch::error::ModuleBoundary>(),
    ];

    assert_eq!(module_boundaries.len(), 9);
}

#[test]
fn config_placeholder_uses_workspace_managed_serde() {
    fn assert_serde<T>()
    where
        T: serde::Serialize + for<'de> serde::Deserialize<'de>,
    {
    }

    assert_serde::<ltsearch::config::AppConfig>();
}
