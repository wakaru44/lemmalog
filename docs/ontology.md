# The ontology file

> **Enforcement is not implemented.** `ontology.yaml` is, today, a
> declaration and nothing more. No code reads it. The engine still decides
> relation semantics by prefix match inside `apply_update`
> (`src/agent.rs`). This document explains what the file says, why it
> exists, and what would have to change for the engine to obey it.

## What a relation has to declare

lemmalog stores `Subject --relation[confidence]--> Object`, bitemporally.
When a second value arrives for the same `(Subject, relation)` pair, the
store has to pick one of three behaviours:

- **accumulate** — both edges stay open (`calls`, `evidence`, `part_of`);
- **supersede** — the old edge's `valid_to` is closed at `now` and the new
  one opens (`status`, `email`, `version`);
- **escalate** — neither is obviously right, so the conflict goes to a
  human queue.

That choice is *the* semantics of the relation. Everything else — domain,
range, evidence obligation — is a guard rail around it.

## Why prefix matching is a bug, not a feature

`apply_update` holds two hardcoded arrays:

```rust
const MULTI: [&str; 15] = [
    "evidence", "mentions", "located", "describes", "tag", "related_to",
    "depends_on", "owns", "calls", "part_of", "source", "cites",
    "symptom_of", "aka", "alias_of",
];
const FUNCTIONAL: [&str; 7] = [
    "status", "phone", "address", "email", "version", "value_of",
    "current_value",
];
```

and classifies with:

```rust
let multi      = MULTI.iter().any(|m| pred_name.starts_with(m));
let functional = FUNCTIONAL.iter().any(|f| pred_name.starts_with(f));
```

`starts_with`, not equality. Three consequences, all silent:

1. **Accidental capture.** A relation called `status_code` is single-valued
   because it starts with `status`. `sourced_from` accumulates because it
   starts with `source`. Nobody chose that; the string did.
2. **Inheritance by spelling.** A relation invented at write time — which
   is the normal case, because these stores are built while reading an
   unfamiliar system — inherits whatever prefix it happens to collide with.
   `owns_status` accumulates only because it starts with `owns`. The right
   answer, by luck.
3. **Silent fallthrough.** A relation matching no prefix at all is neither
   multi nor functional, so the second value escalates as a conflict.
   `sets_status` and `reads_status` are both naturally many-valued and both
   land here: every additional caller produces a bogus conflict.

The failure mode is not an error. It is a store that quietly means
something other than what was written into it, discovered weeks later when
a query returns one row where it should return nine — or nine where the
temporal projection should have closed eight.

A declared vocabulary fixes this by construction: the name is looked up,
not pattern-matched, and a name that is not in the file is a write error
rather than a guess.

## How `ontology.yaml` maps onto the existing mechanisms

| Today | In the file |
| --- | --- |
| name matches a `MULTI` prefix | `cardinality: accumulating` |
| name matches a `FUNCTIONAL` prefix | `cardinality: single` |
| `exclusive("works_at")` fact in `DEFAULT_RULES` | `cardinality: single` |
| name matches nothing → escalate on second value | not expressible; every relation declares a cardinality |

Each relation in `ontology.yaml` carries a `legacy:` block recording which
array and prefix classifies it today, or `array: none` where nothing does.
That block is documentation of the migration, not input to it; it can be
deleted once the arrays are gone.

### `exclusive` versus `FUNCTIONAL`

There are two mechanisms for single-valuedness, and they overlap:

```rust
let exclusive = functional || !self.engine.query("exclusive", &[Some(pred)]).is_empty();
if exclusive && !multi { /* supersede */ }
```

- `FUNCTIONAL` is a **compile-time prefix list** in Rust.
- `exclusive/1` is a **runtime Datalog table**, seeded in `DEFAULT_RULES`
  with the single fact `exclusive("works_at")` and extendable by any
  installed rule batch.

