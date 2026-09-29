//! Tier naming and filter-set self-tests: the filters and layer table are the expected
//! expressions.

use pemu_testkit::tiers::{Layer, Tier, tier_filter};

#[test]
fn t0_a_split_milestone_has_its_own_filter() {
    assert_eq!(
        Tier::T1.milestone_filter("9a"),
        "test(/(^|::)t1_m9a_/)",
        "M9a is its own set, so `m9_` must not stand in for it"
    );
    assert!(
        !"t1_m9a_viewport".starts_with("t1_m9_"),
        "the M9 filter cannot select an M9a test"
    );
}

#[test]
fn t0_the_tier_of_a_test_comes_from_its_prefix() {
    assert_eq!(Tier::of_test("t2_m11_bands"), Some(Tier::T2));
    assert_eq!(Tier::of_test("t0_the_log_keeps_order"), Some(Tier::T0));
    assert_eq!(Tier::of_test("reset_values_match"), None);
    assert_eq!(Tier::T2.prefix(), "t2_");
    assert_eq!(Tier::T1.to_string(), "t1");
}

#[test]
fn t0_the_milestone_filters_are_the_expected_expressions() {
    // A milestone's T1 tests are run by their name prefix.
    assert_eq!(Tier::T1.milestone_filter("3"), "test(/(^|::)t1_m3_/)");
    // Every tier of a claimed milestone.
    assert_eq!(
        Tier::every_tier_milestone_filter("5"),
        "test(/(^|::)t[012]_m5_/)"
    );
    assert_eq!(Tier::T2.name_filter(), "test(/(^|::)t2_/)");
}

/// nextest matches `test(/<re>/)` against the module-qualified name, so every filter anchors on
/// the last `::` segment.
#[test]
fn t0_every_filter_anchors_on_a_path_segment_not_on_the_whole_name() {
    let matches = |filter: &str, name: &str| {
        let prefix = filter
            .strip_prefix("test(/(^|::)")
            .and_then(|f| f.strip_suffix("/)"))
            .unwrap_or_else(|| panic!("{filter} is not a segment-anchored test filter"));
        let last = name.rsplit("::").next().unwrap_or(name);
        last.starts_with(prefix)
    };

    for filter in [
        Tier::T1.name_filter(),
        Tier::T1.milestone_filter("3"),
        Tier::T2.name_filter(),
    ] {
        assert!(
            filter.starts_with("test(/(^|::)"),
            "{filter} must not anchor with a bare `^`"
        );
    }

    // A `t1_` test inside a `mod` must be subtracted too, or T0 would run a corpus-bound test.
    assert!(matches(
        &Tier::T1.name_filter(),
        "decode_corpus::t1_rom_and_corpus_elfs_decode_like_objdump"
    ));
    assert!(matches(&Tier::T1.milestone_filter("3"), "m3::t1_m3_x"));
    assert!(!matches(&Tier::T1.name_filter(), "board::t0_the_log"));
    assert!(
        !matches(&Tier::T1.name_filter(), "helper_t1_not_a_claim"),
        "a prefix in the middle of a segment is not a match"
    );
}

#[test]
fn t0_t0_runs_everything_that_is_not_named_for_a_later_tier() {
    // Crate unit tests carry no tier prefix, so T0 is everything minus the named T1 and T2 tests.
    assert_eq!(
        Tier::T0.filter(),
        "all() - test(/(^|::)t1_/) - test(/(^|::)t2_/)"
    );
    assert_eq!(Tier::T1.filter(), "test(/(^|::)t1_/)");
    assert_eq!(Tier::T2.filter(), "test(/(^|::)t2_/)");
}

#[test]
fn t0_the_layer_table_has_eight_layers_with_their_tiers_and_packages() {
    assert_eq!(Layer::ALL.len(), 8);
    assert_eq!(Layer::L0.id(), "L0");

    assert_eq!(Layer::L0.tiers(), &[Tier::T0]);
    assert_eq!(Layer::L1.tiers(), &[Tier::T0]);
    assert_eq!(Layer::L2.tiers(), &[Tier::T0, Tier::T1]);
    assert_eq!(Layer::L3.tiers(), &[Tier::T1]);
    assert_eq!(Layer::L5.tiers(), &[Tier::T2]);
    assert!(Layer::L4.runs_in(Tier::T1) && Layer::L4.runs_in(Tier::T2));
    assert!(!Layer::L5.runs_in(Tier::T0));

    assert_eq!(Layer::L1.packages(), &["pemu-soc-c3", "pemu-board"]);
    assert!(Layer::L1.what().contains("RegHarness"));

    // Exactly two layers carry no package list; L5 is tested in `pemu-verify`.
    let empty: Vec<Layer> = Layer::ALL
        .into_iter()
        .filter(|l| l.packages().is_empty())
        .collect();
    assert_eq!(empty, vec![Layer::L0, Layer::L7]);
    assert_eq!(Layer::L5.packages(), &["pemu-verify"]);
}

#[test]
fn t0_a_layer_filter_intersects_its_packages_with_the_tier() {
    assert_eq!(
        Layer::L1.filter(Tier::T0),
        "(package(pemu-soc-c3) + package(pemu-board)) \
         & (all() - test(/(^|::)t1_/) - test(/(^|::)t2_/))"
    );
    assert_eq!(
        Layer::L3.filter(Tier::T1),
        "(package(pemu-milestones) + package(pemu-testkit)) & (test(/(^|::)t1_/))"
    );
    assert_eq!(
        Layer::L1.filter(Tier::T2),
        "none()",
        "a layer the tier does not run selects nothing"
    );
    assert_eq!(
        Layer::L7.filter(Tier::T1),
        "none()",
        "the browser and benchmark layer has no cargo test target"
    );
    assert_eq!(
        Layer::L0.filter(Tier::T0),
        Tier::T0.filter(),
        "L0 is `each crate`, so it is the tier filter itself"
    );
}

#[test]
fn t0_a_tier_filter_is_the_union_of_the_layers_that_tier_runs() {
    let t2 = tier_filter(Tier::T2);
    assert!(
        t2.contains("package(pemu-verify)"),
        "L5 is a T2 layer: {t2}"
    );
    assert!(
        t2.contains("package(pemu-milestones)"),
        "L4 is a T2 layer: {t2}"
    );
    assert!(
        !t2.contains("package(pemu-board)"),
        "L1 is T0 only, so it is not in the T2 set: {t2}"
    );
    assert!(!t2.contains("none()"), "empty layers drop out: {t2}");

    let t0 = tier_filter(Tier::T0);
    assert!(t0.starts_with(&Tier::T0.filter()), "L0 comes first: {t0}");
}
