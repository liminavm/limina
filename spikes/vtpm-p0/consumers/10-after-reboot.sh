# After a reboot: the PIN-enrolled LUKS volume from 03 still unlocks (PCR 7 replayed identically),
# and the persistent SRK from 01 is still there.
set -ex
sudo tpm2_getcap handles-persistent
sudo PIN=4321 /usr/lib/systemd/systemd-cryptsetup attach vtpmluks /var/tmp/vtpm-luks.img - tpm2-device=auto,headless=1
ls -l /dev/mapper/vtpmluks
sudo /usr/lib/systemd/systemd-cryptsetup detach vtpmluks
