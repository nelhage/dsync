//! Property test: the watchman expression translation makes `watchman query`
//! return exactly the files the evaluator says are synced.
//!
//! Negated patterns are excluded from generation: they have no watchman
//! translation by design (`TranslateError::UnsupportedNegation`; callers
//! fall back to a full rsync). A non-property test below pins that contract.
//! The `.dsyncexclude` layer is enabled (exclude-only patterns).

mod common;

use proptest::prelude::*;
use std::collections::BTreeSet;

proptest! {
    #![proptest_config(common::config(32))]

    #[test]
    fn watchman_query_matches_evaluator(case in common::case_strategy(false, true)) {
        let tmp = common::materialize(&case);
        let root = tmp.path();
        // A real .git directory: between watchman's default VCS ignore and
        // the expression's .git terms, none of it may come back.
        common::git_init(root);
        let set = dsync_ignore::load_repo(root, None).expect("load_repo");
        let expr = dsync_ignore::watchman_synced_files_expr(&set)
            .expect("negation-free sets must translate");
        let queried = common::watchman_query_files(root, &expr);
        let expected: BTreeSet<String> = case
            .all_paths()
            .into_iter()
            .filter(|p| !set.is_ignored(p, false))
            .collect();
        prop_assert_eq!(
            &queried,
            &expected,
            "watchman returned {:?} but evaluator expects {:?}\nexpression: {}\ncase: {:#?}",
            &queried,
            &expected,
            serde_json::to_string_pretty(&expr).unwrap(),
            &case
        );
    }
}

/// The documented divergence: rule sets containing `!` patterns refuse to
/// translate, so the fast path can fall back to a full rsync.
#[test]
fn negated_patterns_refuse_to_translate() {
    let mut set = dsync_ignore::IgnoreSet::new();
    set.add_source("", "*.tmp\n!keep.tmp\n");
    assert!(matches!(
        dsync_ignore::watchman_synced_files_expr(&set),
        Err(dsync_ignore::TranslateError::UnsupportedNegation { .. })
    ));
}
