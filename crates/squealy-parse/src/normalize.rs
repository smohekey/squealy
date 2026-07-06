//! Cross-dialect normalization — folding dialect spellings to one neutral node, and unwinding the
//! renderer's own idioms.
//!
//! [`sqlparser`] hands back a faithful, dialect-spelled AST and does no
//! canonicalization. Two kinds of transformation stand between that AST and a neutral
//! [`squealy::ExprNode`], and both live here (the lowering in [`crate::lower`] calls into them):
//!
//! 1. **Dialect-spelling folds** — the same neutral operation is written differently per dialect. The
//!    renderer chooses the spelling via a [`squealy::Dialect`] hook; normalization inverts that choice
//!    knowing the input [`crate::SqlDialect`].
//! 2. **Render-idiom unwinding** — the renderer wraps/expands nodes in ways that must be peeled to
//!    recover the original structural node. Several are dialect-*independent* (always emitted) and are
//!    unwound unconditionally.
//!
//! # Fold catalogue (render → the neutral node to recover)
//!
//! Line references are into `crates/squealy/src/view_render.rs` at the time of writing (the oracle);
//! the query-path renderer `crates/squealy/src/render.rs` emits the byte-identical forms.
//!
//! ## Dialect-spelling folds
//! - **Concatenation**: `a || b` (PostgreSQL/SQLite, `concat_uses_pipe_operator`) vs `CONCAT(a, b)`
//!   (MySQL) → [`squealy::ExprNode::ScalarFn`] `Concat`. Semantics match (both propagate `NULL` in the dialects
//!   that use each spelling), so the fold is safe — but MySQL `||` is logical `OR`, so the spelling
//!   must be interpreted per dialect.  (`view_render.rs` ~580 / 612)
//! - **Substring**: `substr(s, start, len)` (SQLite, `substring_uses_function_call`) vs
//!   `SUBSTRING(s FROM start FOR len)` → [`squealy::ExprNode::ScalarFn`] `Substring`.  (~592)
//! - **`now()`**: `CURRENT_TIMESTAMP` vs `CURRENT_TIMESTAMP(6)` (MySQL, `now_fractional_digits`) →
//!   [`squealy::ExprNode::Now`].  (~624)
//! - **Character length**: `length` (SQLite, `unary_string_fn_name`) vs `CHAR_LENGTH` →
//!   [`squealy::ExprNode::ScalarFn`] `Length`.  (helper `scalar_func_name`)
//! - **`extract_second`**: `EXTRACT(SECOND FROM x)` (PostgreSQL) vs
//!   `EXTRACT(SECOND_MICROSECOND FROM x) / 1000000.0` (MySQL, `extract_second_uses_microsecond_unit`)
//!   → [`squealy::ExprNode::ExtractSecond`].  (~683)
//! - **`ILIKE`**: PostgreSQL `ILIKE` vs `LIKE` (`write_like_operator`) → the `case_insensitive` flag on
//!   [`squealy::ExprNode::Like`].
//! - **Cast type names + identifier quoting**: `write_cast_type` / `write_quoted_ident` — invert via the
//!   dialect's spelling.
//!
//! ## Render-idiom unwinding (mostly dialect-independent — always emitted)
//! - **Full parenthesization**: every operator/predicate node is wrapped in `(...)`. Strip redundant
//!   parentheses and recover precedence.  (Binary/Compare/Logical/Not/IsNull/Like/In/Between/…)
//! - **`CAST(<call> AS ty)` result-pins**: [`squealy::ExprNode::Aggregate`]/[`squealy::ExprNode::Window`]/
//!   [`squealy::ExprNode::Extract`]/[`squealy::ExprNode::ExtractSecond`] are wrapped in an outer `CAST` when their
//!   `result` is set. Peel the outer `CAST` into the node's `result` field — distinct from a
//!   user-written cast.  (~307 / 449 / 643)
//! - **Per-branch casts in `CASE`/`NULLIF`/`COALESCE`**: casts are applied inside each branch, and only
//!   when the branch operands are all literals (`render_case_value`). Recovering these needs the same
//!   literal-vs-column heuristic to tell a result-pin from a user cast.  (~758)
//! - **Float-cast division**: `(CAST(l AS <f64>) / CAST(r AS <f64>))` (PostgreSQL,
//!   `integer_division_needs_float_cast`) → a plain [`squealy::ExprNode::Binary`] `Divide`.  (~274)
//! - **Empty `IN ()`**: rendered as `(<op> IS NOT NULL AND 1 = 0)` (or `… OR 1 = 1` when negated),
//!   since SQL has no `IN ()`. Recognize the sentinel and recover an empty [`squealy::ExprNode::In`].  (~373)
//! - **`FLOOR(EXTRACT(SECOND …))`**: the `Second` field of [`squealy::ExprNode::Extract`] is wrapped in `FLOOR`
//!   (whole-second extraction, distinct from fractional [`squealy::ExprNode::ExtractSecond`]).  (~646)
//! - **`AT TIME ZONE`**: an `Extract`/`DateTrunc` with a timezone renders its operand as
//!   `(<operand> AT TIME ZONE '<tz>')`.  (~714)
//!
//! # Semantics safety
//!
//! A fold is only valid where the dialect semantics genuinely match; where they diverge (NULL handling,
//! integer-vs-float division, collation-dependent comparison), the neutral node must carry a canonical
//! semantic the renderer reproduces per dialect, or normalization must refuse to fold. Each fold added
//! here carries that judgement as a first-class review criterion, not an afterthought.
//!
//! # Status (Phase 0)
//!
//! This module is the design reference; the fold/unwind implementations land alongside the lowering in
//! [`crate::lower`], one phase at a time (checks / generated / index expressions first).
