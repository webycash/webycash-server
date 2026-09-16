-- webycash-server schema for Cloudflare D1 (SQLite).
--
-- Mirrors the DynamoDB layout: one row per token keyed by the
-- KeyStrategy-rendered `pk`, a singleton-per-asset mining_state, and an
-- append-only audit_log. The trigger below is the LOAD-BEARING atomicity
-- primitive: a naive `UPDATE tokens SET spent = 1 WHERE pk = ?1 AND spent = 0`
-- silently commits zero rows on contention, which would break double-spend
-- protection inside a `db.batch(...)` transaction. The trigger converts
-- `UPDATE OF spent` on an already-spent row into a real SQLite `ABORT`,
-- which D1 propagates as a batch failure and rolls everything back.
--
-- Apply locally:
--     wrangler d1 migrations apply <db-name> --local
-- Apply to a deployed database:
--     wrangler d1 migrations apply <db-name> --remote

CREATE TABLE IF NOT EXISTS tokens (
    pk          TEXT PRIMARY KEY,
    amount_wats INTEGER NOT NULL,
    spent       INTEGER NOT NULL DEFAULT 0,
    data        TEXT    NOT NULL
);

CREATE TABLE IF NOT EXISTS mining_state (
    pk    TEXT PRIMARY KEY,
    data  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS audit_log (
    pk    TEXT PRIMARY KEY,
    data  TEXT NOT NULL
);

CREATE TRIGGER IF NOT EXISTS tokens_no_double_spend
BEFORE UPDATE OF spent ON tokens
FOR EACH ROW
WHEN OLD.spent = 1
BEGIN
    SELECT RAISE(ABORT, 'double_spend');
END;

-- Existence assertion. SQLite forbids RAISE() in standalone SELECT, but
-- allows it inside trigger bodies. We define a view whose ONLY purpose is
-- to host an INSTEAD-OF-INSERT trigger that fires per-row, runs an
-- existence check, and RAISE(ABORT)s if the target `pk` is missing from
-- tokens. The trigger is INSTEAD OF, so the view's underlying rows are
-- never actually touched. Atomic-replace / atomic-burn batches INSERT
-- into this view as a no-op-or-abort assertion.
CREATE VIEW IF NOT EXISTS assert_token_exists AS SELECT pk FROM tokens;

CREATE TRIGGER IF NOT EXISTS assert_token_exists_check
INSTEAD OF INSERT ON assert_token_exists
FOR EACH ROW
BEGIN
    SELECT CASE
        WHEN NOT EXISTS(SELECT 1 FROM tokens WHERE pk = NEW.pk)
        THEN RAISE(ABORT, 'input_missing')
    END;
END;
