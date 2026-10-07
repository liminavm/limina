#!/usr/bin/env python3
"""Decode swtpm's level-20 log into TPM 2.0 command/response pairs, and summarise a corpus.

    decode.py <slice.swtpm.log>              # one JSON object per event, on stdout
    decode.py --summary corpus/*.jsonl       # the command set, per consumer, as Markdown

swtpm logs each command as ` SWTPM_IO_Read: length N` followed by hex lines, and each response
as ` SWTPM_IO_Write: length N`. The control channel's INIT (a platform reset) and SET_LOCALITY
are kept as `{"ctrl": ...}` events in order. libtpms's own debug lines interleave and are skipped. Only the
fields the design needs are decoded: the command code, the response code, session attributes,
and the algorithm choices of the commands that create keys or sessions.
"""

import json
import re
import sys
from collections import Counter, defaultdict

CC = {
    0x11F: "NV_UndefineSpaceSpecial", 0x120: "EvictControl", 0x121: "HierarchyControl",
    0x122: "NV_UndefineSpace", 0x124: "ChangeEPS", 0x125: "ChangePPS", 0x126: "Clear",
    0x127: "ClearControl", 0x128: "ClockSet", 0x129: "HierarchyChangeAuth",
    0x12A: "NV_DefineSpace", 0x12B: "PCR_Allocate", 0x12C: "PCR_SetAuthPolicy",
    0x12D: "PP_Commands", 0x12E: "SetPrimaryPolicy", 0x12F: "FieldUpgradeStart",
    0x130: "ClockRateAdjust", 0x131: "CreatePrimary", 0x132: "NV_GlobalWriteLock",
    0x133: "GetCommandAuditDigest", 0x134: "NV_Increment", 0x135: "NV_SetBits",
    0x136: "NV_Extend", 0x137: "NV_Write", 0x138: "NV_WriteLock",
    0x139: "DictionaryAttackLockReset", 0x13A: "DictionaryAttackParameters",
    0x13B: "NV_ChangeAuth", 0x13C: "PCR_Event", 0x13D: "PCR_Reset", 0x13E: "SequenceComplete",
    0x13F: "SetAlgorithmSet", 0x140: "SetCommandCodeAuditStatus", 0x141: "FieldUpgradeData",
    0x142: "IncrementalSelfTest", 0x143: "SelfTest", 0x144: "Startup", 0x145: "Shutdown",
    0x146: "StirRandom", 0x147: "ActivateCredential", 0x148: "Certify", 0x149: "PolicyNV",
    0x14A: "CertifyCreation", 0x14B: "Duplicate", 0x14C: "GetTime",
    0x14D: "GetSessionAuditDigest", 0x14E: "NV_Read", 0x14F: "NV_ReadLock",
    0x150: "ObjectChangeAuth", 0x151: "PolicySecret", 0x152: "Rewrap", 0x153: "Create",
    0x154: "ECDH_ZGen", 0x155: "HMAC", 0x156: "Import", 0x157: "Load", 0x158: "Quote",
    0x159: "RSA_Decrypt", 0x15B: "HMAC_Start", 0x15C: "SequenceUpdate", 0x15D: "Sign",
    0x15E: "Unseal", 0x160: "PolicySigned", 0x161: "ContextLoad", 0x162: "ContextSave",
    0x163: "ECDH_KeyGen", 0x164: "EncryptDecrypt", 0x165: "FlushContext",
    0x167: "LoadExternal", 0x168: "MakeCredential", 0x169: "NV_ReadPublic",
    0x16A: "PolicyAuthorize", 0x16B: "PolicyAuthValue", 0x16C: "PolicyCommandCode",
    0x16D: "PolicyCounterTimer", 0x16E: "PolicyCpHash", 0x16F: "PolicyLocality",
    0x170: "PolicyNameHash", 0x171: "PolicyOR", 0x172: "PolicyTicket", 0x173: "ReadPublic",
    0x174: "RSA_Encrypt", 0x176: "StartAuthSession", 0x177: "VerifySignature",
    0x178: "ECC_Parameters", 0x179: "FirmwareRead", 0x17A: "GetCapability",
    0x17B: "GetRandom", 0x17C: "GetTestResult", 0x17D: "Hash", 0x17E: "PCR_Read",
    0x17F: "PolicyPCR", 0x180: "PolicyRestart", 0x181: "ReadClock", 0x182: "PCR_Extend",
    0x183: "PCR_SetAuthValue", 0x184: "NV_Certify", 0x185: "EventSequenceComplete",
    0x186: "HashSequenceStart", 0x187: "PolicyPhysicalPresence",
    0x188: "PolicyDuplicationSelect", 0x189: "PolicyGetDigest", 0x18A: "TestParms",
    0x18B: "Commit", 0x18C: "PolicyPassword", 0x18D: "ZGen_2Phase", 0x18E: "EC_Ephemeral",
    0x18F: "PolicyNvWritten", 0x190: "PolicyTemplate", 0x191: "CreateLoaded",
    0x192: "PolicyAuthorizeNV", 0x193: "EncryptDecrypt2", 0x194: "AC_GetCapability",
    0x195: "AC_Send", 0x196: "Policy_AC_SendSelect", 0x197: "CertifyX509",
    0x198: "ACT_SetTimeout", 0x199: "ECC_Encrypt", 0x19A: "ECC_Decrypt",
    0x19B: "PolicyCapability", 0x19C: "PolicyParameters", 0x19D: "NV_DefineSpace2",
    0x19E: "NV_ReadPublic2",
}

