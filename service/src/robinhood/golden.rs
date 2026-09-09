//! The cross-language EIP-712 golden fixture, and the one reader for it.
//!
//! `contracts/test/fixtures/eip712-golden.json` is the shared contract
//! between this crate's tests and `contracts/test/GoldenDigests.t.sol`.
//! NEITHER side generates it: the Foundry suite asserts the DEPLOYED
//! CONTRACT produces every value in it, and this crate's tests assert the
//! Rust transcription produces the same ones. A drift on either side
//! fails that side against a file it cannot quietly edit into agreement.
//!
//! This module exists so there is exactly ONE parser for that file.
//! [`crate::robinhood::auth`]'s tests pin the payout, refund and
//! settlement vectors; [`crate::robinhood::governance`]'s pin the three
//! governance vectors. A second copy of the accessor could disagree with
//! the first about which key it was reading, which for a file whose whole
//! job is to catch disagreement would be an unusually silly failure.

/// Read at COMPILE time from the exact path the Foundry suite reads at
/// run time.
///
/// `include_str!` rather than a runtime file read: the test binary then
/// carries the fixture's bytes, so a test cannot pass because a file
/// happened to be missing or because a path resolved somewhere
/// unexpected — a wrong path is a build failure, and a wrong value is a
/// test failure.
pub const FIXTURE: &str = include_str!("../../../contracts/test/fixtures/eip712-golden.json");

pub const VERIFYING_CONTRACT: &str = "0x00000000000000000000000000000000000B21D6";
pub const TOKEN: &str = "0x000000000000000000000000000000000000704e";
pub const RECIPIENT: &str = "0x000000000000000000000000000000000000eC19";

/// One fixture value, by dotted JSON path (e.g. `"payout.digest"`,
/// `"governance.setLimits.structHash"`), rendered as the string it
/// appears as in the file.
///
/// Parsed with `serde_json` — already a dependency — rather than scanned
/// textually: several vectors share the same leaf key (`structHash`,
/// `digest`) under different parents, so a leaf-only lookup would
/// silently compare against whichever one it found first.
///
/// A path that resolves to nothing panics. A fixture key renamed on the
/// Solidity side therefore fails loudly here instead of quietly asserting
/// nothing.
pub fn get(path: &str) -> String {
    let root: serde_json::Value =
        serde_json::from_str(FIXTURE).expect("the golden fixture must be valid JSON");
    // `inputs` is a flat block of documented parameters; the digests sit
    // at the top level. Try the literal path first, then under `inputs`,
    // so a caller writes `"expiry"` rather than `"inputs.expiry"` for the
    // half that lives there.
    for prefix in ["", "inputs."] {
        let full = format!("{prefix}{path}");
        let mut node = &root;
        let mut ok = true;
        for segment in full.split('.') {
            match node.get(segment) {
                Some(next) => node = next,
                None => {
                    ok = false;
                    break;
                }
            }
        }
        if ok {
            return match node {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
        }
    }
    panic!("the golden fixture has no key {path:?}");
}

/// The fixture's spelling for a 32-byte value: lowercase, `0x`-prefixed.
pub fn hex32(bytes: &[u8; 32]) -> String {
    format!(
        "0x{}",
        bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
    )
}
