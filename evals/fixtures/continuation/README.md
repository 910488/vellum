# Bounded continuation fixtures

Mechanism evidence is the in-process Port C runner: unfinished deterministic
state, plan→stream-start→commit, stream-failure release, cancel, user steer.
`is_complete` is only a live coding wrapper and is **not** a continuation
hard gate.
