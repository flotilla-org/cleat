# Pinned Ghostty namespace probe

`namespace-probe.c` is the original C reproducer compiled against unmodified
`rjwittams/ghostty@c361de9691f006f65c400be73896d1e48a8ec56c`.
`namespace-probe.txt` is its captured output. The two command streams expose
colliding internal/external numeric placement IDs. Swapping the namespace of
the two declarations changes the source geometry chosen for a zero-ID
placeholder, despite identical exported declaration multisets.

Run after preparing the pinned library:

```sh
cc -I .tools/ghostty-install/include \
  crates/cleat/tests/fixtures/kitty-virtual-declarations/namespace-probe.c \
  -L .tools/ghostty-install/lib -lghostty-vt -o /tmp/cleat-namespace-probe
LD_LIBRARY_PATH="$PWD/.tools/ghostty-install/lib" /tmp/cleat-namespace-probe
```

`vt::kitty_declarations::tests::namespace_collision_probe_preserves_original_ids_and_order`
checks the exported cleat behavior of the same collision pattern.
