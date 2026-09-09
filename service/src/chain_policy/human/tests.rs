use super::*;

// ---------------------------------------------------------------- fee --

/// The percentages an operator actually types, and the basis points they
/// must become. `6` is the Robinhood launch rate and `3` is the Solana
/// one, so these two rows are the change's headline numbers.
#[test]
fn percentages_convert_to_basis_points_exactly() {
    let cases = [
        ("6", 600),
        ("3", 300),
        ("1.5", 150),
        ("1", 100),
        ("0.5", 50),
        ("0.25", 25),
        ("0.01", 1),
        ("6.00", 600),
        ("06", 600),
        (" 6 ", 600),
        (".5", 50),
        ("99.99", 9_999),
    ];
    for (input, expected) in cases {
        assert_eq!(
            parse_fee_percent(input),
            Ok(expected),
            "percentage {input:?} must be {expected} bps"
        );
    }
}

/// A rate finer than one basis point cannot be charged, so it is refused
/// rather than rounded. Rounding is how a fee silently becomes a number
/// nobody chose.
#[test]
fn a_percentage_finer_than_one_basis_point_is_refused() {
    for input in ["0.001", "1.234", "6.005"] {
        assert!(
            matches!(
                parse_fee_percent(input),
                Err(HumanParseError::TooPrecise { .. })
            ),
            "{input:?} must be refused as too precise"
        );
    }
}

#[test]
fn a_negative_percentage_is_refused() {
    for input in ["-1", "-0.5", "-6"] {
        assert!(matches!(
            parse_fee_percent(input),
            Err(HumanParseError::Negative { .. })
        ));
    }
}

#[test]
fn malformed_percentages_are_refused() {
    for input in [
        "", "   ", "abc", "6%", "6.5.5", "6.", "1e3", "6 5", "+6", "0x6",
    ] {
        assert!(
            parse_fee_percent(input).is_err(),
            "{input:?} must be refused"
        );
    }
}

/// Round-tripping a rate back to the string an operator would recognise.
#[test]
fn basis_points_render_as_the_percentage_they_are() {
    assert_eq!(format_percent(600), "6%");
    assert_eq!(format_percent(300), "3%");
    assert_eq!(format_percent(150), "1.5%");
    assert_eq!(format_percent(25), "0.25%");
    assert_eq!(format_percent(1), "0.01%");
    assert_eq!(format_percent(0), "0%");
}

#[test]
fn every_parseable_percentage_round_trips_through_its_rendering() {
    for bps in 0..=BPS_PER_HUNDRED_PERCENT {
        let rendered = format_percent(bps);
        let back = parse_fee_percent(rendered.trim_end_matches('%'))
            .unwrap_or_else(|e| panic!("{rendered} did not parse back: {e}"));
        assert_eq!(back, bps, "{rendered} round-tripped to {back}, not {bps}");
    }
}

// ------------------------------------------------------------- amounts --

/// The launch figures, in the form an operator types them.
#[test]
fn glc_amounts_convert_to_canonical_units_exactly() {
    let cases = [
        ("20000", 2_000_000_000_000u64),
        ("20,000", 2_000_000_000_000),
        ("20_000", 2_000_000_000_000),
        ("100000", 10_000_000_000_000),
        ("10000000", 1_000_000_000_000_000),
        ("10,000,000", 1_000_000_000_000_000),
        ("1", 100_000_000),
        ("0.5", 50_000_000),
        ("0.00000001", 1),
        ("0", 0),
    ];
    for (input, expected) in cases {
        assert_eq!(
            parse_glc(input).map(|c| c.0),
            Ok(expected),
            "{input:?} GLC must be {expected} canonical"
        );
    }
}

/// The three launch numbers, asserted against the exact atomic values the
/// config file and the runbook state, so a typo in either cannot pass.
#[test]
fn the_robinhood_launch_figures_are_the_documented_atomic_values() {
    assert_eq!(parse_fee_percent("6").unwrap(), 600);
    assert_eq!(parse_glc("20000").unwrap().0, 2_000_000_000_000);
    assert_eq!(parse_glc("10000000").unwrap().0, 1_000_000_000_000_000);
    // And half the strict rolling figure, which is what goes on chain.
    assert_eq!(parse_glc("5000000").unwrap().0, 500_000_000_000_000);
}

#[test]
fn an_amount_finer_than_one_canonical_unit_is_refused() {
    for input in ["0.000000001", "1.123456789"] {
        assert!(matches!(
            parse_glc(input),
            Err(HumanParseError::TooPrecise { .. })
        ));
    }
}

#[test]
fn a_negative_amount_is_refused() {
    for input in ["-1", "-20000", "-0.5"] {
        assert!(matches!(
            parse_glc(input),
            Err(HumanParseError::Negative { .. })
        ));
    }
}

#[test]
fn malformed_amounts_are_refused() {
    for input in ["", "GLC", "20000 GLC", "1.2.3", "20000.", "1e7", "++1"] {
        assert!(parse_glc(input).is_err(), "{input:?} must be refused");
    }
}

/// An amount too large for the canonical unit is an overflow, not a
/// wrapped value.
#[test]
fn an_amount_beyond_the_canonical_range_overflows_rather_than_wrapping() {
    // u64::MAX canonical is ~184,467,440,737 GLC; ten times that cannot
    // be represented.
    for input in [
        "1844674407371",
        "99999999999999999999",
        "184467440737.09551616",
    ] {
        assert!(
            matches!(parse_glc(input), Err(HumanParseError::Overflow { .. })),
            "{input:?} must overflow rather than wrap"
        );
    }
    // The largest representable amount still parses.
    assert_eq!(parse_glc("184467440737.09551615").unwrap().0, u64::MAX);
}

#[test]
fn canonical_amounts_render_as_grouped_glc() {
    assert_eq!(format_glc(2_000_000_000_000), "20,000 GLC");
    assert_eq!(format_glc(1_000_000_000_000_000), "10,000,000 GLC");
    assert_eq!(format_glc(500_000_000_000_000), "5,000,000 GLC");
    assert_eq!(format_glc(10_000_000_000_000), "100,000 GLC");
    assert_eq!(format_glc(100_000_000), "1 GLC");
    assert_eq!(format_glc(50_000_000), "0.5 GLC");
    assert_eq!(format_glc(1), "0.00000001 GLC");
    assert_eq!(format_glc(0), "0 GLC");
}

#[test]
fn grouping_is_display_only_and_never_changes_the_value() {
    for canonical in [0u64, 1, 999, 100_000_000, 2_000_000_000_000, u64::MAX] {
        let rendered = format_glc(canonical);
        let back = parse_glc(rendered.trim_end_matches(" GLC")).expect("renders back");
        assert_eq!(back.0, canonical, "{rendered}");
    }
}
