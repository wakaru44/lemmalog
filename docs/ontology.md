# The ontology file

> **Enforcement is implemented, and it is opt-in.** `apply_update`
> (`src/agent.rs`) consults a loaded ontology before it touches the store:
> a declared relation's `cardinality` decides the update policy, and a
> blocking violation keeps the fact out of the store entirely. Nothing is
> loaded unless you ask for it — set `LEMMALOG_ONTOLOGY` to the file's path,
> or call `set_ontology` / `set_ontology_path` / `with_ontology`. With no
> ontology in force the prefix heuristic below is still what runs, unchanged,
> so every existing store keeps behaving exactly as it did. This document
> explains what the file says, why it exists, and what the engine now does
> with it.

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

### Precedence: a declaration beats the prefix arrays

The arrays are still in `apply_update`, but they are now the *fallback*, not
the decision. For each candidate fact:

1. **Declared in a loaded ontology** — `cardinality: single` supersedes,
   `cardinality: accumulating` accumulates. The `MULTI` / `FUNCTIONAL` prefix
   match and the `exclusive/1` table are not consulted; the declaration is the
   answer. A declared `accumulating` relation never raises a second-value
   conflict, because the file has already said many values are legitimate.
2. **No ontology loaded** — exactly the old behaviour: `MULTI` /
   `FUNCTIONAL` prefix match, `exclusive` OR-ed in, `MULTI` overriding both,
   and an unmatched name escalating on its second value.

With an ontology in force there is no third case: `unknown_relations: reject`
means an undeclared name never reaches the classification step at all. The
two schemes therefore never fight — the file answers every fact it admits,
and the prefix guess runs only where no file is loaded.

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

### Blocking and non-blocking violations

`Violation::is_blocking` (`src/ontology.rs`) is where that asymmetry is
actually decided, one predicate for the whole system:

| Violation | Blocking? | Effect on the fact |
| --- | --- | --- |
| unknown relation | yes | not stored |
| subject outside the declared `domain` | yes | not stored |
| object outside the declared `range` | yes | not stored |
| unknown entity kind (`prefix:`) | no | stored, flagged |
| `requires_evidence` with no sibling `evidence` | no | stored, flagged |

A candidate with any blocking violation is refused: `apply_update` returns
before it interns a single symbol, so nothing about the fact reaches the
store, not even a closed edge. Its explanation is pushed onto
`IngestReport::rejected`. Non-blocking violations do not stop the write; each
one is appended to `IngestReport::escalations` and the fact is stored
normally.

### What an operator sees when a fact is rejected

Rejections are reported on the same summary line as everything else, and only
when there are any — a run under no ontology prints what it always printed.

```
$ LEMMALOG_ONTOLOGY=ontology.yaml lemmalog-cli observe \
    --facts 'svc:x --frobnicates--> svc:y'
added=0 updated=0 noop=0 escalations=0 rejected=1
rejected: svc:x --frobnicates--> svc:y (unknown relation `frobnicates`)
```

The `lemmalog_observe` MCP tool carries the same information in its response
text, listed ahead of the escalations, so a calling agent is told its fact was
refused and why instead of inferring silence from `added=0` and retrying the
same bad write:

```
added=0 updated=0 noop=0 escalations=0 rejected=1
1 fact(s) refused by the ontology — NOT asserted:
  rejected: svc:x --frobnicates--> svc:y (unknown relation `frobnicates`)
```

A non-blocking violation reads differently, and deliberately so: the fact is
in the store and the flag is a backlog item, not a failure.

```
$ LEMMALOG_ONTOLOGY=ontology.yaml lemmalog-cli observe \
    --facts 'entity:invoice --stored_in--> table:invoices'
added=1 updated=0 noop=0 escalations=1
```

The fix for a rejection is always the same: declare the relation in
`ontology.yaml`, or correct the fact. There is no override flag, because an
override would reintroduce the guess the file exists to remove.

## Migration note — what `apply_update` does, and what is still open

The plan below was written before enforcement existed. Steps 1, 3, 4 and 6
have shipped, with one deliberate change: step 4 keeps the arrays as a
fallback instead of deleting them, which is what makes the switch-on
non-breaking. Steps 2 and 5 are still open. Annotations follow each step.

1. **Load.** Parse `ontology.yaml` once at `AgentMemory::new`, into a
   `HashMap<String, RelationDef>` plus a `HashSet<String>` of entity kinds.
   The file path belongs next to the snapshot, and a missing file should
   fall back to today's behaviour so existing stores keep loading.
   *Shipped, opt-in and lazier than planned:* loading is resolved on first
   ingestion, not in `AgentMemory::new`, from `LEMMALOG_ONTOLOGY` or an
   explicit setter. Unset, empty or absent path → nothing enforced. An
   unreadable or malformed file also enforces nothing, but the parse error is
   escalated rather than eaten, so a typo'd path cannot masquerade as a clean
   run.
2. **Persist.** A snapshot that was written under one ontology and loaded
   under another is a real hazard. Record the ontology `version` (and
   ideally a hash of the file) in the snapshot header, and warn on
   mismatch. *Still open.* Nothing records which ontology a store was
   written under.
3. **Validate at the boundary.** In `apply_update`, before any `sym()`
   call: look up `c.pred`. Unknown → return a rejection into
   `IngestReport` (a new `rejected` counter alongside `added` / `updated` /
   `noop`), not a panic. Then split the `prefix:` off `c.subj` and `c.obj`
   and check them against the relation's `domain` / `range`; an unknown
   prefix is a flag on the report, a known-but-wrong one is a rejection.
   *Shipped* as `ontology_verdict`, called as the first statement of
   `apply_update` — before any `sym()` call, so a refused fact leaves no
   trace at all. The rejection carries a written explanation rather than
   only bumping a counter.
4. **Replace the classification.** Delete `MULTI`, `FUNCTIONAL` and the
   `starts_with` calls. `let single = def.cardinality == Single ||
   exclusive_derived(pred);` and branch on that. The `&& !multi` guard goes
   away with the arrays.
   *Shipped, but as an override rather than a deletion.* The arrays and the
   `exclusive && !multi` expression are still there and still run — for
   undeclared relations, which under a loaded ontology means only when no
   ontology is loaded at all. See "Precedence" above.
5. **Evidence obligation.** `requires_evidence` cannot be checked per-fact
   — the sibling may arrive later in the batch — so it is a post-batch
   sweep at the end of `observe_at`, appending warnings to the report.
   *Still open.* It is checked per-fact today, so a claim whose `evidence`
   lands later in the same batch is flagged anyway. The flag is
   non-blocking, so this costs noise, not facts.
6. **Report, don't panic.** Every check above is a boundary check on
   agent-authored input. All of it lands in `IngestReport` so the caller
   can decide; none of it aborts ingestion.
   *Shipped:* blocking violations land in `IngestReport::rejected`,
   non-blocking ones in `escalations`, and ingestion of the rest of the
   batch carries on either way.

Step 4 was the one expected to change behaviour for existing stores:
relations that were accumulating by prefix accident would become
single-valued or rejected. Making enforcement opt-in is what defused that —
nothing is enforced until a store's owner points `LEMMALOG_ONTOLOGY` at a
file, and the fallback keeps every undeclared relation on its old path. The
dry-run mode the original note asked for — classify under both schemes and
report the disagreements — is still worth having, and is still open.
