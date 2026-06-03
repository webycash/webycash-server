//! SQL constants for the Cloudflare D1 schema used by `WorkerProxyStore`.
//! The Container's native binary serializes these statements as JSON and
//! the TS edge Worker translates them into `D1Database.prepare(sql).bind()`
//! calls, packed atomically through `D1Database.batch([...])`.
//!
//! Schema definitions live in
//! `deploy/cloudflare-container/migrations/0001_initial.sql`. The
//! trigger-based atomicity primitives there (the `BEFORE UPDATE OF spent`
//! trigger on `tokens` plus the `INSTEAD OF INSERT` view trigger on
//! `assert_token_exists`) are what make `db.batch(...)` rollbacks correct
//! under contention.

/// `INSERT` a freshly-minted token row.
pub const INSERT_TOKEN: &str =
    "INSERT INTO tokens (pk, amount_wats, spent, data) VALUES (?1, ?2, 0, ?3)";

/// Flip a token's `spent` column to 1; the `BEFORE UPDATE OF spent`
/// trigger raises `ABORT 'double_spend'` if the row was already spent.
pub const UPDATE_SPENT: &str = "UPDATE tokens SET spent = 1 WHERE pk = ?1";

/// `INSERT INTO` the existence-assertion view; the INSTEAD-OF-INSERT
/// trigger on the view raises `ABORT 'input_missing'` if the referenced
/// row isn't in `tokens`.
pub const ASSERT_TOKEN_EXISTS: &str = "INSERT INTO assert_token_exists (pk) VALUES (?1)";

/// Append an audit-log row.
pub const INSERT_AUDIT: &str = "INSERT INTO audit_log (pk, data) VALUES (?1, ?2)";

/// Upsert the per-asset MiningState singleton.
pub const UPSERT_MINING_STATE: &str = "\
    INSERT INTO mining_state (pk, data) VALUES (?1, ?2) \
    ON CONFLICT(pk) DO UPDATE SET data = excluded.data";

/// Read the per-asset MiningState singleton.
pub const SELECT_MINING_STATE: &str = "SELECT data FROM mining_state WHERE pk = ?1";

/// SQLite's bound-parameter limit is 999. Chunking at 500 leaves headroom
/// for the per-statement bookkeeping placeholders and matches the
/// chunking sizes used by the DynamoDB BatchGetItem path.
pub const READ_CHUNK: usize = 500;
