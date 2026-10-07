# Does the firmware event log replay to the PCRs the TPM reports? (P3's oracle.)
set -x
sudo tpm2_eventlog /sys/kernel/security/tpm0/binary_bios_measurements > /tmp/eventlog.yaml
sed -n '/^pcrs:/,$p' /tmp/eventlog.yaml
sudo tpm2_pcrread sha256:0,1,2,3,4,5,6,7,8,9+sha1:0,1,2,3,4,5,6,7
sudo /usr/lib/systemd/systemd-pcrlock log 2>&1
sudo tpm2_getcap pcrs