# Handles in the command's handle area, for the commands whose parameters are decoded below
# and for every command that can carry sessions (the authorization area follows the handles).
HANDLES = {
    "NV_UndefineSpaceSpecial": 2, "EvictControl": 2, "HierarchyControl": 1,
    "NV_UndefineSpace": 2, "Clear": 1, "ClearControl": 1, "HierarchyChangeAuth": 1,
    "NV_DefineSpace": 1, "PCR_Allocate": 1, "CreatePrimary": 1, "NV_Increment": 2,
    "NV_SetBits": 2, "NV_Extend": 2, "NV_Write": 2, "NV_WriteLock": 2,
    "DictionaryAttackLockReset": 1, "DictionaryAttackParameters": 1, "NV_ChangeAuth": 1,
    "PCR_Event": 1, "PCR_Reset": 1, "SequenceComplete": 1, "ActivateCredential": 2,
    "Certify": 2, "PolicyNV": 3, "Duplicate": 2, "NV_Read": 2, "NV_ReadLock": 2,
    "ObjectChangeAuth": 2, "PolicySecret": 2, "Create": 1, "ECDH_ZGen": 1, "HMAC": 1,
    "Import": 1, "Load": 1, "Quote": 1, "RSA_Decrypt": 1, "HMAC_Start": 1,
    "SequenceUpdate": 1, "Sign": 1, "Unseal": 1, "PolicySigned": 2, "ContextLoad": 0,
    "ContextSave": 1, "ECDH_KeyGen": 1, "EncryptDecrypt": 1, "FlushContext": 0,
    "LoadExternal": 0, "NV_ReadPublic": 1, "PolicyAuthorize": 1, "PolicyAuthValue": 1,
    "PolicyCommandCode": 1, "PolicyCpHash": 1, "PolicyLocality": 1, "PolicyNameHash": 1,
    "PolicyOR": 1, "PolicyTicket": 1, "ReadPublic": 1, "RSA_Encrypt": 1,
    "StartAuthSession": 2, "VerifySignature": 1, "GetCapability": 0, "GetRandom": 0,
    "Hash": 0, "PCR_Read": 0, "PolicyPCR": 1, "PolicyRestart": 1, "PCR_Extend": 1,
    "EventSequenceComplete": 2, "HashSequenceStart": 0, "PolicyGetDigest": 1,
    "PolicyPassword": 1, "PolicyNvWritten": 1, "CreateLoaded": 1, "PolicyAuthorizeNV": 3,
    "EncryptDecrypt2": 1, "TestParms": 0, "ReadClock": 0, "Startup": 0, "Shutdown": 0,
    "SelfTest": 0, "GetTestResult": 0, "ECC_Parameters": 0, "MakeCredential": 1,
    "CertifyCreation": 2, "NV_Certify": 3, "GetTime": 2, "StirRandom": 0,
}

ALG = {
    0x0001: "RSA", 0x0004: "SHA1", 0x0005: "HMAC", 0x0006: "AES", 0x0007: "MGF1",
    0x0008: "KEYEDHASH", 0x000A: "XOR", 0x000B: "SHA256", 0x000C: "SHA384",
    0x000D: "SHA512", 0x0010: "NULL", 0x0014: "RSASSA", 0x0015: "RSAES", 0x0016: "RSAPSS",
    0x0017: "OAEP", 0x0018: "ECDSA", 0x0019: "ECDH", 0x001A: "ECDAA", 0x001C: "ECSCHNORR",
    0x0020: "KDF1_SP800_56A", 0x0021: "KDF2", 0x0022: "KDF1_SP800_108", 0x0023: "ECC",
    0x0025: "SYMCIPHER", 0x0026: "CAMELLIA", 0x0027: "SHA3_256", 0x0040: "CTR",
    0x0041: "OFB", 0x0042: "CBC", 0x0043: "CFB", 0x0044: "ECB",
}
CURVE = {0x0003: "NIST_P256", 0x0004: "NIST_P384", 0x0005: "NIST_P521", 0x0010: "BN_P256",
         0x0020: "SM2_P256"}
