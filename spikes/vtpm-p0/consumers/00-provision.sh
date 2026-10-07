# Guest provisioning the consumers need: the tss group for /dev/tpmrm0, the consumer packages,
# and ssh-tpm-agent from its release (Fedora does not package it). Takes effect on the next login.
set -ex
sudo usermod -aG tss claude
sudo dnf install -y -q tpm2-pkcs11 tpm2-pkcs11-tools clevis clevis-luks tpm2-openssl opensc openssl
mkdir -p /var/tmp/ssh-tpm-agent
cd /var/tmp/ssh-tpm-agent
curl -sSLO https://github.com/Foxboron/ssh-tpm-agent/releases/download/v0.9.0/ssh-tpm-agent-v0.9.0-linux-arm64.tar.gz
tar xzf ssh-tpm-agent-v0.9.0-linux-arm64.tar.gz
sudo install -m755 ssh-tpm-agent/ssh-tpm-add ssh-tpm-agent/ssh-tpm-agent ssh-tpm-agent/ssh-tpm-hostkeys ssh-tpm-agent/ssh-tpm-keygen /usr/local/bin/
