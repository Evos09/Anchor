# py-hygiene: an example verifier plugin

A complete [Anchor verifier plugin](../../docs/verifier/README.md) in one
Python file, standard library only, written from the published contract
(`docs/verifier/`) without reference to Anchor's source.

For each claim it answers:

- `refuted` (`cited_test_not_passed`) if a test the claim cites did not
  pass;
- `refuted` (`debug_leftover`) if a non-test file the claim names gains a
  `breakpoint()`, a `pdb` hook or a `FIXME`;
- `unverifiable` if the input was truncated or none of the cited tests ran;
- `supported` otherwise.

Combined with Anchor's built-in reviewer, it can only add rejections.

## Try it

```sh
cargo build
./target/debug/anchor verifier check examples/verifier-python/verifier.toml
python3 -m unittest examples/verifier-python/test_verifier.py
```

Use it in a project by listing it in `team.toml`:

```toml
[verify]
verifiers = ["path/to/verifier-python/verifier.toml"]
```

Measure it on the seeded-bug corpus:

```sh
./target/debug/anchor eval run --corpus examples/eval/corpus \
    --verifier examples/verifier-python/verifier.toml
```
