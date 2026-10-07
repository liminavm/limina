# systemd's SRK provisioning (what systemd-tpm2-setup.service does on a measured-UKI boot).
set -x
sudo /usr/lib/systemd/systemd-tpm2-setup
sudo tpm2_getcap handles-persistent
ls -l /run/systemd/tpm2-srk-public-key.* /var/lib/systemd/tpm2-srk-public-key.* 2>&1
