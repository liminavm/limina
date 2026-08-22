#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
"""Offline slot health for a LiminaOS disk image — the check the guest can never perform.

**Why this exists.** dm-verity validates lazily, per block, on read: activation checks only the
hash tree's root, and a data block is hashed when something actually reads it. A `/usr` with
corruption 50 MiB in boots cleanly and gets blessed, because nothing touches that block during
boot. So there is no moment at which a running guest validates its whole image — and the slot it
would fall *back* to is worse, because nothing will ever read a dormant slot at all. A rotting B
slot is invisible by construction until a rollback needs it, with the boot counter already spent.

The host can answer that, and only with the guest stopped: the image cannot change mid-check, and
**the root hash is recoverable from the GPT alone** — systemd stamps a slot's two partition UUIDs
with the two halves of its verity root hash — so this needs nothing but the disk file. No ESP
parse, no guest cooperation, no state stored on the host.

Usage:
    verify-slot.py <disk.raw>            # report every slot
    verify-slot.py <disk.raw> --verify   # also walk the verity tree (reads the whole slot)
"""

import argparse
import hashlib
import struct
import sys
import uuid

SECTOR = 512
# Discoverable Partitions Spec type GUIDs we care about (aarch64).
TYPE_USR_ARM64 = "b0e01050-ee5f-4390-949a-9101b17104e9"
TYPE_USR_VERITY_ARM64 = "6e11a4e7-fbca-4ded-b9e9-e1a512bb664e"


def read_gpt(path):
    """Return [(name, type_guid, unique_guid_hex, first_lba, last_lba)] for a raw image."""
    with open(path, "rb") as f:
        f.seek(SECTOR)
        hdr = f.read(92)
        if hdr[:8] != b"EFI PART":
            sys.exit(f"{path}: no GPT header (not a LiminaOS disk image?)")
        part_lba = struct.unpack_from("<Q", hdr, 72)[0]
        nparts = struct.unpack_from("<I", hdr, 80)[0]
        psize = struct.unpack_from("<I", hdr, 84)[0]
        f.seek(part_lba * SECTOR)
        out = []
        for _ in range(nparts):
            e = f.read(psize)
            if e[:16] == b"\x00" * 16:
                continue
            out.append(
                (
                    e[56:128].decode("utf-16-le").rstrip("\x00"),
                    str(uuid.UUID(bytes_le=e[:16])),
                    uuid.UUID(bytes_le=e[16:32]).hex,
                    struct.unpack_from("<Q", e, 32)[0],
                    struct.unpack_from("<Q", e, 40)[0],
                )
            )
        return out


def verity_superblock(path, first_lba):
    """Parse the on-disk verity superblock so parameters are READ, not assumed.

    Defaults drift across veritysetup versions; reading them per-slot means a changed default
    shows up as a different value rather than as a mysterious hash mismatch.
    """
    with open(path, "rb") as f:
        f.seek(first_lba * SECTOR)
        sb = f.read(512)
    if sb[:8] != b"verity\x00\x00":
        return None
    version, hash_type = struct.unpack_from("<II", sb, 8)
    sb_uuid = uuid.UUID(bytes=sb[16:32])
    algo = sb[32:64].rstrip(b"\x00").decode(errors="replace")
    data_bs, hash_bs = struct.unpack_from("<II", sb, 64)
    data_blocks = struct.unpack_from("<Q", sb, 72)[0]
    salt_size = struct.unpack_from("<H", sb, 80)[0]
    salt = sb[88 : 88 + salt_size]
    return {
        "version": version,
        "hash_type": hash_type,
        "uuid": str(sb_uuid),
        "algorithm": algo,
        "data_block_size": data_bs,
        "hash_block_size": hash_bs,
        "data_blocks": data_blocks,
        "salt": salt.hex(),
    }


TYPE_ESP = "c12a7328-f81f-11d2-ba4b-00a0c93ec93b"


