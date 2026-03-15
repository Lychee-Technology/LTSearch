#[test]
fn lancedb_dependencies_compile_for_local_connect_builder() {
    let builder = lancedb::connect("/tmp/ltsearch-smoke");
    let _ = builder;
}
