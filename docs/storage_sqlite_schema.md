# SQLite storage — schema draft

Branch `w44/feat/alt_storage`. Replaces the single-snapshot persistence
(`AgentMemory::save` / `AgentMemory::load`, `src/agent.rs:1121`, `:1189`) with SQLite via
`sqlx`. Draft for argument, not yet implemented.

## What persistence has to carry

Enumerated from `save()` — this is the complete set, nothing else is
durable:

| Snapshot line | Meaning |
|---|---|
| `NOW <i64>` | engine clock |
| `RULES <src>` | `extra_rules`, installed at construction |
| `RULEB <id> <src>` | batches installed later; ids positional (`b{n}`) |
| `EP <id> <ts> <speaker> <text>` | episodes, verbatim source text |
| `ESC <text>` | escalation queue |
| `FACT <pred> <conf> <prov,…> <args…>` | base (EDB) facts, n-ary |

Derived facts are **not** persisted — they are rebuilt by one
maintenance run on load. The schema must preserve that: storing
derivations would be storing a cache we have no way to invalidate.

## The one real modelling decision

The engine holds facts as generic n-ary predicates (`relations: pred ->
rows of Sym|Int`). A faithful schema is therefore one generic table.

But one predicate carries all the weight:

```
edge(subject, predicate, object, valid_from, valid_to, asserted_at)
       [0]        [1]       [2]       [3]         [4]        [5]
```

`valid_to = i64::MAX` means open (`AgentMemory::assert_open`, `src/agent.rs:527`);
supersession rewrites `[4]` to the current clock.

Every field we are adding — commit provenance, retraction reason, fact
class — is an `edge` concept. So: **specialize `edge`, keep a generic
table for everything else.** Cost is a small amount of special-casing at
the seam. Benefit is that the store stays queryable by anything that
speaks SQL (scripts, Grafana, a human with `sqlite3`) without a Datalog
engine in the loop, and the bitemporal columns get real indexes.

## DDL

```sql
PRAGMA journal_mode = WAL;           -- concurrent readers, one writer

CREATE TABLE meta (                   -- NOW, schema_version, …
  key    TEXT PRIMARY KEY,
  value  TEXT NOT NULL
) STRICT;

CREATE TABLE edges (
  id               INTEGER PRIMARY KEY,
  subject          TEXT    NOT NULL,
  predicate        TEXT    NOT NULL,
  object           TEXT    NOT NULL,

  -- bitemporal. valid_to   = 9223372036854775807 (i64::MAX) -> open.
  --             valid_from = -9223372036854775808 (i64::MIN) -> predates
  --             recorded history; bounded_by names how far we could look.
  valid_from       INTEGER NOT NULL,
  valid_to         INTEGER NOT NULL,
  asserted_at      INTEGER NOT NULL,

  confidence       REAL    NOT NULL CHECK (confidence BETWEEN 0.0 AND 1.0),

  -- ours ---------------------------------------------------------------
  asserted_at_sha  TEXT,                  -- repo HEAD when asserted
  asserted_on_branch TEXT,
  bounded_by       TEXT,                  -- root sha searched, when valid_from = -inf
  fact_class       TEXT NOT NULL DEFAULT 'agent'
                   CHECK (fact_class IN ('machine','agent','human')),
  retract_reason   TEXT
                   CHECK (retract_reason IN ('wrong','world_changed','superseded')),
  retracted_at     INTEGER,
  retracted_by     TEXT
) STRICT;

CREATE INDEX edges_spo      ON edges (subject, predicate, object);
CREATE INDEX edges_open     ON edges (predicate, valid_to);
CREATE INDEX edges_pred_obj ON edges (predicate, object);

-- provenance is a set, not a CSV string (upstream joins it with ',')
CREATE TABLE edge_prov (
  edge_id  INTEGER NOT NULL REFERENCES edges(id) ON DELETE CASCADE,
  prov     TEXT    NOT NULL,              -- episode id, or 'superseded'
  PRIMARY KEY (edge_id, prov)
) STRICT;

-- every non-edge base predicate, n-ary, faithful to the engine
CREATE TABLE facts (
  id          INTEGER PRIMARY KEY,
  predicate   TEXT NOT NULL,
  arity       INTEGER NOT NULL,
  confidence  REAL NOT NULL
) STRICT;

CREATE TABLE fact_args (
  fact_id  INTEGER NOT NULL REFERENCES facts(id) ON DELETE CASCADE,
  pos      INTEGER NOT NULL,
  kind     TEXT    NOT NULL CHECK (kind IN ('sym','int')),
  sym      TEXT,
  int_val  INTEGER,
  PRIMARY KEY (fact_id, pos)
) STRICT;

CREATE TABLE episodes (
  id       TEXT PRIMARY KEY,              -- 'ep17'
  ts       INTEGER NOT NULL,
  speaker  TEXT,
  text     TEXT NOT NULL                  -- verbatim; no length limit here
) STRICT;

CREATE TABLE escalations (
  id    INTEGER PRIMARY KEY,
  text  TEXT NOT NULL
) STRICT;

CREATE TABLE rule_batches (               -- ids are positional; keep order
  ord  INTEGER PRIMARY KEY,
  id   TEXT NOT NULL UNIQUE,
  src  TEXT NOT NULL
) STRICT;
```

Portability: `STRICT` + declared types + explicit integer primary keys +
no `INSERT OR REPLACE` and no `rowid` dependence. Swapping to Postgres
is then a driver change plus `INTEGER PRIMARY KEY -> BIGSERIAL` and
`REAL -> DOUBLE PRECISION`.

## Notes and open issues

**`-inf` costs nothing.** `valid_to` already uses `i64::MAX` as the open
sentinel and `current/3` guards on `VF =< T < VT`. Using `i64::MIN` for
an unbounded `valid_from` is the exact mirror and needs **no engine
change** — it simply always satisfies the guard, which is the correct
semantics for "older than anything we can see."

**Upstream loses provenance on supersession.** In `AgentMemory::apply_update` (`src/agent.rs:501`):

```rust
self.engine.declare("edge", &closed, Ann::base(0.9, ["superseded"]));
```

The closed edge's annotation is rebuilt with the literal prov
`"superseded"` and a hardcoded `0.9`, discarding both the original
episode id and the original confidence. For a months-long archaeology
store that is the wrong trade: the history row survives but can no
longer answer "who asserted this, and how sure were they?" The schema
keeps `edge_prov` per row so the fix is expressible; whether to change
the behaviour is a decision, not a given.

**Cardinality still comes from relation-name prefixes.** The `MULTI` (15) and
`FUNCTIONAL` (7) prefix lists in `AgentMemory::apply_update`
(`src/agent.rs:465`, `:482`) decide single- vs multi-valued by string prefix (`status*`, `owns*`, …). `ontology.yaml` replaces this
with declared cardinality; until it lands the prefix behaviour is
load-bearing and must not be broken.

**Not addressed here, deliberately:** the 8-word / 60-char / ASCII
clipping. That is a parser guard (`entity_token_problem`, `src/agent.rs:107`) against models
leaking prose into facts, not a storage limit. Separate argument.

## Order of work

1. Schema + `sqlx` wiring behind the existing `save`/`load` signatures —
   no behaviour change, snapshot converted in, identical results out.
2. Differential check: load snapshot, save to SQLite, reload, compare
   fact counts and a sample of `why()` proof trees.
3. Then, and only then, the additive columns (commit provenance,
   retraction reasons, fact class).
4. `suspect` as a rule, once `world_changed` can be recorded.
