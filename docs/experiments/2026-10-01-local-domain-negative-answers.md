# 2026-10-01 — Why does a miss under `arcbox.local` take 10 s on macOS, and which answer ends it?

- Type: experiment
- Area: `virt/arcbox-net` (`DnsForwarder`), `app/arcbox-daemon` (`DnsService`)
- Outcome: unknown names under the local domain are answered NODATA instead of NXDOMAIN (same commit series as this entry)
- Probes: `tests/bench/dns-local-negative/`
- Host: Apple Silicon, macOS 26.4, `/etc/resolver/arcbox.local` installed by the production daemon (`nameserver 127.0.0.1`, `port 5553`, `timeout 5`), production daemon stopped so a probe could own the port

## Question

While moving the DNS bind ahead of the VM boot, the daemon's socket is now bound but silent during startup. Does that change what a client sees, and is there an answer the daemon could give that fails a lookup fast instead of letting it hang? The answer decides whether the startup window needs a "not ready" response and what the daemon should return for a name that is not registered.

## Hypotheses

1. A port with no listener fails a lookup at once (ICMP port unreachable), so binding early and staying silent makes lookups slower during startup.
2. An immediate SERVFAIL or REFUSED during the startup window ends the lookup faster than silence.
3. The daemon's NXDOMAIN for an unknown name ends a lookup at once.

## Method

`responder.py <mode>` on `127.0.0.1:5553` stands in for the daemon and answers every query one fixed way; `lookup.sh` times one `dscacheutil -q host -a name <fresh-name>.arcbox.local` (getaddrinfo → mDNSResponder → resolver file). One fresh name per lookup. Controls: a `.local` name with no resolver file (`foo.local`, pure mDNS) and a name under a normal unicast domain. `dig @127.0.0.1 -p 5553` checked the responder's answers parse.

## Results

| Listener on 5553 | Lookup | Result |
|---|---|---|
| nothing bound | 10.01 s | no address |
| bound, never answers | 10.01 s | no address |
| SERVFAIL at once | 10.03 s | no address |
| REFUSED at once | 10.01 s | no address |
| NXDOMAIN at once, no SOA | 10.01 s | no address |
| NXDOMAIN at once, SOA in authority | 10.02 s | no address |
| NODATA at once (NOERROR, no records) | 0.01 s | no address |
| A + AAAA records | 0.01 s | addresses |
| A record, NODATA for AAAA | 0.01 s | address |
| A record, no answer for AAAA | 5.01 s | address |
| control: `nothere.foo.local`, no resolver file | 10.01 s | no address |
| control: `nothere.example.com` | 0.01 s | answered by the system resolver |

The responder's log shows the client's schedule: AAAA at 0.45 s, retried at 1.5 s and 3.5 s, then A at 5.45 s, each type abandoned after 5 s. With NODATA for a type, the next type's query follows at once. `dig` against the unbound port timed out rather than reporting a refusal; `dig @127.0.0.1 -p 5553` received the SERVFAIL in 0.01 s. A name answered NODATA, then re-queried once the responder answered positively, resolved in 0.01 s: no negative cache was observed.

## Findings

1. Hypothesis 1 died. Nothing on the port behaves exactly like a silent listener for mDNSResponder and for `dig`: unconnected UDP sockets see no ICMP error on macOS. Binding early changes nothing a client can observe.
2. Hypothesis 2 died. SERVFAIL and REFUSED are ignored for a `.local` question; the lookup waits out the mDNS leg regardless.
3. Hypothesis 3 died, which matters more. NXDOMAIN is ignored too, with or without an SOA. Every miss under `arcbox.local` cost 10 s through getaddrinfo: 5 s per record type, the mDNS timeout, because RFC 6762 gives `.local` to mDNS and mDNS has no "name does not exist" answer. The `foo.local` control shows the same 10 s with no resolver file at all.
4. NODATA (NOERROR with an empty answer section) is the one negative form mDNSResponder accepts from the unicast server for a `.local` name. It ends the question immediately and was not cached. The same form already made AAAA-for-an-IPv4-name fast; extending it to unknown names makes a miss fail in milliseconds.
5. Only a positive answer or NODATA can shorten a lookup. There is no answer the daemon could give during startup that beats silence, so the bound-but-silent window needs no special handling.

## Decisions taken / open

- `DnsForwarder::try_resolve_locally_or_nodata` answers unknown names under the local domain NODATA; the rule and its reason live in `app/AGENTS.md`.
- Open: registering container names with mDNSResponder directly (Bonjour, as OrbStack's `orb.local` does) would let registered names resolve without `/etc/resolver` and without admin. From an unentitled process on this host both `DNSServiceRegisterRecord` and `DNSServiceRegister` return `kDNSServiceErr_PolicyDenied` (-65570) in the callback, while Apple's `/usr/bin/dns-sd -P`, which carries `com.apple.developer.networking.multicast.BYPASS`, registered a `.foo.local` host record that resolved in 0.01 s including the AAAA negative. Whether a Developer ID LaunchAgent is allowed, or prompted for Local Network access, is untested; the unified log was unreadable from the test shell.
