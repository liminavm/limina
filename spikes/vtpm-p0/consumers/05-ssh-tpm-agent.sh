# ssh-tpm-agent: an ECDSA and an RSA key that live in the TPM, signing through the agent.
set -ex
d=$(mktemp -d); cd $d
ssh-tpm-keygen -t ecdsa -N "" -f $d/id_ecdsa
ssh-tpm-keygen -t rsa -N "" -f $d/id_rsa
ssh-tpm-agent -l $d/agent.sock > $d/agent.log 2>&1 &
apid=$!
for i in $(seq 50); do [ -S $d/agent.sock ] && break; sleep 0.1; done
export SSH_AUTH_SOCK=$d/agent.sock SSH_TPM_AUTH_SOCK=$d/agent.sock
ssh-tpm-add $d/id_ecdsa.tpm
ssh-tpm-add $d/id_rsa.tpm
ssh-add -l
echo data > data
ssh-keygen -Y sign -f $d/id_ecdsa.pub -n file data
mv data.sig data.ecdsa.sig
ssh-keygen -Y sign -f $d/id_rsa.pub -n file data
ls data*.sig
kill $apid