SESSION_TYPE = {0: "HMAC", 1: "POLICY", 3: "TRIAL"}
CAP = {0: "ALGS", 1: "HANDLES", 2: "COMMANDS", 3: "PP_COMMANDS", 4: "AUDIT_COMMANDS",
       5: "PCRS", 6: "TPM_PROPERTIES", 7: "PCR_PROPERTIES", 8: "ECC_CURVES",
       9: "AUTH_POLICIES", 10: "ACT", 0x100: "VENDOR_PROPERTY"}


def alg(v):
    return ALG.get(v, f"0x{v:04x}")


class Reader:
    def __init__(self, b, off=0):
        self.b, self.o = b, off

    def u8(self):
        v = self.b[self.o]
        self.o += 1
        return v

    def u16(self):
        v = int.from_bytes(self.b[self.o:self.o + 2], "big")
        self.o += 2
        return v

    def u32(self):
        v = int.from_bytes(self.b[self.o:self.o + 4], "big")
        self.o += 4
        return v

    def tpm2b(self):
        n = self.u16()
        v = self.b[self.o:self.o + n]
        self.o += n
        return v


def sym_def(r, obj=False):
    a = r.u16()
    if a == 0x0010:
        return "NULL"
    bits = r.u16()
    if a == 0x000A:  # XOR: the "key bits" are a hash algorithm, and there is no mode
        return f"XOR/{alg(bits)}"
    return f"{alg(a)}-{bits}-{alg(r.u16())}"


def scheme(r):
    a = r.u16()
    if a == 0x0010:
        return "NULL"
    h = r.u16()
    if a == 0x001A:  # ECDAA carries a count
        r.u16()
    return f"{alg(a)}/{alg(h)}"


def public_area(b):
    r = Reader(b)
    t, name_alg, attrs = r.u16(), r.u16(), r.u32()
    r.tpm2b()  # authPolicy
    d = {"type": alg(t), "nameAlg": alg(name_alg), "attrs": f"0x{attrs:08x}"}
    if t == 0x0001:  # RSA
        d["sym"], d["scheme"], d["bits"] = sym_def(r), scheme(r), r.u16()
    elif t == 0x0023:  # ECC
        d["sym"], d["scheme"] = sym_def(r), scheme(r)
        d["curve"] = CURVE.get(r.u16(), "?")
        d["kdf"] = scheme(r)
    elif t == 0x0008:  # KEYEDHASH
        s = r.u16()
        if s == 0x0005:
            d["scheme"] = f"HMAC/{alg(r.u16())}"
        elif s == 0x000A:
            d["scheme"] = f"XOR/{alg(r.u16())}/{alg(r.u16())}"
        else:
            d["scheme"] = alg(s)
    elif t == 0x0025:  # SYMCIPHER
        d["sym"] = sym_def(r)
    return d


def decode_command(cmd):
    r = Reader(cmd)
    tag, _size, cc = r.u16(), r.u32(), r.u32()
    name = CC.get(cc, f"0x{cc:03x}")
    d = {"cc": name, "sessions": []}
    nh = HANDLES.get(name)
    if nh is None:
        return d
    d["handles"] = [f"0x{r.u32():08x}" for _ in range(nh)]
    if tag == 0x8002:
        auth_end = r.u32() + r.o
        while r.o < auth_end:
            h = r.u32()
            r.tpm2b()  # nonce
            attrs = r.u8()
            r.tpm2b()  # hmac
            flags = [f for bit, f in ((0x01, "cont"), (0x20, "decrypt"), (0x40, "encrypt"),
                                      (0x80, "audit")) if attrs & bit]
            d["sessions"].append({"handle": f"0x{h:08x}", "attrs": flags})
    try:
        if name == "StartAuthSession":
            r.tpm2b()
            salt = r.tpm2b()
            d["sessionType"] = SESSION_TYPE.get(r.u8(), "?")
            d["symmetric"] = sym_def(r)
            d["authHash"] = alg(r.u16())
            d["salted"] = len(salt) > 0
            d["bound"] = d["handles"][1] != "0x40000007"
        elif name in ("Create", "CreatePrimary", "CreateLoaded"):
            r.tpm2b()  # inSensitive
            d["public"] = public_area(r.tpm2b())
        elif name == "LoadExternal":
            r.tpm2b()
            d["public"] = public_area(r.tpm2b())
        elif name == "GetCapability":
            c = r.u32()
            d["capability"] = CAP.get(c, f"0x{c:x}")
            d["property"] = f"0x{r.u32():08x}"
        elif name == "Startup" or name == "Shutdown":
            d["type"] = "STATE" if r.u16() == 1 else "CLEAR"
    except IndexError:
        d["truncated"] = True
    return d


