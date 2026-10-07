# tpm2-pkcs11: a token with an ECC and an RSA key, used through the PKCS#11 module.
set -ex
export TPM2_PKCS11_STORE=$(mktemp -d)
tpm2_ptool init --path=$TPM2_PKCS11_STORE
tpm2_ptool addtoken --pid=1 --label=vtpm --sopin=sopin --userpin=userpin --path=$TPM2_PKCS11_STORE
tpm2_ptool addkey --algorithm=ecc256 --label=vtpm --key-label=ec --userpin=userpin --path=$TPM2_PKCS11_STORE
tpm2_ptool addkey --algorithm=rsa2048 --label=vtpm --key-label=rsa --userpin=userpin --path=$TPM2_PKCS11_STORE
M=/usr/lib64/pkcs11/libtpm2_pkcs11.so
pkcs11-tool --module $M -L
echo data > /tmp/p11data
pkcs11-tool --module $M --token-label vtpm -l -p userpin --sign -m ECDSA-SHA256 --label ec -i /tmp/p11data -o /tmp/p11.ecsig
ls -l /tmp/p11.*sig
# RSA signing through this module answers CKR_MECHANISM_INVALID on swtpm too (P0 RESULTS.md); kept out.
