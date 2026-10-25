# Cross-check goldens

Every engine in this workspace was ported from a Python reference implementation
and verified **byte-identical** to it. Those assertions live in each crate's
`tests/xcheck_*.rs` as frozen literals (and, for the larger corpora, in
`tests/goldens/*.rs`), so the whole test suite runs with **no Python present** —
which is the point: this tree is 100% Rust.

## Regenerating a golden

The generators (`tests/regen_*.py`) and the reference implementation
(`legacy-python/`) were removed once the migration completed, in the commit
"removing python" of the astrx-suite repository
(https://github.com/Vxmdzhb/astrx-suite). Both are preserved in its history,
in that commit's parent:

    git clone https://github.com/Vxmdzhb/astrx-suite && cd astrx-suite
    ref=$(git log --format=%H -1 --grep='^removing python$')^
    git show "$ref" --stat
    git checkout "$ref" -- legacy-python crates/<crate>/tests/regen_goldens.py

Then follow the header comment in the generator (each documents its
`PYTHONPATH=legacy-python/<pkg> python3 …` invocation), regenerate, and drop the
restored files again.

## Why the goldens are frozen rather than regenerated in CI

The reference is retired; it no longer receives fixes. A golden that can be
silently regenerated is a golden that can silently drift, so from here the
literals ARE the specification — a change to rendered output must be a
deliberate edit to the expected value, visible in review, not a side effect of
re-running a script against a moving target.
