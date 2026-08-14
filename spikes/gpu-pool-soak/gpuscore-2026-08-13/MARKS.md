# gpuscore run — phase marks

Boundaries as reported by the user driving the run, so segments are cut on stated times rather
than inferred from the counters. GPU work does not move the blob count the way a window open
does, so the benchmark start/finish marks in particular cannot be recovered from the data.

| local | phase |
|---|---|
| 21:14:16 | sampler start; baseline, Firefox not running |
| ~21:15:30 | Firefox launched (regions 3685 -> 5824, blobs 50 -> 67, clients 6 -> 9) |
| ~21:17:00 | web.gpuscore.com loaded, benchmark NOT started (settles to ~5464 regions) |
| | benchmark start — pending |
| | benchmark finish — pending |
| | tab closed — pending |
| | Firefox quit — pending |

## Baseline, for differencing

    gfx 532 MB / 3661 regions   guest 50 blobs / 281 MB, 6 clients
    footprint 12703 MB   compressed 9481 MB   balloon 16757 MB   guest free 496 MB

## Reading so far

Firefox existing costs ~+2140 regions and +17 guest blobs. Loading the benchmark page costs
nothing beyond that — 5464 regions loaded vs 5824 on the launch transient, i.e. it settles
*below* the peak of simply starting the browser. So page load and GPU work are cleanly separable
in this run, which is what the two separate holds were for.
