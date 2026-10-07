# clevis's tpm2 pin, unbound and bound to PCR 7.
set -ex
echo -n "clevis-plain" | clevis encrypt tpm2 '{}' > /tmp/c1.jwe
clevis decrypt < /tmp/c1.jwe; echo
echo -n "clevis-pcr7" | clevis encrypt tpm2 '{"pcr_ids":"7"}' > /tmp/c2.jwe
clevis decrypt < /tmp/c2.jwe; echo
