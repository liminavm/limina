# systemd-creds sealed to the TPM, with and without a PCR binding and the host key.
set -ex
cd "$(mktemp -d)"
echo -n "s3cret-tpm2" | sudo systemd-creds encrypt --with-key=tpm2 --name=t1 - t1.cred
sudo systemd-creds decrypt --name=t1 t1.cred -
echo -n "s3cret-pcr7" | sudo systemd-creds encrypt --with-key=tpm2 --tpm2-pcrs=7 --name=t2 - t2.cred
sudo systemd-creds decrypt --name=t2 t2.cred -
echo -n "s3cret-host+tpm2" | sudo systemd-creds encrypt --with-key=host+tpm2 --name=t3 - t3.cred
sudo systemd-creds decrypt --name=t3 t3.cred -
echo -n "s3cret-user" | systemd-creds --user encrypt --name=t4 - t4.cred
systemd-creds --user decrypt --name=t4 t4.cred -