def read_esp_ukis(path, first_lba):
    """List /EFI/Linux/*.efi from the ESP, in pure Python — no mount, no mtools, no privileges.

    Keeping this dependency-free is what makes the whole check a function of the disk file: a
    stopped VM's health should not require tooling that may or may not be on the host.
    """
    with open(path, "rb") as f:
        base = first_lba * SECTOR
        f.seek(base)
        bpb = f.read(512)
        bps = struct.unpack_from("<H", bpb, 11)[0]
        spc = bpb[13]
        reserved = struct.unpack_from("<H", bpb, 14)[0]
        nfats = bpb[16]
        fatsz = struct.unpack_from("<I", bpb, 36)[0]
        root_clus = struct.unpack_from("<I", bpb, 44)[0]
        if not bps or not spc or not fatsz:
            return []  # not FAT32
        data_start = reserved + nfats * fatsz

        def chain(start):
            """Follow the FAT cluster chain; bounded so a corrupt FAT cannot spin forever."""
            out, c, guard = [], start, 0
            while 2 <= c < 0x0FFFFFF8 and guard < 100000:
                out.append(c)
                f.seek(base + reserved * bps + c * 4)
                c = struct.unpack("<I", f.read(4))[0] & 0x0FFFFFFF
                guard += 1
            return out

        def read_dir(start):
            data = b""
            for c in chain(start):
                f.seek(base + (data_start + (c - 2) * spc) * bps)
                data += f.read(spc * bps)
            entries, lfn = [], []
            for i in range(0, len(data), 32):
                e = data[i : i + 32]
                if len(e) < 32 or e[0] == 0x00:
                    break
                if e[0] == 0xE5:
                    lfn = []
                    continue
                if e[11] == 0x0F:  # long-file-name fragment
                    part = (e[1:11] + e[14:26] + e[28:32]).decode("utf-16-le", "ignore")
                    lfn.insert(0, part.split("￿")[0].rstrip("\x00"))
                    continue
                name = "".join(lfn) if lfn else e[:11].decode("ascii", "ignore").strip()
                lfn = []
                clus = (struct.unpack_from("<H", e, 20)[0] << 16) | struct.unpack_from("<H", e, 26)[0]
                entries.append((name, e[11], clus, struct.unpack_from("<I", e, 28)[0]))
            return entries

        def find(entries, want):
            for name, attr, clus, _sz in entries:
                if name.rstrip("\x00").lower() == want.lower() and attr & 0x10:
                    return clus
            return None

        efi = find(read_dir(root_clus), "EFI")
        if efi is None:
            return []
        linux = find(read_dir(efi), "Linux")
        if linux is None:
            return []

        ukis = []
        for name, attr, clus, size in read_dir(linux):
            if attr & 0x10 or not name.lower().endswith(".efi"):
                continue
            blob = b""
            for c in chain(clus):
                f.seek(base + (data_start + (c - 2) * spc) * bps)
                blob += f.read(spc * bps)
            ukis.append((name, blob[:size]))
        return ukis


def pe_sections(blob):
    """Extract PE section payloads — each UKI names the slot it boots, in its own .cmdline."""
    try:
        pe = struct.unpack_from("<I", blob, 0x3C)[0]
        if blob[pe : pe + 4] != b"PE\x00\x00":
            return {}
        nsec = struct.unpack_from("<H", blob, pe + 6)[0]
        optsz = struct.unpack_from("<H", blob, pe + 20)[0]
        sect = pe + 24 + optsz
        out = {}
        for i in range(nsec):
            off = sect + i * 40
            nm = blob[off : off + 8].rstrip(b"\x00").decode(errors="replace")
            _v, _a, raw, ptr = struct.unpack_from("<IIII", blob, off + 8)
            out[nm] = blob[ptr : ptr + raw]
        return out
    except Exception:
        return {}


def boot_order(ukis):
    """Reproduce sd-boot's ordering: unexhausted entries newest-first, then exhausted ones.

    The fallback is NOT simply "the other slot" — an entry's position changes as tries are
    spent, with no byte of either slot changing. Hence an ordered list rather than one static
    "fallback" field.
    """
    import re

    rows = []
    for name, blob in ukis:
        m = re.match(r"^(?P<id>[^_]+)_(?P<ver>[^+]+?)(?:\+(?P<left>\d+)(?:-(?P<done>\d+))?)?\.efi$",
                     name, re.I)
        secs = pe_sections(blob)
        osrel = secs.get(".osrel", b"").decode(errors="replace")
        mv = re.search(r"^IMAGE_VERSION=(.*)$", osrel, re.M)
        uh = re.search(r"usrhash=([0-9a-f]{64})",
                       secs.get(".cmdline", b"").rstrip(b"\x00").decode(errors="replace"))
        left = m.group("left") if m else None
        rows.append({
            "file": name,
            "version": (mv.group(1).strip() if mv else (m.group("ver") if m else "?")),
            "tries_left": None if left is None else int(left),
            "exhausted": left is not None and int(left) == 0,
            "usrhash": uh.group(1) if uh else None,
        })

    def vkey(v):
        return [int(x) if x.isdigit() else x for x in re.split(r"(\d+)", v) if x != ""]

    return sorted(rows, key=lambda r: (r["exhausted"], [-c if isinstance(c, int) else 0
                                                        for c in vkey(r["version"])]))


def hash_block(algo, salt, block):
    h = hashlib.new(algo)
    h.update(salt)
    h.update(block)
    return h.digest()


