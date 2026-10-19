# netd-icmp-probe — unprivileged ICMP echo for limina-netd

limina-netd answers the guest's pings by sending echo requests of its own from an unprivileged
ICMP socket (`socket(AF_INET, SOCK_DGRAM, IPPROTO_ICMP)`, no root). This probe opens two such
sockets, sends an echo request from each with its own identifier to the same destination, with the
ICMP checksum filled in and left zero, and reads what each socket receives.

Build and run: `xcrun clang -o build/probe probe.c && build/probe [remote-ip]` (default 1.1.1.1).

## Measured 2026-10-19, macOS 27.0.1

| | 127.0.0.1 | 1.1.1.1 |
|---|---|---|
| Checksum filled | both replies reach **both** sockets | both replies reach **both** sockets |
| Checksum left zero | no reply | no reply |

- **Every unprivileged ICMP socket receives every echo reply**, whichever socket sent the request:
  replies are not demultiplexed by identifier. netd has to match replies to requests itself, and
  sees the replies to every other process's pings (and to every other VM's netd) too.
- **The identifier is the sender's**: the kernel neither rewrites nor checks it, so two senders
  using the same identifier cannot be told apart by it. netd gives each guest flow an identifier
  of its own and maps the reply back, matching on the replying address as well.
- **The kernel does not fill in the checksum**: a request sent with a zero checksum gets no reply.
  netd computes it, as it must anyway after rewriting the identifier.
- **A received reply starts with its IP header** (20 bytes here), then the ICMP message.
