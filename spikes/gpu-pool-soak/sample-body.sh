# Sampler body for soak.sh — sourced values (OUT/WORKER/SSH/HOURS) are prepended by the
# generator, so this file is a plain script with no escaping games. One CSV row every 2 min.
#
# `footprint(1)` is the instrument rather than vmmap: it attributes per category AND counts
# regions, and region count is the discriminator this soak exists for (bytes alone cannot tell
# "the guest holds many live scanouts" from "host-side retention is not keeping up").

# Every size cell carries its own unit and the units CHANGE as the pool grows (MB -> GB),
# which is exactly the movement being measured — normalize to MB so a column never silently
# changes meaning mid-run. Header line is "limina-vmm [pid]: 64-bit  Footprint: 1453 MB (...)"
# so value/unit are $5/$6 there and $1/$2 on the category rows.
MB='{v=$(f); u=$(f+1); if(u=="GB") v*=1024; else if(u=="KB") v/=1024; else if(u=="B") v/=1048576; printf "%.0f", v; exit}'

echo "ts,elapsed_min,footprint_mb,gfx_mb,gfx_regions,iosurface_mb,owned_unmapped_mb,owned_regions,guest_flips,guest_created" > "$OUT/soak.csv"
START=$(date +%s)
END=$(( START + HOURS * 3600 ))
while [ "$(date +%s)" -lt "$END" ]; do
    kill -0 "$WORKER" 2>/dev/null || { echo "worker $WORKER gone $(date +%H:%M:%S)" >> "$OUT/soak.err"; break; }
    F=$(/usr/bin/footprint "$WORKER" 2>/dev/null)
    fp=$(echo "$F"  | awk -v f=5 "/Footprint:/$MB")
    gfx=$(echo "$F" | awk -v f=1 "/IOAccelerator \(graphics\)/$MB")
    gfxr=$(echo "$F"| awk '/IOAccelerator \(graphics\)/{for(i=1;i<=NF;i++) if($i=="IOAccelerator"){print $(i-1); exit}}')
    ios=$(echo "$F" | awk -v f=1 "/IOSurface/$MB")
    own=$(echo "$F" | awk -v f=1 "/Owned physical footprint \(unmapped\) \(graphics\)/$MB")
    ownr=$(echo "$F"| awk '/Owned physical footprint \(unmapped\) \(graphics\)/{for(i=1;i<=NF;i++) if($i=="Owned"){print $(i-1); exit}}')
    tail=$($SSH "tail -1 /tmp/kmschurn.log" 2>/dev/null | tr -d '\r')
    flips=$(echo "$tail"   | grep -oE '(PROGRESS|flips=) *[0-9]+' | grep -oE '[0-9]+' | tail -1)
    created=$(echo "$tail" | grep -oE 'created=[0-9]+' | cut -d= -f2)
    now=$(date +%s)
    echo "$(date +%H:%M:%S),$(( (now - START) / 60 )),${fp:-},${gfx:-},${gfxr:-},${ios:-},${own:-},${ownr:-},${flips:-},${created:-}" >> "$OUT/soak.csv"
    sleep 120
done
echo "sampler done $(date +%H:%M:%S)" >> "$OUT/soak.csv"
