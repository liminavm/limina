# Hiding SME from guests

**Status: SHIPPED 2026-08-13** (libkrun `hvf`). Chrome 151's renderer SIGILLs in the
guest on an M4 host: its SME path issues a non-streaming SVE `cntd`, which UNDEFs on
SME-without-SVE hardware — which is what Apple Silicon from M4 on implements. The bug
is Chrome's, but the user's call was to stop advertising SME to guests
(`limina-chrome-sme-sigill`).

## What was already there, and why it did not fire

libkrun already masked `ID_AA64PFR1_EL1.SME`, with the comment "the guest will break
after enabling the MMU" — but the code sat **inside the `if self.nested_enabled`
branch** of `set_initial_state`. limina does not run nested, so ordinary guests were
told they have SME. The fix is to hoist it out of the branch; the nested case keeps
its behaviour.

The mask constant was also `3 << 24`, two bits of a four-bit field
(`ID_AA64PFR1_EL1.SME` is `[27:24]`). Widened to `0xf << 24`.

## Two premises, both verified rather than assumed

**1. Can we even change what the guest is told?** Hypervisor.framework has
`hv_vcpu_config_get_feature_reg` but **no setter**, so the config route is closed. The
remaining candidate was `hv_vcpu_set_sys_reg` on `HV_SYS_REG_ID_AA64PFR1_EL1` — the
register exists in `hv_sys_reg_t`, but HVF honouring a write to a read-only ID
register is undocumented. `idreg-write-probe.c` answers it: writes are **honoured**,
read-back follows.

Note the probe had to be made non-vacuous first. Its initial version cleared the SME
field on this M1 Max dev Mac, where that field is already zero — so it wrote the value
already present and reported "HONORED" while proving nothing. It now writes a value
that *differs* (sets SME=1, then restores).

**2. Does an ID-register write actually reach the guest?** HVF accepting the write into
its vCPU state is not the same as the guest observing it. This host has no SME, so SME
itself cannot be tested here — so the mechanism was proven with a field this host *does*
have, via a temporary `LIMINA_EXPERIMENT_MASK_AES` hook (since reverted) clearing
`ID_AA64ISAR0_EL1.AES [7:4]`:

    baseline: Features: fp asimd evtstrm aes pmull sha1 sha2 crc32 ...
    masked:   Features: fp asimd evtstrm         sha1 sha2 crc32 ...

`aes` **and** `pmull` both disappeared from the guest's `/proc/cpuinfo` — both are
gated by that one field. Same image, same kernel, same worker binary; only the env var
differed. ID-register masking is observed by the guest, so the SME mask will hold on an
M4.

## What the L2 test can and cannot catch

`crates/limina-test/tests/cpu_features.rs` asserts no `sme` in the guest's feature
list. On this dev Mac (and any M1/M2/M3) the assertion **cannot fail** — the host
advertises no SME, so the guest could not see it either way. The test prints exactly
that when `hw.optional.arm.FEAT_SME` is 0, so a green run here is never mistaken for
coverage; on an M4-class host it is a real guard. It also asserts `asimd` is present,
so a parsing or ssh mishap cannot masquerade as "SME absent".

Verifying the fix on M4 hardware means running it on the dogfood Mac, which is the
user's to do.
