# The tpm2 OpenSSL provider: a TPM-held key generating and signing.
set -ex
cd "$(mktemp -d)"
openssl genpkey -provider tpm2 -provider default -propquery '?provider=tpm2' -algorithm EC -pkeyopt group:P-256 -out ec.pem
echo data > data
openssl pkeyutl -provider tpm2 -provider default -sign -inkey ec.pem -in data -rawin -digest sha256 -out sig
openssl genpkey -provider tpm2 -provider default -propquery '?provider=tpm2' -algorithm RSA -pkeyopt bits:2048 -out rsa.pem
openssl pkeyutl -provider tpm2 -provider default -sign -inkey rsa.pem -in data -rawin -digest sha256 -out rsig
head -n1 ec.pem rsa.pem
ls -l sig rsig
