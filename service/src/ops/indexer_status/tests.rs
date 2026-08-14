use super::*;

#[test]
fn the_deepest_reorg_is_kept_not_the_most_recent() {
    // A 40-block reorg an hour ago is the fact an operator needs; a
    // later 1-block reorg must not erase it.
    let s = IndexerStatus::new(1_000);
    assert_eq!(s.deepest_reorg(), 0);
    s.record_reorg(40);
    s.record_reorg(1);
    assert_eq!(s.deepest_reorg(), 40);
    s.record_reorg(41);
    assert_eq!(s.deepest_reorg(), 41);
}

#[test]
fn the_configured_ceiling_is_reported_alongside_it() {
    // Without it a scraper cannot tell 40 out of 50 from 40 out of 500.
    let s = IndexerStatus::new(1_000);
    s.set_max_reorg_depth(50);
    assert_eq!(s.max_reorg_depth(), 50);
}

#[test]
fn a_fresh_status_is_not_halted() {
    let s = IndexerStatus::new(1_000);
    assert!(!s.is_halted());
    assert_eq!(s.seconds_since_tick(1_000), 0);
}

#[test]
fn a_halt_is_recorded_with_the_depth_that_caused_it() {
    let s = IndexerStatus::new(1_000);
    s.record_halt(120);
    assert!(s.is_halted());
    assert_eq!(s.halted_depth(), 120);
}

#[test]
fn a_halt_is_one_way() {
    // Clearing it means an operator widened max_reorg_depth and
    // restarted; nothing in-process may decide the halt is over.
    let s = IndexerStatus::new(1_000);
    s.record_halt(120);
    s.record_tick(2_000);
    assert!(s.is_halted(), "a later tick must not clear the halt");
}

#[test]
fn ticks_advance_the_freshness_clock() {
    let s = IndexerStatus::new(1_000);
    assert_eq!(s.seconds_since_tick(1_060), 60);
    s.record_tick(1_060);
    assert_eq!(s.seconds_since_tick(1_060), 0);
    assert_eq!(s.seconds_since_tick(1_090), 30);
}

#[test]
fn a_backwards_clock_reports_zero_rather_than_negative_staleness() {
    // A negative value would compare as "very recent" against any
    // threshold, turning a clock problem into a silent monitoring gap.
    let s = IndexerStatus::new(2_000);
    assert_eq!(s.seconds_since_tick(1_000), 0);
}

#[test]
fn extreme_timestamps_do_not_overflow() {
    let s = IndexerStatus::new(i64::MIN);
    assert!(s.seconds_since_tick(i64::MAX) >= 0);
}

#[test]
fn the_start_time_seeds_the_clock_so_a_first_scrape_is_not_decades_stale() {
    let s = IndexerStatus::new(1_700_000_000);
    assert_eq!(s.seconds_since_tick(1_700_000_030), 30);
}
