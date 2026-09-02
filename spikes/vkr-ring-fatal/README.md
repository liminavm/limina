# The venus ring going FATAL with nothing logged before it

`dogfood-2026-09-01-2205.log` is the 1200-line window around a live occurrence: 800 lines
before the fatal, 400 after.

## What it shows

```
22:05:32.482  ERROR virgl: vkr: cs decoder: ring FATAL set at vn_cs_decoder_set_fatal:69 (context 3 [synoik])
22:05:33.554  three iosurface scanouts recreated
22:05:36.117  vkr: limina GPU budget: ctx 3 [synoik] destroyed — lifetime 944 charges totalling 2.8 GiB
```

The 800 lines before the fatal are **ordinary**: KK counters climbing steadily, scanout
recreations, allocator-pool reports with every detector at zero. No KK error, no rejection, no
warning, nothing that names a cause. The ring simply goes fatal.

## Why it presents as a hang rather than a crash

A FATAL ring stops draining, so the guest's `vn_ring_wait_seqno` never advances and anything
parked in it waits forever — **holding the last frame it presented, which is a correct picture**.
Measured at the time of this capture: the guest was `up 17 min, load 0.25`, ssh responsive, and
the compositor process still alive at 2.3% CPU with 17:45 elapsed. Nothing was wedged except the
graphics; the host worker kept rendering and logging at 49% CPU throughout.

So "the VM hung" is, in this failure, a claim about one venus context. Check the guest over ssh
before treating it as a VM-level fault — the session is usually recoverable and the user's work
is intact.

## What is missing, and what would fix that

The cause. `vn_cs_decoder_set_fatal` is reached from several places and the log names only the
line number, not the command being decoded. The traces that would say more
(`LIMINA_VKR_MTLTEX_TRACE` and friends) are off on a dogfood build, and arming them after the
fact is useless for a fault that leaves no precursor.

**A breadcrumb at the fatal site would close this**: record the last command opcode and context
decoded, and print it *with* the fatal. One line, only when it fires, no steady-state cost. That
turns the next occurrence from "it went fatal" into "it went fatal on this command", which is the
difference between a data point and a lead.

Related: the same ghost ring-FATAL class was seen from a KK modifier rejection poisoning a ring,
where the rejection at least logged. This one did not, which is why it is worth its own capture.
