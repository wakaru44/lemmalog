# 2. Retraction records why, in three kinds

**Status:** accepted · 2026-09-20

A single `retract` collapses two events with opposite consequences: *we misread
the code* (the fact was never true, so everything derived from it is poisoned)
and *the code changed* (the fact was true until commit X, so its dependents were
correct for that period and merely need rechecking). We therefore record a
reason on every retraction — `wrong` closes assertion time and cascades;
`world_changed` closes valid time at a commit and marks dependents **suspect**;
`superseded` chains to the successor and re-derives.

**Consequence:** a third fact state exists — suspect, meaning *unverified against
current reality* — expressed as a Datalog rule over supports closed in valid
time, never as stored state. The set of suspect facts is a re-verification
queue, which on a months-long archaeology project is worth more than any
individual fact in the store. Agents may clear a suspect fact only at confidence
1.0 with a freshly verified `path::symbol` anchor at current HEAD; anything
inferred goes to a human.
