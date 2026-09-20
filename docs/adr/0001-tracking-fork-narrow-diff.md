# 1. Track upstream lemmalog with a deliberately narrow diff

**Status:** accepted · 2026-09-20

Upstream ([JordyZomer/lemmalog](https://github.com/JordyZomer/lemmalog), MIT)
carries the expensive parts we depend on and cannot maintain alone — the
seminaive evaluator (`src/eval.rs`, ~1,970 lines) and the magic-set demand
transformation (`src/magic.rs`). We therefore keep this fork *rebaseable*:
changes stay confined to the storage seam (`AgentMemory::save` / `load`) and to
additive API surface, and we do not touch `eval.rs` or `magic.rs`. A change that
would require evaluator surgery is the signal that we have chosen wrong, not a
licence to diverge.

**Consequence:** we accept occasionally clumsier implementations — the
supersession-provenance fix (ADR 2) does a linear scan rather than add a
`by_key` accessor to `eval.rs` — in exchange for continuing to inherit upstream's
evaluator work for free. We do not upstream our changes: this is an experiment
and upstream is a consolidated project we have no wish to pollute.