HDR = re.compile(r"^\s*(SWTPM_IO_Read|SWTPM_IO_Write|Ctrl Cmd): length (\d+)")
HEX = re.compile(r"^\s*([0-9A-F]{2} )+\s*$")

# swtpm control-channel commands that change what the TPM does with the next command. INIT is
# _TPM_Init (a platform reset); without it a replay cannot tell a reboot's Startup from a repeated
# one. SET_LOCALITY sets the locality every later command runs at.
CTRL_INIT, CTRL_SET_LOCALITY = 0x02, 0x05


def parse_log(path):
    """The log's events in order: ("cmd", command, response) and ("ctrl", name, value)."""
    events, cur, buf, want, pending = [], None, [], 0, None
    for line in open(path, errors="replace"):
        m = HDR.match(line)
        if m:
            cur, want, buf = m.group(1), int(m.group(2)), []
            continue
        if cur and HEX.match(line):
            buf.extend(bytes.fromhex(line))
            if len(buf) >= want:
                data = bytes(buf[:want])
                if cur == "SWTPM_IO_Read":
                    pending = data
                elif cur == "SWTPM_IO_Write":
                    if pending is not None:
                        events.append(("cmd", pending, data))
                    pending = None
                else:
                    code = int.from_bytes(data[0:4], "big")
                    if code == CTRL_INIT:
                        events.append(("ctrl", "init", None))
                    elif code == CTRL_SET_LOCALITY:
                        events.append(("ctrl", "locality", data[-1]))
                cur = None
    return events


def decode(path):
    for ev in parse_log(path):
        if ev[0] == "ctrl":
            d = {"ctrl": ev[1]}
            if ev[2] is not None:
                d["value"] = ev[2]
            print(json.dumps(d))
            continue
        _, cmd, rsp = ev
        d = decode_command(cmd)
        d["rc"] = f"0x{int.from_bytes(rsp[6:10], 'big'):03x}"
        d["cmd"], d["rsp"] = cmd.hex(), rsp.hex()
        print(json.dumps(d))


def summary(paths):
    per = defaultdict(Counter)
    consumers = []
    features = defaultdict(set)
    failures = defaultdict(Counter)
    for p in paths:
        c = p.rsplit("/", 1)[-1].removesuffix(".jsonl")
        consumers.append(c)
        for line in open(p):
            d = json.loads(line)
            if "ctrl" in d:
                continue
            per[d["cc"]][c] += 1
            if d["rc"] != "0x000":
                failures[d["cc"]][d["rc"]] += 1
            for s in d.get("sessions", []):
                for a in s["attrs"]:
                    if a != "cont":
                        features["session attrs"].add(a)
            if d["cc"] == "StartAuthSession":
                features["sessions"].add(
                    f'{d["sessionType"]} sym={d["symmetric"]} hash={d["authHash"]}'
                    f'{" salted" if d["salted"] else ""}{" bound" if d["bound"] else ""}')
            if "public" in d:
                pub = d["public"]
                features["objects"].add(" ".join(f"{k}={v}" for k, v in pub.items()
                                                  if k != "attrs"))
            if d["cc"] == "GetCapability":
                features["capabilities"].add(f'{d["capability"]}@{d["property"]}')
    print("| command | " + " | ".join(consumers) + " | non-success rc |")
    print("|---|" + "---|" * len(consumers) + "---|")
    for cc in sorted(per):
        cells = [str(per[cc][c]) if per[cc][c] else "" for c in consumers]
        bad = ", ".join(f"{rc}×{n}" for rc, n in failures[cc].items())
        print(f"| {cc} | " + " | ".join(cells) + f" | {bad} |")
    print(f"\n{len(per)} distinct commands.\n")
    for k in sorted(features):
        print(f"**{k}:**\n")
        for v in sorted(features[k]):
            print(f"- {v}")
        print()


if __name__ == "__main__":
    if sys.argv[1] == "--summary":
        summary(sys.argv[2:])
    else:
        decode(sys.argv[1])
