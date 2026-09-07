//! Removing endpoint identity and embedded credentials from any text
//! that reaches an operator-facing surface.
//!
//! # The leak this closes
//!
//! `robinhood::rpc::EvmRpcError::Transport` is constructed from
//! `reqwest::Error::to_string()`, and reqwest's `Display` embeds the
//! request URL: a connection failure renders as
//! `error sending request for url (https://user:pass@node.example:8545/KEY)`.
//! That string was published verbatim into
//! [`super::health::RobinhoodHealthSnapshot::last_rpc_error`], which is
//! read by `/health`, `/metrics`-adjacent tooling and the admin API.
//!
//! `/health` has no authentication by design (`ops::health`'s module docs
//! record why: adding one would mean this process holding another
//! secret), so anything in that snapshot is readable by anything that can
//! reach the port. An RPC URL is infrastructure detail; an RPC URL with
//! `user:pass@` in it is a live credential. Neither belongs there.
//!
//! # Why redaction rather than "just do not log the URL"
//!
//! Because this service does not build those strings. They come from
//! reqwest, from a JSON-RPC node's own `message` field, and from library
//! errors this crate does not control — three sources whose formatting
//! can change under a dependency bump without any code here changing. A
//! rule of the form "call sites must remember not to include the URL" is
//! exactly the kind that holds until the day it does not.
//!
//! So the redaction sits at the BOUNDARY instead: every string entering
//! the published health state passes through [`Redactor::apply`], and no
//! call site is trusted to have been careful. A future error variant, a
//! future dependency's phrasing, and a hostile node echoing text back all
//! land in the same filter.
//!
//! # Three passes, deliberately overlapping
//!
//! 1. **Scheme-qualified URLs.** Any `scheme://…` run is replaced
//!    wholesale by [`REDACTED_ENDPOINT`]. This is format-independent: it
//!    does not need to know what the configured endpoint is, so it
//!    catches a URL this service never configured — a redirect target, a
//!    node's own upstream, a proxy named in an error.
//! 2. **The configured endpoint's own literals.** The URL, its
//!    `user`/`password`, its `host[:port]`, any long path segment, and —
//!    as a fallback for a malformed URL — every non-numeric
//!    delimiter-separated token of it. Matched case-insensitively and
//!    longest-first. This catches what pass 1 cannot: a bare hostname in
//!    a DNS error, or a password echoed on its own.
//! 3. **Residual `user:pass@host` forms.** Anything still carrying
//!    userinfo shape after the first two passes.
//!
//! Overlap is the point. Pass 1 needs no configuration and pass 2 needs
//! no format assumptions, so a leak has to defeat both independently.
//!
//! # What it deliberately does NOT do
//!
//! It does not redact block numbers, chain ids, contract addresses,
//! transaction hashes, JSON-RPC error codes, or the error's own
//! prose. Those are what makes a health surface diagnostic rather than
//! decorative, and none of them is a secret — a bridge contract address
//! is on a public chain by construction. Over-redaction has a real cost:
//! an operator who cannot tell a rate-limited endpoint from a wrong chain
//! id will restart the wrong thing at 3am.

/// What a scheme-qualified URL is replaced with. A fixed marker rather
/// than an empty string, so an operator can see that something was
/// removed and does not read the gap as the error itself being empty.
pub const REDACTED_ENDPOINT: &str = "<redacted-endpoint>";

/// What a credential-shaped run is replaced with.
pub const REDACTED_CREDENTIAL: &str = "<redacted-credential>";

/// Userinfo components shorter than this are not treated as literal
/// secrets in pass 2.
///
/// Not a judgement that a two-character password is safe — it is a
/// judgement that no real deployment has one, and that redacting every
/// occurrence of a one- or two-character string would corrupt every
/// message it appears in (`"at"`, `"of"`, a hex nibble) while protecting
/// nothing. Pass 1 still removes such a credential whenever it appears
/// inside the URL it belongs to, which is the only place it realistically
/// appears at all.
const MIN_LITERAL_SECRET_LEN: usize = 3;

/// Holds the literal strings that must never reach an operator-facing
/// surface for one configured endpoint.
///
/// Construct one with [`Redactor::for_endpoint`] and keep it beside the
/// health state it protects. [`Redactor::none`] exists for the
/// unconfigured case, and still performs passes 1 and 3 — a deployment
/// with no Robinhood endpoint has no literals to strip, but it can still
/// be handed a string containing somebody else's URL.
#[derive(Debug, Clone, Default)]
pub struct Redactor {
    /// Longest-first, lowercased. Longest-first matters: redacting the
    /// host before the full URL would leave the scheme and path behind as
    /// two fragments around a marker.
    literals: Vec<String>,
}

