//! Doc/binary consistency for `glc-admin` (docs/07-implementation-plan.md
//! Phase 5). Ported from the old bridge's `relayer/tests/
//! runbook_commands.rs` (docs/01-reuse-inventory.md): pure text-parsing,
//! no DB, no RPC, no process spawn — it exists because the old bridge's
//! own history shows a runbook and a binary drift apart silently
//! otherwise.

use std::collections::BTreeSet;
use std::path::Path;

fn runbook_text() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs/09-runbook.md");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("could not read {}: {e}", path.display()))
}

fn glc_admin_source() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/bin/glc-admin.rs");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("could not read {}: {e}", path.display()))
}

/// Every `glc-admin <word>` occurrence in the runbook, where `<word>` is a
/// run of lowercase ASCII letters/hyphens — the same shape a real
/// subcommand name takes.
fn commands_named_in_runbook(doc: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let needle = "glc-admin ";
    let mut rest = doc;
    while let Some(pos) = rest.find(needle) {
        let after = &rest[pos + needle.len()..];
        let word: String = after
            .chars()
            .take_while(|c| c.is_ascii_lowercase() || *c == '-')
            .collect();
        if !word.is_empty() {
            names.insert(word.clone());
        }
        let advance = word.len().max(1);
        if advance > after.len() {
            break;
        }
        rest = &after[advance..];
    }
    names
}

/// Every `match`-arm string literal in `glc-admin.rs`'s top-level command
/// dispatch (`match cmd.as_str() { ... }` in `main`) — a line whose
/// trimmed text starts with a quoted string immediately followed by `=>`.
/// Scoped to just that one match block, not every `match` in the file: the
/// binary also matches on `--direction`/`--scope` values, which are
/// argument values, not subcommands, and must not be confused with them.
fn commands_the_binary_accepts(src: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let start = src
        .find("match cmd.as_str()")
        .expect("glc-admin.rs must dispatch on `match cmd.as_str()` — extractor out of sync with the binary");
    let block = &src[start..];
    let end = block
        .find("\n    };")
        .expect("could not find the end of the command dispatch match block");
    for line in block[..end].lines() {
        let t = line.trim();
        let Some(rest) = t.strip_prefix('"') else {
            continue;
        };
        let Some(end) = rest.find('"') else { continue };
        let word = &rest[..end];
        if rest[end + 1..].trim_start().starts_with("=>") {
            names.insert(word.to_string());
        }
    }
    names
}

#[test]
fn every_command_the_runbook_names_exists_in_the_binary() {
    let named = commands_named_in_runbook(&runbook_text());
    let accepted = commands_the_binary_accepts(&glc_admin_source());
    assert!(!named.is_empty(), "extractor found nothing — likely broken");
    let missing: BTreeSet<_> = named.difference(&accepted).collect();
    assert!(
        missing.is_empty(),
        "docs/09-runbook.md names glc-admin subcommand(s) the binary does not accept: {missing:?}"
    );
}

#[test]
fn the_runbook_covers_every_command_the_binary_offers() {
    let named = commands_named_in_runbook(&runbook_text());
    let accepted = commands_the_binary_accepts(&glc_admin_source());
    let undocumented: BTreeSet<_> = accepted.difference(&named).collect();
    assert!(
        undocumented.is_empty(),
        "glc-admin accepts subcommand(s) docs/09-runbook.md never mentions: {undocumented:?}"
    );
}

/// The runbook must keep stating its own gaps, not silently start claiming
/// completeness it doesn't have (same discipline the old bridge's
/// equivalent test enforced).
#[test]
fn the_runbook_states_its_own_unbuilt_procedures() {
    let doc = runbook_text();
    for marker in ["No procedure exists yet", "not yet built"] {
        assert!(
            doc.contains(marker),
            "docs/09-runbook.md must keep stating what isn't built yet (expected to find {marker:?})"
        );
    }
}
