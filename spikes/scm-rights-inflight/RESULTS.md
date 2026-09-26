# A socket passed with SCM_RIGHTS dies in the queue once its sender lets go

**Question.** `limina_launch::connect` opens a connection to a worker listener by making a
stream pair and sending one end down a link socketpair with `SCM_RIGHTS`. Can the sender close
its own copy straight after `sendmsg`, as it would after a `connect()`?

**Answer.** No. Once the sender closes its copy, the only reference left is the one in flight,
and if the receiver does not take it within moments it arrives already at EOF: the byte written
on its peer is gone, although the peer is still open. Holding the sender's copy until the receiver
has it keeps it intact. Datagram and stream links behave the same.

## Vehicle

`inflight.c <close|keep> <delay-ms> [stream|dgram]`: send `far` down the link; close the sender's
copy of `far` or keep it; write one byte on `near` (which stays open); wait; receive `far` and read.
`run.sh` builds it and runs the arms below.

## Results

Measured 2026-09-26 on the dev Mac (M1 Max, macOS 26.6.2):

| sender's copy | delay before the receive | link | read from `far` |
|---|---|---|---|
| closed | 0 ms | stream | `1 [a]` |
| closed | 300 ms | stream | `0 []`, EOF |
| kept | 300 ms | stream | `1 [a]` |
| closed | 300 ms | dgram | `0 []`, EOF |
| kept | 300 ms | dgram | `1 [a]` |

The 0 ms arm is why this looks fine in a quick test: a receiver fast enough wins the race. In
the unit tests it failed within one run of two connections.

## What this means for limina

`Connector::connect` keeps its copy until the worker acknowledges the accept with one byte on the
link, and only then drops it; a connect that is not acknowledged within `ACCEPT_TIMEOUT` fails, as
a connect to an unbound path did. Any other fd-passing of sockets should do the same, or keep
the sender's copy until the receiver confirms.
