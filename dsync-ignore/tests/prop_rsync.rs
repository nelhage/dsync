//! Property test: the rsync filter translation makes rsync transfer exactly
//! the files the evaluator says are synced.
//!
//! rsync invoked with a source but no destination (`--list-only`) enumerates
//! the files it would transfer, honoring filters — no sync-and-diff needed.
//! The evaluator itself is checked against git in `prop_git.rs`; here it
//! serves as the expectation, which also lets these cases exercise the
//! `.dsyncexclude` layer (which git cannot adjudicate). Negations are in
//! scope: the rsync translation supports them.

mod common;

use proptest::prelude::*;
use std::collections::BTreeSet;

proptest! {
    #![proptest_config(common::config(96))]

    #[test]
    fn rsync_list_matches_evaluator(case in common::case_strategy(true, true)) {
        let tmp = common::materialize(&case);
        let root = tmp.path();
        // A real .git directory, so the built-in `- /.git` rule is what keeps
        // its contents out of the listing.
        common::git_init(root);
        let set = dsync_ignore::load_repo(root, None).expect("load_repo");
        // The generator cannot produce patterns past the variant cap; treat
        // translation as total here.
        let rules = dsync_ignore::rsync_filter_rules(&set).expect("translate");
        let listed = common::rsync_list_files(root, &rules);
        let expected: BTreeSet<String> = case
            .all_paths()
            .into_iter()
            .filter(|p| !set.is_ignored(p, false))
            .collect();
        prop_assert_eq!(
            &listed,
            &expected,
            "rsync listed {:?} but evaluator expects {:?}\nfilters: {:#?}\ncase: {:#?}",
            &listed,
            &expected,
            &rules,
            &case
        );
    }
}
