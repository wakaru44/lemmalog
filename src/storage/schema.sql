-- lemmalog SQLite store. Replaces the single-snapshot file.
-- Portable SQL subset: STRICT tables, declared types, explicit integer
-- primary keys, no INSERT OR REPLACE, no rowid dependence. Swapping to
-- Postgres is then INTEGER PRIMARY KEY -> BIGSERIAL and REAL -> DOUBLE
-- PRECISION.

CREATE TABLE IF NOT EXISTS meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
) STRICT;

-- The `edge` relation, specialized. Every field we add is an edge concept,
-- and real columns keep the store queryable by anything that speaks SQL.
--
--   valid_to   =  9223372036854775807 (i64::MAX) -> open, still true
--   valid_from = -9223372036854775808 (i64::MIN) -> predates recorded
--                history; `bounded_by` names how far back we could look
CREATE TABLE IF NOT EXISTS edges (
  id                 INTEGER PRIMARY KEY,
  subject            TEXT    NOT NULL,
  predicate          TEXT    NOT NULL,
  -- The object is the one edge slot that need not be a symbol: an edge can
  -- point at a number. Same one-of-two shape as `fact_args`, so that
  -- `edges` is the COMPLETE edge set and SQL never has to union in `facts`
  -- to see them all. Subject and predicate are always symbols.
  object_kind        TEXT    NOT NULL DEFAULT 'sym'
                     CHECK (object_kind IN ('sym','int')),
  object             TEXT,
  object_int         INTEGER,

  valid_from         INTEGER NOT NULL,
  valid_to           INTEGER NOT NULL,
  asserted_at        INTEGER NOT NULL,

  confidence         REAL    NOT NULL CHECK (confidence BETWEEN 0.0 AND 1.0),

  -- additive, ours
  asserted_at_sha    TEXT,
  asserted_on_branch TEXT,
  bounded_by         TEXT,
  fact_class         TEXT    NOT NULL DEFAULT 'agent'
                     CHECK (fact_class IN ('machine','agent','human')),
  -- Why the edge stopped being asserted. 'wrong' never reaches the store
  -- (a wrong fact is deleted, it was never true); 'world_changed' and
  -- 'superseded' close `valid_to` and keep the row, which is what makes
  -- "true until when, and why not now" answerable in SQL.
  retract_reason     TEXT    CHECK (retract_reason IN
                                   ('wrong','world_changed','superseded')),
  retracted_at       INTEGER,
  retracted_by       TEXT,

  CHECK ((object_kind = 'sym' AND object IS NOT NULL AND object_int IS NULL)
      OR (object_kind = 'int' AND object_int IS NOT NULL AND object IS NULL)),
  UNIQUE (subject, predicate, object_kind, object, object_int,
          valid_from, valid_to, asserted_at)
) STRICT;

CREATE INDEX IF NOT EXISTS edges_spo      ON edges (subject, predicate, object);
CREATE INDEX IF NOT EXISTS edges_open     ON edges (predicate, valid_to);
CREATE INDEX IF NOT EXISTS edges_pred_obj ON edges (predicate, object);

-- Provenance is a set. Upstream joins it into a CSV string, which cannot
-- be queried and breaks on any id containing a comma.
CREATE TABLE IF NOT EXISTS edge_prov (
  edge_id INTEGER NOT NULL REFERENCES edges(id) ON DELETE CASCADE,
  prov    TEXT    NOT NULL,
  PRIMARY KEY (edge_id, prov)
) STRICT;

-- Every other base predicate, n-ary, faithful to the engine's own shape.
CREATE TABLE IF NOT EXISTS facts (
  id         INTEGER PRIMARY KEY,
  predicate  TEXT    NOT NULL,
  arity      INTEGER NOT NULL,
  confidence REAL    NOT NULL CHECK (confidence BETWEEN 0.0 AND 1.0)
) STRICT;

CREATE INDEX IF NOT EXISTS facts_pred ON facts (predicate);

CREATE TABLE IF NOT EXISTS fact_args (
  fact_id INTEGER NOT NULL REFERENCES facts(id) ON DELETE CASCADE,
  pos     INTEGER NOT NULL,
  kind    TEXT    NOT NULL CHECK (kind IN ('sym','int')),
  sym     TEXT,
  int_val INTEGER,
  PRIMARY KEY (fact_id, pos),
  CHECK ((kind = 'sym' AND sym IS NOT NULL AND int_val IS NULL)
      OR (kind = 'int' AND int_val IS NOT NULL AND sym IS NULL))
) STRICT;

CREATE TABLE IF NOT EXISTS fact_prov (
  fact_id INTEGER NOT NULL REFERENCES facts(id) ON DELETE CASCADE,
  prov    TEXT    NOT NULL,
  PRIMARY KEY (fact_id, prov)
) STRICT;

-- Verbatim source text. No length limit here; the 8-word / 60-char clip
-- is a parser guard (entity_token_problem), not a storage one.
CREATE TABLE IF NOT EXISTS episodes (
  id      TEXT PRIMARY KEY,
  ts      INTEGER NOT NULL,
  speaker TEXT,
  text    TEXT NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS escalations (
  id   INTEGER PRIMARY KEY,
  text TEXT NOT NULL
) STRICT;

-- Batch ids are positional (`b{n}`); replay order is load-bearing, so the
-- ordinal is the primary key rather than the id.
CREATE TABLE IF NOT EXISTS rule_batches (
  ord INTEGER PRIMARY KEY,
  id  TEXT NOT NULL UNIQUE,
  src TEXT NOT NULL
) STRICT;
