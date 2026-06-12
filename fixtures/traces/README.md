# Trace fixtures (gitignored, regenerable)

apitrace captures consumed by the `venus_replay` test
(`crates/limina-test/tests/venus_replay.rs`). Like the `.raw` disk images, the
`.trace` files are kept locally and never committed — regenerate with the seated
desktop up (`spikes/venus-draw-probe/boot-seated-kk.sh` + `--net`):

```sh
spikes/trace-replay/capture-replay.sh build   # captures + pulls glmark2-build.trace here
```

The capture runs IN the guest on real zink→venus (the trace then replays on any
backend); see `spikes/trace-replay/RESULTS.md` for the pipeline details and the
env trap to avoid.
