# Field data, 2026-08-10 (rescued from /tmp before host reboots)

- `limina-ledger-trace.csv` — the dogfood Mac sampler's first rows (fresh worker
  pid 68019, ~1 h uptime). Early signal: internal_compressed 2.2 G vs
  vmmap-attributable swapped 0.86 G — the gap was already opening. First row's
  swap column reads "free" (pre-fix sampler); later rows are MB used.
- `limina-ledger-vmmap.log` — hourly vmmap attributable companion.
- `ic_sweep.txt` — dogfood Mac, GROSS internal_compressed per task, worker
  ALIVE at 43.8 G (the round-3 pre-restart sweep).
- `ic_sweep2.txt` — dogfood Mac, NET compressed per task, worker DEAD:
  sums 22.7 G vs 57.7 G stored = the ~35 G orphan pool.
- `devmac_sweep.txt` — dev Mac NET sweep after the churn-probe legs: 22.0 G
  owned vs 44.9 G stored, same graveyard silhouette.

Both machines were rebooted after this capture (reclaiming the graveyards);
post-reboot baselines start clean. Redeploy: build `ledger-dump`, scp it plus
`ledger-sampler.sh` to the target's /tmp, start with
`nohup bash /tmp/limina-ledger-sampler.sh &` (script self-guards via pidfile).
