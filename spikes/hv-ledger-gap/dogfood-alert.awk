# Alert filter for watch-worker.sh output: print only lines that mean something,
# plus a periodic heartbeat so a silent watch can be told apart from a calm guest.
#
#   watch-worker.sh | awk -f dogfood-alert.awk
#
# Thresholds are deliberately loose -- this is a "wake me" filter, not a metric. Each one
# names a failure this stack has actually had; see the balloon/ledger memories for the
# incident behind it.

/worker=none/ { next }
{
  bal=0; pf=0; psi=0; flt=0; cdr=0
  if (match($0, /balloon=[0-9]+G/)) { bal = substr($0, RSTART+8, RLENGTH-9) + 0 }
  if (match($0, /pf=[0-9]+M/))      { pf  = substr($0, RSTART+3, RLENGTH-4) + 0 }
  if (match($0, /psi=[0-9]+/))      { psi = substr($0, RSTART+4, RLENGTH-4) + 0 }
  if (match($0, /faults=[0-9]+/))   { flt = substr($0, RSTART+7, RLENGTH-7) + 0 }
  if (match($0, /cd_run=[0-9]+/))   { cdr = substr($0, RSTART+7, RLENGTH-7) + 0 }

  why=""
  if (bal < 8)    why = why "balloon-released "      # balloon gave everything back
  if (pf > 25000) why = why "footprint-ratchet "     # host phys_footprint climbing
  if (psi > 500)  why = why "guest-memory-pain "     # >5% memory PSI in the guest
  if (flt > 0)    why = why "sweep-faults "          # ledger settle sweep faulted
  if (seen && cdr > prev_cdr) why = why "cooldown-run-grew(" prev_cdr "->" cdr ") "
  prev_cdr = cdr; seen = 1
  if (/host=warn|host=critical/) why = why "host-pressure "

  if (why != "")            { print "ALERT[" why "] " $0; fflush() }
  else if (++n % 18 == 1)   { print "(3-hourly heartbeat, all nominal) " $0; fflush() }
}
