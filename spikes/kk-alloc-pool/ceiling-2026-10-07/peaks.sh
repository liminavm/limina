#!/bin/bash
# Per-pool high-water marks from a worker log's [LIMINA-ALLOC-*] lines.
grep -n "LIMINA-ALLOC" "$1" | awk '{p="";k=0;b=0;for(i=1;i<=NF;i++){if($i~/^pool=/)p=$i; if($i~/^peak=/)k=substr($i,6); if($i~/^borrowed=/)b=substr($i,10)} if(p=="")next; split($1,a,":"); if(!(p in first))first[p]=a[1]; if(k+0>max[p])max[p]=k+0; if(b+0>mb[p])mb[p]=b+0; if($0~/teardown/)td[p]=1} END{for(p in max) print p, "first_line="first[p], "peak="max[p], "max_borrowed="mb[p], (td[p]?"torn-down":"live")}'
grep -c "ceiling of" "$1" | sed 's/^/ceiling wait\/refuse lines: /'