They mean the same thing and are combined with `||`. `MULTI` then overrides
both — `multi` wins even against an explicit `exclusive` declaration, so
today you cannot mark an accumulating-prefixed relation exclusive at all.

This file treats `cardinality` as the single answer and resolves the
overlap in favour of the declaration:

- `cardinality: single` is what `FUNCTIONAL` and `exclusive` were both
  trying to say. `works_at` is declared here for exactly that reason.
- `exclusive/1` keeps a job the file cannot do: it is *derivable*. A rule
  batch can conclude exclusivity from other facts. So the intended end
  state is **union, not replacement** — a relation is single-valued if the
  ontology says so **or** if `exclusive` derives it — with the `MULTI`
  override deleted, because an ontology that declares `accumulating` and a
  rule that derives `exclusive` is a contradiction worth reporting, not
  silently resolving in favour of one side.

## Strict relations, lenient entity kinds

```yaml
policy:
  unknown_relations: reject
  unknown_entity_kinds: accept_and_flag
```

The asymmetry is deliberate.

**Relations are rejected** because a relation is the unit the update policy
acts on. An unknown relation has no cardinality, so the store must guess,
and guessing is the bug this file exists to remove. The cost of rejection
is low: the writer adds four lines to `ontology.yaml` and retries. The act
of adding them is the review.

**Entity kinds are accepted and flagged** because the vocabulary of a
system under investigation is genuinely open. You are reading unfamiliar
code and you meet a kind of thing nobody has named yet — a queue, a
scheduled job, a report definition. Blocking the write until the prefix is
registered stalls the investigation at exactly the moment the knowledge is
freshest, and the predictable response is that the writer reaches for a
prefix that is already allowed and means something else. Better a flagged
`job:` than a dishonest `module:`. The flag is the backlog: promote it into
`entity_kinds` or correct it later, in the quiet.

Same principle for `requires_evidence`, which warns rather than rejects.
The evidence fact usually arrives a line later in the same batch; failing
the claim because its citation has not landed yet would push writers to
stop claiming.

## Migration note — what `apply_update` would have to do

Described, not implemented. Roughly, in order:

1. **Load.** Parse `ontology.yaml` once at `AgentMemory::new`, into a
   `HashMap<String, RelationDef>` plus a `HashSet<String>` of entity kinds.
   The file path belongs next to the snapshot, and a missing file should
   fall back to today's behaviour so existing stores keep loading.
2. **Persist.** A snapshot that was written under one ontology and loaded
   under another is a real hazard. Record the ontology `version` (and
   ideally a hash of the file) in the snapshot header, and warn on
   mismatch.
3. **Validate at the boundary.** In `apply_update`, before any `sym()`
   call: look up `c.pred`. Unknown → return a rejection into
   `IngestReport` (a new `rejected` counter alongside `added` / `updated` /
   `noop`), not a panic. Then split the `prefix:` off `c.subj` and `c.obj`
   and check them against the relation's `domain` / `range`; an unknown
   prefix is a flag on the report, a known-but-wrong one is a rejection.
4. **Replace the classification.** Delete `MULTI`, `FUNCTIONAL` and the
   `starts_with` calls. `let single = def.cardinality == Single ||
   exclusive_derived(pred);` and branch on that. The `&& !multi` guard goes
   away with the arrays.
5. **Evidence obligation.** `requires_evidence` cannot be checked per-fact
   — the sibling may arrive later in the batch — so it is a post-batch
   sweep at the end of `observe_at`, appending warnings to the report.
6. **Report, don't panic.** Every check above is a boundary check on
   agent-authored input. All of it lands in `IngestReport` so the caller
   can decide; none of it aborts ingestion.

Step 4 is the one that changes behaviour, and it will change it for
existing stores: relations that were accumulating by prefix accident
become single-valued or rejected. That is the point, but it wants a dry-run
mode — classify under both schemes, report the disagreements — before it
is switched on.
