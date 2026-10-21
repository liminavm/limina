# netd-dns-probe — answering the guest's DNS through the host resolver

limina-netd answers DNS at the gateway through the host's own resolver, so a VPN's split DNS and
scoped resolvers apply to the guest as to any app (`docs/design/multi-vm-networking.md` §3.2).
`DNSServiceQueryRecord` (`<dns_sd.h>`, mDNSResponder) is the API that does that for every record
type; it returns records one per callback, and netd builds the DNS response from them. This probe
asks it for a CNAME chain, an empty answer, a missing name, MX, SRV and TXT, with no flags and with
`kDNSServiceFlagsReturnIntermediates | kDNSServiceFlagsTimeout`, logging every callback.

Build and run: `xcrun clang -o build/probe probe.c && build/probe [seconds-per-query]`.

## Measured 2026-10-19, macOS 27.0.1

| Query | No flags | `ReturnIntermediates \| Timeout` |
|---|---|---|
| `www.github.com` A (CNAME → `github.com`) | one A record, owner `github.com.` | CNAME (owner `www.github.com.`) then A (owner `github.com.`), one batch |
| `github.com` AAAA (no such record) | no callback at all | one callback, `kDNSServiceErr_NoSuchRecord`, no rdata |
| `no-such-name.limina.invalid` A (no such name) | no callback at all | the same as above: `NoSuchRecord`, no rdata |
| `gmail.com` MX, `google.com` TXT | every record, `MoreComing` on all but the last | the same |
| `_xmpp-server._tcp.gmail.com` SRV (none) | no callback | `NoSuchRecord` |

- **Without the flags the API cannot serve a stub resolver:** a CNAME chain arrives as the final
  record under the target's name, and a negative answer never arrives, so every lookup of a missing
  name would wait out netd's own timeout.
- **With them, a query's answer is one batch:** callbacks until one without
  `kDNSServiceFlagsMoreComing` (`0x1`), each record under its own owner name, CNAMEs included, in
  chain order. With one `DNSServiceRef` per query, its socket and its `MoreComing` are that query's
  alone.
- **A missing name and an empty answer look the same** (`kDNSServiceErr_NoSuchRecord`, -65554;
  `NoSuchName` never appeared), so netd answers both as NOERROR with no records. Stub resolvers
  treat that as "no data", and the search-list walk continues on it as on NXDOMAIN.
- Answers from mDNSResponder's cache carry `kDNSServiceFlagAnsweredFromCache` (`0x40000000`) and
  are otherwise the same.
- Answers arrive in 2–3 ms from cache and 15–170 ms otherwise.
