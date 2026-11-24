# A set LEMMALOG_ONTOLOGY pointing at a missing file disables enforcement in silence

**Raised:** 2026-09-26 · **Status:** defect, not yet fixed. Small and self-contained.
**Found by:** the per-repo-install foreman run, wiring lane, on the hubos repo.

## What happens

`src/agent.rs::resolve_ontology` (~line 645) collapses three different operator
situations into one outcome:

| Situation | Result | Diagnostic |
| --- | --- | --- |
| `LEMMALOG_ONTOLOGY` unset or empty | enforcement off | none |
| set, file **does not exist** | enforcement off | **none** |
| set, file exists but is malformed | enforcement off | escalation: `ontology: ...failed to load (...) — nothing is being enforced` |
| set, file exists and parses | enforcement on | rejects undeclared relations |

Measured on the hubos wiring, with `LEMMALOG_ONTOLOGY` set and no file present:

```console
$ LEMMALOG_ONTOLOGY=docs/architecture/lemmalog/lemmalog.yaml \
    lemmalog-cli-w44 observe --facts 'svc:a --frobnicates--> svc:b'
added=1 updated=0 noop=0 escalations=0
```

Accepted. Exit 0. No warning. With a valid ontology at that path the same write is
`rejected: svc:a --frobnicates--> svc:b (unknown relation \`frobnicates\`)`.

## Why this is a defect and not the documented behaviour

The doc comment on `resolve_ontology` states the current design deliberately:

> Unset, empty or absent file -> nothing is enforced; unreadable or malformed file
> -> nothing is enforced either, but the parse error is escalated rather than eaten.

So absent-is-silent is intent, not an oversight — and for **unset** it is the right
call: no configuration means no enforcement, and warning about a feature nobody asked
for is noise.

The bug is the middle row. A **set** variable is an operator saying "enforce this
vocabulary". Pointing it at a path that does not exist is a broken configuration, and
it is currently indistinguishable at runtime from having no configuration at all. The
code already accepts that a configured-but-unusable ontology deserves a diagnostic —
that is exactly what the malformed branch does. A missing file is the same class of
failure as an unparseable one, and it is the more likely of the two: a typo'd path, a
file not yet created, a relative path resolved from an unexpected cwd, a checkout where
the file is gitignored.

## How it bit

The hubos repo now has `LEMMALOG_ONTOLOGY=docs/architecture/lemmalog/lemmalog.yaml` in
its committed `.mcp.json`, and that file has not been written yet. Every agent writing
to that store believes a vocabulary is in force. None is. The store accepts any relation
anyone invents and reports nothing — and this is the same store where 75 non-conforming
facts were once shipped, which is the reason the vocabulary was wanted in the first
place.

## The fix

Escalate on set-but-missing, reusing the malformed branch's wording:

```rust
if !std::path::Path::new(&path).exists() {
    report.escalations.push(format!(
        "ontology: {ONTOLOGY_ENV}={path} does not exist — nothing is being enforced"
    ));
    self.ontology = OntologySource::Off;
    return;
}
```

Leave the unset/empty branch silent. Update the doc comment, which currently groups
"absent file" with "unset" and would become wrong.

## Testing it

The escalation is observable in `IngestReport`, so this is a unit test, not a CLI test:
set the env var to a path under a temp dir that does not exist, ingest one fact, assert
one escalation whose text names the path. Guard against the vacuous version — the test
must fail if the push is removed, so assert on the message, not merely on
`escalations.len() > 0` which other escalations also satisfy.

Worth a second case asserting the unset path stays silent, so a later "just warn
always" change cannot pass unnoticed.
