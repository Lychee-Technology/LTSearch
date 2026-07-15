//! The neutral contract facade must name all six contracts without pulling AWS.

use ltsearch::contracts::{ArtifactSync, BuildJob, BuildJobSource};

#[test]
fn build_job_carries_receipt_and_body() {
    let job = BuildJob {
        receipt: "r-1".to_string(),
        body: "{}".to_string(),
    };
    assert_eq!(job.receipt, "r-1");
    assert_eq!(job.body, "{}");
}

// Compile-only: the facade re-exports the storage contracts under one path.
#[allow(dead_code)]
fn contract_paths_exist() {
    fn assert_impl<T: ?Sized>() {}
    assert_impl::<dyn BuildJobSource>();
    assert_impl::<dyn ArtifactSync>();
    // `PublishStorage` is a `Clone`-bounded (non-object-safe) trait, so reference
    // it as a trait bound to prove the facade re-export path resolves.
    fn _requires_publish_storage<T: ltsearch::contracts::PublishStorage>() {}
}
