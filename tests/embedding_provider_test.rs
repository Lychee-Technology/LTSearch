use std::sync::Mutex;

use ltsearch::embedding::{
    fixed_generator_from_env, provider_from_env_or_default, required_provider_from_env,
    EmbeddingGenerator, EmbeddingProvider,
};

static EMBEDDING_PROVIDER_ENV_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn required_provider_from_env_reports_missing_variable() {
    let _guard = EMBEDDING_PROVIDER_ENV_LOCK.lock().unwrap();
    std::env::remove_var("LTSEARCH_QUERY_EMBEDDING_PROVIDER");

    let error = required_provider_from_env("LTSEARCH_QUERY_EMBEDDING_PROVIDER").unwrap_err();

    assert_eq!(
        error.to_string(),
        "missing LTSEARCH_QUERY_EMBEDDING_PROVIDER"
    );
}

#[test]
fn provider_from_env_or_default_returns_fixed_when_variable_is_missing() {
    let _guard = EMBEDDING_PROVIDER_ENV_LOCK.lock().unwrap();
    std::env::remove_var("LTSEARCH_BUILD_EMBEDDING_PROVIDER");

    let provider = provider_from_env_or_default(
        "LTSEARCH_BUILD_EMBEDDING_PROVIDER",
        EmbeddingProvider::Fixed,
    )
    .expect("expected missing build provider to fall back to fixed");

    assert_eq!(provider, EmbeddingProvider::Fixed);
}

#[test]
fn provider_parsing_rejects_unsupported_provider() {
    let _guard = EMBEDDING_PROVIDER_ENV_LOCK.lock().unwrap();
    std::env::set_var("LTSEARCH_QUERY_EMBEDDING_PROVIDER", "mystery");

    let error = required_provider_from_env("LTSEARCH_QUERY_EMBEDDING_PROVIDER").unwrap_err();

    assert_eq!(
        error.to_string(),
        "unsupported LTSEARCH_QUERY_EMBEDDING_PROVIDER: mystery"
    );
}

#[test]
fn provider_parsing_rejects_ltembed_when_feature_is_disabled() {
    let _guard = EMBEDDING_PROVIDER_ENV_LOCK.lock().unwrap();
    std::env::set_var("LTSEARCH_QUERY_EMBEDDING_PROVIDER", "ltembed");

    let error = required_provider_from_env("LTSEARCH_QUERY_EMBEDDING_PROVIDER").unwrap_err();

    assert_eq!(
        error.to_string(),
        "unsupported LTSEARCH_QUERY_EMBEDDING_PROVIDER: ltembed (feature disabled)"
    );
}

#[test]
fn fixed_generator_from_env_builds_compatible_embedding_generator() {
    let _guard = EMBEDDING_PROVIDER_ENV_LOCK.lock().unwrap();
    std::env::set_var("LTSEARCH_BUILD_FIXED_EMBEDDING", "0.1,0.2,0.3");
    std::env::set_var("LTSEARCH_BUILD_EMBEDDING_DIM", "3");

    let generator = fixed_generator_from_env(
        "LTSEARCH_BUILD_FIXED_EMBEDDING",
        Some("LTSEARCH_BUILD_EMBEDDING_DIM"),
    )
    .expect("expected fixed embedding generator to be constructed");

    let embedding = generator
        .generate("ignored")
        .expect("expected fixed generator to return configured embedding");
    assert_eq!(embedding, vec![0.1, 0.2, 0.3]);
}

#[test]
fn fixed_generator_from_env_rejects_dimension_mismatch() {
    let _guard = EMBEDDING_PROVIDER_ENV_LOCK.lock().unwrap();
    std::env::set_var("LTSEARCH_QUERY_FIXED_EMBEDDING", "0.1,0.2");
    std::env::set_var("LTSEARCH_QUERY_EMBEDDING_DIM", "3");

    let error = fixed_generator_from_env(
        "LTSEARCH_QUERY_FIXED_EMBEDDING",
        Some("LTSEARCH_QUERY_EMBEDDING_DIM"),
    )
    .unwrap_err();

    assert_eq!(
        error.to_string(),
        "LTSEARCH_QUERY_FIXED_EMBEDDING dimension 2 does not match LTSEARCH_QUERY_EMBEDDING_DIM 3"
    );
}
