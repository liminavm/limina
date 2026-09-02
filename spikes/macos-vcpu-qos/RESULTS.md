# How a "little" vCPU is actually made little on Apple Silicon

Two questions the little-vCPU design rests on, both answered here by measurement rather than by
what the APIs appear to offer. Build either with `clang -O1 -o <name> <name>.c` and run it.

## `aff.c` — can a thread be pinned to the efficiency cores?

**No.** `THREAD_AFFINITY_POLICY` returns `KERN_NOT_SUPPORTED` (46) on Apple Silicon, for both set
and get:

```
THREAD_AFFINITY_POLICY  -> kr=46 (KERN_NOT_SUPPORTED)
  read back             -> kr=46 tag=0 default=0
```

It was always an Intel cache-affinity *hint* rather than a pin, and arm64 rejects it outright. So
there is no way to place a vCPU thread on a particular core or cluster, and **QoS is the only
channel that exists**. A "little" vCPU is therefore defined by a scheduling class, not by a core
type — which is why its advertised `capacity-dmips-mhz` had to be measured (`limina-vcpu-eas-packing`)
instead of derived from the hardware's P/E ratio.

## `override.c` — can the host change a live vCPU thread's QoS from outside?

**Yes**, with `pthread_override_qos_class_start_np`, and it is cleanly reversible:

```
BACKGROUND (self-set):            1339 units/1.5s
+ external override USER_INIT:    4248 units/1.5s     3.17x
after override ended:             1369 units/1.5s     restored to 1.02x
```

This is what lets a power-profile change promote or demote a running vCPU without the vCPU thread
cooperating: `pthread_set_qos_class_self_np` is self-only, which would otherwise force a polled
flag in the vCPU run loop. `std::os::unix::thread::JoinHandleExt::as_pthread_t()` supplies the
handle from the existing `VcpuHandle`.

Note the trap this probe was written to avoid: `pthread_get_qos_class_np` reports the thread's
**requested** class, so it keeps reading `BACKGROUND` while an override is active and in force.
A read-back check would have concluded the override does nothing. Only throughput answers.

## What is still not separated

The slowdown a little vCPU shows (3.75x in-guest, 3.17x here) is the **combined** effect of
efficiency-core placement and the throttling `QOS_CLASS_BACKGROUND` also applies. That it
overshoots the ~2.3x P/E hardware ratio is what implies throttling is part of it — inference, not
measurement. Separating the two needs `powermetrics --samplers cpu_power`, which needs root.