def verify_slot(path, data_part, hash_part, sb, expect_root):
    """Walk the verity tree bottom-up and compare the computed root to the expected one.

    This is the whole-image check that never happens anywhere else: verity itself only ever
    hashes the blocks something reads.
    """
    algo = sb["algorithm"]
    salt = bytes.fromhex(sb["salt"])
    bs = sb["data_block_size"]
    hbs = sb["hash_block_size"]
    ndata = sb["data_blocks"]
    digest_size = hashlib.new(algo).digest_size
    per_block = hbs // digest_size

    with open(path, "rb") as f:
        # Level 0: hash every data block of the usr partition.
        f.seek(data_part[3] * SECTOR)
        digests = []
        for _ in range(ndata):
            blk = f.read(bs)
            if len(blk) < bs:
                blk = blk.ljust(bs, b"\x00")
            digests.append(hash_block(algo, salt, blk))

    # Fold upward until a single digest remains — that is the root.
    while len(digests) > 1:
        nxt = []
        for i in range(0, len(digests), per_block):
            group = b"".join(digests[i : i + per_block]).ljust(hbs, b"\x00")
            nxt.append(hash_block(algo, salt, group))
        digests = nxt
    return digests[0].hex()


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("disk")
    ap.add_argument("--verify", action="store_true",
                    help="walk the verity tree and compare to the root hash in the GPT "
                         "(reads the whole slot; the check nothing else ever performs)")
    args = ap.parse_args()

    parts = read_gpt(args.disk)
    usr = [p for p in parts if p[1].lower() == TYPE_USR_ARM64]
    ver = [p for p in parts if p[1].lower() == TYPE_USR_VERITY_ARM64]
    free = [p for p in parts if p[0] == "_empty"]

    print(f"{args.disk}")
    print(f"  usr slots: {len(usr)}   verity slots: {len(ver)}   free (_empty): {len(free)}")
    # _empty is a lifecycle value, not a fault: 2 on a fresh image, 0 after the first update.
    print("  (a free-slot count of 0, 1 or 2 is all healthy — it is not a damage signal)\n")

    # Which slot boots, and which one catches you if it doesn't.
    esp = next((p for p in parts if p[1].lower() == TYPE_ESP), None)
    order, slot_role = [], {}
    if esp:
        order = boot_order(read_esp_ukis(args.disk, esp[3]))
    if order:
        print("  boot order (sd-boot: unexhausted newest-first, exhausted last):")
        for i, r in enumerate(order):
            tries = "blessed" if r["tries_left"] is None else (
                "EXHAUSTED" if r["exhausted"] else f"{r['tries_left']} tries left")
            role = "boots next" if i == 0 else ("FALLBACK" if i == 1 else f"#{i+1}")
            print(f"      {role:<11} {r['version']:<6} {tries:<15} {r['file']}")
            if r["usrhash"]:
                slot_role[r["usrhash"][:32]] = role
        if len(order) == 1:
            print("      !! ONLY ONE ENTRY — nothing to fall back to; boot counting cannot")
            print("         protect a single-slot machine (an exhausted entry is retried, not refused)")
        print()

    rc = 0
    for u in usr:
        if u[0] == "_empty":
            print(f"  [{u[0]}] free slot, nothing installed")
            continue
        # A slot's root hash is its two partition UUIDs concatenated: usr first, verity second.
        mate = next((v for v in ver if v[0].endswith(u[0].split("_usr_")[-1])), None)
        if mate is None:
            print(f"  [{u[0]}] NO MATCHING VERITY PARTITION — slot cannot be verified or booted")
            rc = 1
            continue
        root = u[2] + mate[2]
        role = slot_role.get(u[2])
        print(f"  [{u[0]}]" + (f"   <-- {role}" if role else ""))
        print(f"      root hash (from GPT) {root}")
        sb = verity_superblock(args.disk, mate[3])
        if sb is None:
            print("      NO VERITY SUPERBLOCK in the hash partition — slot is not bootable")
            rc = 1
            continue
        print(f"      algorithm {sb['algorithm']}  data_bs {sb['data_block_size']}  "
              f"blocks {sb['data_blocks']}  salt {sb['salt'][:16]}…")
        if args.verify:
            got = verify_slot(args.disk, u, mate, sb, root)
            ok = got == root
            print(f"      computed             {got}")
            print(f"      >>> {'VERIFIED' if ok else 'CORRUPT — contents do not match the root hash'}")
            if not ok:
                rc = 1
        else:
            print("      (pass --verify to walk the tree; parameters above are READ, not assumed)")
    return rc


if __name__ == "__main__":
    sys.exit(main())
