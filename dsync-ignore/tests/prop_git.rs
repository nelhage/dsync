//! Property test: the direct evaluator agrees with git's own rule engine.
//!
//! Ground truth is `git ls-files -o -i --exclude-standard`, which enumerates
//! every untracked file git considers ignored (including files inside
//! ignored directories). Cases use the full pattern space, negations
//! included; the `.dsyncexclude` layer is disabled since git knows nothing
//! about it.

mod common;

use proptest::prelude::*;

proptest! {
    #![proptest_config(common::config(96))]

    #[test]
    fn evaluator_matches_git(case in common::case_strategy(true, false)) {
        let tmp = common::materialize(&case);
        let root = tmp.path();
        common::git_init(root);
        let git_ignored = common::git_ignored_files(root);
        let set = dsync_ignore::load_repo(root, None).expect("load_repo");
        for path in case.all_paths() {
            let ours = set.is_ignored(&path, false);
            let gits = git_ignored.contains(&path);
            prop_assert_eq!(
                ours,
                gits,
                "verdict mismatch for {:?}: evaluator says ignored={}, git says ignored={}\ncase: {:#?}",
                path,
                ours,
                gits,
                &case
            );
        }
    }
}
