use super::*;

#[test]
fn renders_help_and_type_before_samples() {
    // A metric an operator cannot interpret is not observability.
    let mut r = Registry::new();
    r.gauge(
        "glc_reserve_balance",
        "Reserve balance in atomic units",
        42.0,
    );
    let out = r.encode();
    assert_eq!(
        out,
        "# HELP glc_reserve_balance Reserve balance in atomic units\n\
         # TYPE glc_reserve_balance gauge\n\
         glc_reserve_balance 42\n"
    );
}

#[test]
fn integral_values_render_without_a_decimal_point() {
    let mut r = Registry::new();
    r.gauge("v", "h", 21_000_000_000.0);
    assert!(r.encode().contains("v 21000000000\n"), "{}", r.encode());
}

#[test]
fn values_beyond_i64_render_exactly_rather_than_saturating() {
    // The regression a surviving mutant exposed in the old bridge: an
    // `as i64` cast saturates, so this once rendered as
    // 9223372036854775807 — a plausible-looking wrong number in a metric
    // an operator acts on.
    let mut r = Registry::new();
    r.gauge("huge", "h", 1e20);
    let out = r.encode();
    assert!(out.contains("huge 100000000000000000000\n"), "{out}");
    assert!(
        !out.contains("9223372036854775807"),
        "must not saturate at i64::MAX: {out}"
    );
}

#[test]
fn fractional_values_keep_their_fraction() {
    let mut r = Registry::new();
    r.gauge("f", "h", 0.5);
    assert!(r.encode().contains("f 0.5\n"), "{}", r.encode());
}

#[test]
fn labels_are_sorted_so_scrapes_are_diffable() {
    let mut r = Registry::new();
    r.record("m", Kind::Gauge, "h", &[("zeta", "1"), ("alpha", "2")], 1.0);
    assert!(
        r.encode().contains("m{alpha=\"2\",zeta=\"1\"} 1\n"),
        "{}",
        r.encode()
    );
}

#[test]
fn families_are_sorted_so_scrapes_are_diffable() {
    let mut r = Registry::new();
    r.gauge("zzz", "h", 1.0);
    r.gauge("aaa", "h", 1.0);
    let out = r.encode();
    assert!(out.find("aaa").unwrap() < out.find("zzz").unwrap());
}

#[test]
fn one_family_can_carry_many_labelled_samples() {
    let mut r = Registry::new();
    for (direction, n) in [("GoldcoinReserve", 3.0), ("SolanaReserve", 1.0)] {
        r.record(
            "glc_bridge_requests",
            Kind::Gauge,
            "Bridge requests per direction",
            &[("direction", direction)],
            n,
        );
    }
    let out = r.encode();
    assert_eq!(
        out.matches("# TYPE glc_bridge_requests").count(),
        1,
        "one TYPE line"
    );
    assert!(out.contains("glc_bridge_requests{direction=\"GoldcoinReserve\"} 3\n"));
    assert!(out.contains("glc_bridge_requests{direction=\"SolanaReserve\"} 1\n"));
}

#[test]
fn label_values_are_escaped() {
    // A refusal reason ends up in a label; an unescaped quote would
    // produce output no scraper can parse.
    let mut r = Registry::new();
    r.record("m", Kind::Gauge, "h", &[("reason", "a\"b\\c\nd")], 1.0);
    assert!(
        r.encode().contains(r#"m{reason="a\"b\\c\nd"} 1"#),
        "{}",
        r.encode()
    );
}

#[test]
fn non_finite_values_render_as_prometheus_expects() {
    let mut r = Registry::new();
    r.gauge("nan", "h", f64::NAN);
    r.gauge("pinf", "h", f64::INFINITY);
    r.gauge("ninf", "h", f64::NEG_INFINITY);
    let out = r.encode();
    assert!(out.contains("nan NaN\n"));
    assert!(out.contains("pinf +Inf\n"));
    assert!(out.contains("ninf -Inf\n"));
}

#[test]
fn an_empty_registry_encodes_to_nothing() {
    assert!(Registry::new().is_empty());
    assert_eq!(Registry::new().encode(), "");
}

#[test]
fn counters_and_gauges_are_typed_distinctly() {
    let mut r = Registry::new();
    r.counter("c", "h", 1.0);
    r.gauge("g", "h", 1.0);
    let out = r.encode();
    assert!(out.contains("# TYPE c counter\n"));
    assert!(out.contains("# TYPE g gauge\n"));
}