impl Redactor {
    /// A redactor holding no endpoint literals. Passes 1 and 3 still
    /// apply.
    pub fn none() -> Redactor {
        Redactor {
            literals: Vec::new(),
        }
    }

    /// A redactor for one configured RPC endpoint.
    ///
    /// Parsed by hand rather than with a URL crate: this needs to work on
    /// a malformed URL too (an operator's typo is exactly when errors get
    /// logged), and a parser that rejected the input would leave the
    /// literals unprotected at the moment they matter most.
    pub fn for_endpoint(url: &str) -> Redactor {
        let mut literals: Vec<String> = Vec::new();
        let trimmed = url.trim();
        if !trimmed.is_empty() {
            literals.push(trimmed.to_string());
        }

        // Everything after the scheme separator, if there is one.
        let after_scheme = match trimmed.find("://") {
            Some(i) => &trimmed[i + 3..],
            None => trimmed,
        };
        // authority ends at the first '/', '?' or '#'.
        let authority_end = after_scheme
            .find(['/', '?', '#'])
            .unwrap_or(after_scheme.len());
        let authority = &after_scheme[..authority_end];

        // `userinfo@host[:port]` — rsplit, because a password may itself
        // contain '@'.
        let (userinfo, host_port) = match authority.rfind('@') {
            Some(i) => (Some(&authority[..i]), &authority[i + 1..]),
            None => (None, authority),
        };

        if let Some(userinfo) = userinfo {
            if !userinfo.is_empty() {
                literals.push(userinfo.to_string());
            }
            // Split on the FIRST ':' — a password may contain ':'.
            match userinfo.find(':') {
                Some(i) => {
                    literals.push(userinfo[..i].to_string());
                    literals.push(userinfo[i + 1..].to_string());
                }
                None => literals.push(userinfo.to_string()),
            }
        }
        if !host_port.is_empty() {
            literals.push(host_port.to_string());
            if let Some(i) = host_port.rfind(':') {
                literals.push(host_port[..i].to_string());
            }
        }

        // Some providers put the API key in the PATH rather than in
        // userinfo (`https://host/v2/<key>`). The whole URL is already a
        // literal, but a path segment can also be echoed on its own, so
        // each non-trivial segment is protected too.
        let path = &after_scheme[authority_end..];
        for segment in path.split(['/', '?', '#', '&', '=']) {
            if segment.len() >= 8 {
                literals.push(segment.to_string());
            }
        }

        // Fallback: every delimiter-separated token of the raw URL.
        //
        // The structured parse above assumes a well-formed URL, and a
        // malformed one is exactly when this matters most — an operator's
        // typo is what produces the errors that get logged. On
        // `htp:/user:pass@host` the parse locates the authority wrongly
        // and would protect neither `pass` nor `host`; tokenizing does,
        // without needing the shape to be right.
        //
        // The structured literals are kept alongside these because they
        // carry the COMBINED forms (`user:pass`, `host:port`, the whole
        // URL) that a tokenizer cannot produce, and longest-first
        // ordering means those are matched first for tidier output.
        for token in trimmed.split([':', '/', '?', '#', '@', '&', '=']) {
            // Purely numeric tokens are ports and version numbers, never
            // secrets — and redacting every occurrence of `8545` would
            // corrupt any message that happened to mention block 8545.
            // A numeric API key is still covered by the whole-URL literal
            // and by the URL-shape pass, which is where it appears.
            if !token.is_empty() && !token.bytes().all(|b| b.is_ascii_digit()) {
                literals.push(token.to_string());
            }
        }

        literals.retain(|l| l.len() >= MIN_LITERAL_SECRET_LEN);
        literals.iter_mut().for_each(|l| *l = l.to_lowercase());
        literals.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        literals.dedup();
        Redactor { literals }
    }

    /// Runs all three passes. Safe to apply to already-redacted text —
    /// the markers themselves contain no scheme, no configured literal
    /// and no userinfo shape, so a second application is a no-op.
    pub fn apply(&self, text: &str) -> String {
        let stage1 = redact_url_runs(text);
        let stage2 = self.redact_literals(&stage1);
        redact_userinfo_runs(&stage2)
    }

    /// Pass 2. Case-insensitive, because DNS is and because reqwest
    /// lowercases hosts, so a configured `Node.Example` and a reported
    /// `node.example` are the same secret.
    fn redact_literals(&self, text: &str) -> String {
        if self.literals.is_empty() {
            return text.to_string();
        }
        let mut out = text.to_string();
        for literal in &self.literals {
            out = replace_ignore_ascii_case(&out, literal, REDACTED_CREDENTIAL);
        }
        out
    }

