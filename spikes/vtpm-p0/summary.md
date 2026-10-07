| command | boot | 01-tpm2-setup | 02-creds | 03-cryptenroll | 04-clevis | 05-ssh-tpm-agent | 06-pkcs11 | 07-measured-boot | 08-openssl | 09-reboot | 10-after-reboot | non-success rc |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| ContextLoad | 1 | 11 | 128 | 94 | 90 | 32 | 79 |  | 38 | 1 | 24 |  |
| ContextSave | 1 | 14 | 152 | 108 | 108 | 44 | 104 |  | 50 | 1 | 27 | 0x910×103 |
| Create |  | 1 | 4 | 2 | 2 | 2 | 5 |  | 2 |  |  |  |
| CreatePrimary | 1 | 2 | 8 |  | 4 | 4 | 1 |  | 5 | 1 |  |  |
| ECC_Parameters |  |  |  |  |  |  |  |  | 4 |  |  |  |
| EvictControl |  | 1 |  |  |  |  | 1 |  |  |  |  |  |
| FlushContext | 2 | 13 | 108 | 52 | 67 | 32 | 53 |  | 48 | 3 | 14 |  |
| GetCapability | 76 | 15 | 32 | 18 |  | 4 | 29 | 6 | 12 | 76 | 5 |  |
| GetRandom | 14 | 5 | 8 | 4 | 4 |  | 25 | 3 | 4 | 7 | 2 |  |
| Hash |  |  |  |  | 2 |  |  |  |  |  |  |  |
| HashSequenceStart |  |  |  |  |  |  | 1 |  | 2 |  |  |  |
| HierarchyChangeAuth | 1 |  |  |  |  |  |  |  |  | 1 |  |  |
| Load |  |  | 4 | 4 | 2 | 2 | 5 |  | 5 |  | 1 |  |
| NV_DefineSpace |  | 2 |  |  |  |  |  |  |  |  |  |  |
| NV_Extend |  | 2 |  |  |  |  |  |  |  |  |  |  |
| PCR_Extend | 31 | 2 |  |  |  |  |  |  |  | 31 |  |  |
| PCR_Read | 41 | 8 | 4 | 10 | 2 |  |  | 15 |  | 41 | 1 |  |
| PolicyAuthValue |  |  |  | 2 |  |  |  |  |  |  | 1 |  |
| PolicyGetDigest |  |  | 4 | 4 | 2 |  |  |  |  |  | 1 |  |
| PolicyPCR |  |  | 1 | 4 | 2 |  |  |  |  |  | 1 |  |
| RSA_Decrypt |  |  |  |  |  | 1 |  |  |  |  |  |  |
| ReadClock | 1 |  |  |  |  |  | 2 |  |  |  |  | 0x101×1 |
| ReadPublic |  | 2 |  | 4 |  |  | 2 |  |  |  |  |  |
| SelfTest | 2 |  |  |  |  |  |  |  |  | 2 |  |  |
| SequenceComplete |  |  |  |  |  |  | 1 |  | 2 |  |  |  |
| SequenceUpdate |  |  |  |  |  |  | 1 |  | 2 |  |  |  |
| Shutdown |  |  |  |  |  |  |  |  |  | 1 |  |  |
| Sign |  |  |  |  |  | 1 | 2 |  | 2 |  |  |  |
| StartAuthSession | 1 | 1 | 12 | 10 | 12 | 6 | 14 |  |  | 1 | 2 |  |
| Startup | 1 | 3 | 8 | 4 |  |  | 2 | 1 |  | 1 | 1 | 0x100×19 |
| TestParms |  | 5 | 12 | 4 |  |  | 36 | 1 |  |  | 1 | 0x1c4×4 |
| Unseal |  |  | 4 | 4 | 2 |  | 3 |  |  |  | 1 |  |

32 distinct commands.

**capabilities:**

- ALGS@0x00000000
- ALGS@0x00000001
- COMMANDS@0x00000000
- COMMANDS@0x0000011f
- COMMANDS@0x00000124
- ECC_CURVES@0x00000000
- HANDLES@0x81000000
- HANDLES@0x81000001
- PCRS@0x00000000
- TPM_PROPERTIES@0x00000000
- TPM_PROPERTIES@0x00000100
- TPM_PROPERTIES@0x00000105
- TPM_PROPERTIES@0x0000010b
- TPM_PROPERTIES@0x0000010c
- TPM_PROPERTIES@0x0000011e
- TPM_PROPERTIES@0x0000011f
- TPM_PROPERTIES@0x00000129
- TPM_PROPERTIES@0x0000012c

**objects:**

- type=ECC nameAlg=SHA256 sym=AES-128-CFB scheme=NULL curve=NIST_P256 kdf=NULL
- type=ECC nameAlg=SHA256 sym=NULL scheme=NULL curve=NIST_P256 kdf=NULL
- type=KEYEDHASH nameAlg=SHA256 scheme=NULL
- type=RSA nameAlg=SHA256 sym=AES-128-CFB scheme=NULL bits=2048
- type=RSA nameAlg=SHA256 sym=NULL scheme=NULL bits=2048

**session attrs:**

- decrypt
- encrypt

**sessions:**

- HMAC sym=AES-128-CFB hash=SHA256
- HMAC sym=AES-128-CFB hash=SHA256 salted
- HMAC sym=AES-128-CFB hash=SHA256 salted bound
- HMAC sym=NULL hash=SHA256
- POLICY sym=AES-128-CFB hash=SHA256
- POLICY sym=AES-128-CFB hash=SHA256 salted
- TRIAL sym=AES-128-CFB hash=SHA256

