# LUKS2 enrolled to the TPM (PCR 7), unlocked through systemd-cryptsetup; then with a PIN.
set -ex
sudo rm -f /var/tmp/vtpm-luks.img
sudo truncate -s 64M /var/tmp/vtpm-luks.img
img=/var/tmp/vtpm-luks.img
echo -n "passphrase" | sudo cryptsetup luksFormat --type luks2 --batch-mode --pbkdf pbkdf2 --pbkdf-force-iterations 1000 $img -
sudo PASSWORD=passphrase systemd-cryptenroll --tpm2-device=auto --tpm2-pcrs=7 $img
sudo /usr/lib/systemd/systemd-cryptsetup attach vtpmluks $img - tpm2-device=auto,headless=1
ls -l /dev/mapper/vtpmluks
sudo /usr/lib/systemd/systemd-cryptsetup detach vtpmluks
sudo PASSWORD=passphrase NEWPIN=4321 systemd-cryptenroll --wipe-slot=tpm2 --tpm2-device=auto --tpm2-pcrs=7 --tpm2-with-pin=yes $img
sudo PIN=4321 /usr/lib/systemd/systemd-cryptsetup attach vtpmluks $img - tpm2-device=auto,headless=1
ls -l /dev/mapper/vtpmluks
sudo /usr/lib/systemd/systemd-cryptsetup detach vtpmluks
sudo cryptsetup luksDump $img