    /// Whether this redactor holds any endpoint literals — used only by
    /// tests and diagnostics.
    pub fn literal_count(&self) -> usize {
        self.literals.len()
    }
}

/// Pass 1: replace every `scheme://…` run with [`REDACTED_ENDPOINT`].
///
/// A run starts at the beginning of the scheme (walking back over the
/// characters RFC 3986 allows in one) and ends at the first character
/// that cannot appear unescaped in a URL in running prose.
fn redact_url_runs(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut emitted = 0usize;
    let mut i = 0usize;
    while i + 3 <= bytes.len() {
        if &bytes[i..i + 3] != b"://" {
            i += 1;
            continue;
        }
        // Walk back over the scheme.
        let mut start = i;
        while start > emitted {
            let c = bytes[start - 1];
            if c.is_ascii_alphanumeric() || c == b'+' || c == b'-' || c == b'.' {
                start -= 1;
            } else {
                break;
            }
        }
        // A scheme must be non-empty and begin with a letter; otherwise
        // this `://` is not a URL and is left alone.
        if start == i || !bytes[start].is_ascii_alphabetic() {
            i += 1;
            continue;
        }
        let mut end = i + 3;
        while end < bytes.len() && !is_url_terminator(bytes[end]) {
            end += 1;
        }
        out.push_str(&text[emitted..start]);
        out.push_str(REDACTED_ENDPOINT);
        emitted = end;
        i = end;
    }
    out.push_str(&text[emitted..]);
    out
}

/// Characters that end a URL when one appears inside human-readable
/// text. `)` matters most: reqwest's own phrasing is
/// `for url (https://…)`.
fn is_url_terminator(c: u8) -> bool {
    c.is_ascii_whitespace()
        || matches!(
            c,
            b'(' | b')'
                | b'<'
                | b'>'
                | b'"'
                | b'\''
                | b'`'
                | b','
                | b';'
                | b'['
                | b']'
                | b'{'
                | b'}'
                | b'|'
                | b'\\'
                | b'^'
        )
}

/// Pass 3: replace any remaining whitespace-delimited run that has
/// `something:something@something` shape.
///
/// Requires BOTH a `:` before the `@` and a non-empty remainder after it,
/// so an ordinary `label: value@thing` phrase or a lone email-looking
/// token is left alone — a run only matches when it looks like userinfo,
/// not merely because it contains an `@`.
fn redact_userinfo_runs(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for run in split_keeping_whitespace(text) {
        if run.chars().next().is_some_and(char::is_whitespace) {
            out.push_str(run);
            continue;
        }
        match run.rfind('@') {
            Some(at) if at + 1 < run.len() && run[..at].contains(':') => {
                out.push_str(REDACTED_CREDENTIAL);
            }
            _ => out.push_str(run),
        }
    }
    out
}

/// Splits into alternating runs of whitespace and non-whitespace,
/// preserving every character — so a redacted message keeps its original
/// spacing rather than being re-joined with single spaces.
fn split_keeping_whitespace(text: &str) -> Vec<&str> {
    let mut runs = Vec::new();
    let mut start = 0usize;
    let mut current_is_ws: Option<bool> = None;
    for (i, c) in text.char_indices() {
        let is_ws = c.is_whitespace();
        match current_is_ws {
            None => current_is_ws = Some(is_ws),
            Some(prev) if prev != is_ws => {
                runs.push(&text[start..i]);
                start = i;
                current_is_ws = Some(is_ws);
            }
            _ => {}
        }
    }
    if start < text.len() {
        runs.push(&text[start..]);
    }
    runs
}

/// ASCII-case-insensitive substring replacement.
///
/// Only ASCII case is folded: a hostname and a hex API key are ASCII, and
/// full Unicode case folding can change a string's byte length, which
/// would make the match offsets found in the lowercased copy wrong for
/// the original.
fn replace_ignore_ascii_case(haystack: &str, needle: &str, replacement: &str) -> String {
    if needle.is_empty() {
        return haystack.to_string();
    }
    let hay_lower = haystack.to_ascii_lowercase();
    let needle_lower = needle.to_ascii_lowercase();
    let mut out = String::with_capacity(haystack.len());
    let mut cursor = 0usize;
    while let Some(found) = hay_lower[cursor..].find(&needle_lower) {
        let at = cursor + found;
        out.push_str(&haystack[cursor..at]);
        out.push_str(replacement);
        cursor = at + needle_lower.len();
    }
    out.push_str(&haystack[cursor..]);
    out
}

#[cfg(test)]
mod tests;
