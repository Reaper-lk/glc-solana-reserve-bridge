# Atomic amounts on the public API are JSON strings (2026-09-05)

**Status:** implemented, backend side. Requires the coordinated UI change
described under "Rollout".

## The defect

`GET /stats` served:

```json
"goldcoin_reserve": { "settled_volume_atomic": 9408405829927559 }
```

JSON has one numeric type, and every mainstream JavaScript client parses it
into an IEEE-754 double. Doubles represent integers exactly only up to
`Number.MAX_SAFE_INTEGER` = `2^53 - 1` = `9007199254740991`. The value above
is larger, so `JSON.parse` returned **`9408405829927560`** — a different
number, wrong by one atomic unit, with no error raised anywhere.

The bridge UI validates responses before rendering them and refused the
out-of-range value, so the Reserves page showed:

> The bridge returned data this page could not read.

**The UI was right to refuse.** The corruption happens inside the client's
JSON parser, before any validation can run. Relaxing the check would not
have recovered the original digits; it would only have replaced an error
with a silently wrong balance on a page whose entire purpose is reporting
reserve figures accurately. The wire format had to change.

This was not a `/stats` bug. Every atomic amount on every endpoint had the
same defect; `settled_volume_atomic` was simply the first to grow past
`2^53`.

## The change

Atomic monetary amounts are now serialized as **decimal strings**:

```json
"goldcoin_reserve": { "settled_volume_atomic": "9408405829927559" }
```

JSON strings survive every parser byte-for-byte, so a client receives the
exact digits and can widen them to a `BigInt`. Negative amounts keep a
leading `-` (`"-1"`). No hex, no scientific notation, no object wrapper —
the plainest encoding that round-trips and stays readable in a response
body.

Implemented as `api::atomic::AtomicU64` / `AtomicI64`, so the encoding is
defined once rather than per field.

## Which fields changed

Every field below now serializes as a string. This is the complete list;
`api::tests::every_atomic_field_on_every_public_dto_is_a_json_string`
enforces it and fails if a new atomic field is added as a number.

| Endpoint | Field(s) |
|---|---|
| `GET /status` | `glc_to_sol_rolling_volume_remaining`, `sol_to_glc_rolling_volume_remaining` |
| `GET /limits` | `min_transfer_amount`, `per_transfer_limit` |
| `GET /reserve` | `goldcoin_available_capacity`, `solana_available_capacity` |
| `GET /stats` | `glc_to_sol_rolling_volume_remaining`, `sol_to_glc_rolling_volume_remaining`, and on both `goldcoin_reserve` and `solana_reserve`: `available_capacity`, `settled_volume_atomic`, `accrued_fees_atomic` |
| `GET /reserves/history` | `expected_atomic`, `observed_atomic`, `delta_atomic` |
| `GET /transfers`, `GET /transfers/:id` | `gross_amount_atomic`, `fee_amount_atomic`, `net_amount_atomic` |
| `POST /quote` | `gross_amount`, `fee_amount`, `net_amount` |
| `POST /transfers` (input) | `amount_atomic` — **accepts both** forms, see Compatibility |
| `POST /quote` (input) | `gross_amount` — **accepts both** forms |

## Which fields deliberately did NOT change

These stay plain JSON numbers. Each is bounded by something real and none
can approach `2^53`, so a string would add friction for every client with
no correctness gain:

| Field(s) | Bound |
|---|---|
| `bridge_fee_bps`, `fee_bps` | basis points, `0..=10_000` |
| `source_decimals`, `destination_decimals` | `0..=30` |
| `total_requests`, `in_progress_requests`, `settled_requests`, `manual_review_requests`, `manual_review_backlog` | row counts |
| `id`, `request_id` | SQLite rowids |
| `as_of`, `detected_at`, `created_at`, `at`, `retry_after`, `retry_after_seconds`, `window_seconds`, `*_seconds_since_tick` | unix seconds |
| `source_confirmations`, `required_source_confirmations` | block depth |
| `post_finality_reorg_events` | event count |
| `next_solana_obligation_index` | a per-deposit counter; reaching `2^53` needs ~9 quadrillion deposits |

`gross_display_amount` / `fee_display_amount` / `net_display_amount` were
already strings and are unchanged — they are *formatted decimals* for
display (`"12.34500000"`), not atomic integers, and the two must not be
confused.

## Compatibility

**Responses: breaking.** A field that was a number is now a string. There
is no way to be lossless and unchanged at the same time — that is the whole
defect. Adding parallel `*_str` fields was considered and rejected: it
would leave the wrong value on the wire indefinitely, and any client still
reading the numeric field would keep silently mis-reporting balances.

**Requests: unchanged.** `POST /transfers` and `POST /quote` accept an
atomic amount as *either* a JSON number (existing clients, unaffected) or a
decimal string (new, and the only way to send a value above `2^53`). A
float is refused outright rather than truncated.

### Rollout

Deploy the **UI first**, then the backend. The updated UI schemas accept
either form, so they work against both the old and the new backend:

- new backend → string → exact;
- old backend, value ≤ `2^53 - 1` → number → exact;
- old backend, value > `2^53 - 1` → unsafe number → **rejected**, which is
  the behaviour that exists today and is not a regression.

That last case is deliberate. Accepting an unsafe number would mean
rendering a corrupted balance, which is worse than the error it replaces.

## Tests

- `api::atomic::tests` — encoding, round-trip, and boundary tests at
  `2^53 - 1`, `2^53`, the production value `9408405829927559`, and up to
  `u64::MAX`; float and malformed-string refusals; the number-input
  compatibility path. Includes a test proving the *old* representation was
  genuinely lossy for the production value.
- `api::tests::the_production_stats_payload_serializes_every_atomic_amount_as_a_string`
  — the live-payload regression fixture, the exact `/stats` shape with the
  real production figure.
- `api::tests::every_atomic_field_on_every_public_dto_is_a_json_string` —
  walks serialized payloads for all seven affected endpoints and fails on
  any atomic field that is not a string. Self-checked against a
  deliberately-regressed payload so it cannot pass vacuously.
- `api::tests::transfer_and_quote_inputs_accept_both_a_number_and_a_string`.
