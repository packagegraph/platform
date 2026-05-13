# CentOS Stream 10 — MicroShift node for PackageGraph
# 120GB disk, optimized for TopoLVM PVC provisioning
#
# Usage:
#   Boot CentOS Stream 10 installer with:
#   inst.ks=https://raw.githubusercontent.com/packagegraph/platform/main/deploy/kickstart/cs10-microshift.ks
#
# Post-install:
#   1. Copy kubeconfig: scp k8s1:/var/lib/microshift/resources/kubeadmin/kubeconfig ~/.kube/config-2
#   2. Deploy: KUBECONFIG=~/.kube/config-2 oc apply -k deploy/overlays/dev

# --- Installation ---
text
reboot
eula --agreed
firstboot --disable

# --- Locale ---
lang en_US.UTF-8
keyboard us
timezone America/Los_Angeles --utc

# --- Network ---
network --bootproto=dhcp --device=link --activate --hostname=k8s1.west-1.kafka.tel

# --- Root + user ---
rootpw --lock
user --name=bharrington --groups=wheel --shell=/bin/bash
sshkey --username=bharrington "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPLACEHOLDER_REPLACE_WITH_REAL_KEY bharrington"

# --- Security ---
firewall --enabled --service=ssh --port=6443:tcp --port=80:tcp --port=443:tcp
selinux --enforcing

# --- Disk: 120GB, single VG with free space for TopoLVM ---
# Wipes the target disk. TopoLVM carves PVCs from free VG extents.
ignoredisk --only-use=sda
zerombr
clearpart --all --drives=sda --initlabel
bootloader --location=mbr --driveorder=sda

part /boot/efi --fstype=efi --size=600
part /boot     --fstype=xfs --size=1024
part pv.01     --fstype=lvmpv --size=1 --grow

volgroup cs pv.01

# OS filesystems — intentionally small to leave room for TopoLVM
logvol swap   --vgname=cs --name=swap   --fstype=swap --size=4096
logvol /      --vgname=cs --name=root   --fstype=xfs  --size=25600
logvol /var   --vgname=cs --name=var    --fstype=xfs  --size=30720

# Remaining ~58GB stays as free extents in VG "cs" for TopoLVM PVCs:
#   fuseki-tdb2:  40Gi  (TDB2 downloaded from Minio, ~25-30GB actual)
#   minio-data:   15Gi  (nt-output + tdb2 snapshots, ~12GB actual)
#   Free:         ~3Gi  (headroom)

# --- Package selection ---
%packages
@^minimal-environment
# MicroShift
microshift
microshift-olm
microshift-multus
# CRI-O (pulled as dependency, but be explicit)
cri-o
cri-tools
# Container networking
openvswitch3.3
# System tools
podman
skopeo
buildah
git
vim-enhanced
tmux
htop
iotop
jq
bash-completion
# TopoLVM (included with MicroShift)
# Firewall
firewalld
# Chrony for NTP
chrony
%end

# --- Post-install ---
%post --log=/root/ks-post.log
set -euxo pipefail

# Enable MicroShift
systemctl enable microshift.service
systemctl enable crio.service

# Configure MicroShift storage — TopoLVM uses VG "cs"
mkdir -p /etc/microshift
cat > /etc/microshift/config.yaml <<'EOF'
storage:
  driver: lvms
  optionalCSIComponents:
    - snapshot-controller
    - snapshot-webhook
  lvms:
    deviceClasses:
      - name: default
        volumeGroup: cs
        default: true
        thinPoolConfig:
          name: thin-pool-0
          sizePercent: 90
          overprovisionRatio: 5
EOF

# Pull MicroShift container images (saves time on first boot)
microshift show-config --mode effective 2>/dev/null || true

# Configure container registry auth (if needed for ghcr.io)
mkdir -p /etc/containers/registries.conf.d
cat > /etc/containers/registries.conf.d/999-packagegraph.conf <<'EOF'
[[registry]]
prefix = "ghcr.io/packagegraph"
location = "ghcr.io/packagegraph"
EOF

# Kernel tuning for mmap-heavy workloads (TDB2/Fuseki)
cat > /etc/sysctl.d/90-packagegraph.conf <<'EOF'
# Increase max mmap regions for TDB2 (default 65530 is too low for large indexes)
vm.max_map_count = 262144

# Increase page cache pressure — prefer keeping mmap'd TDB2 pages resident
vm.vfs_cache_pressure = 50

# Allow overcommit for JVM heap reservation (Fuseki, tdb2.tdbloader)
vm.overcommit_memory = 1
EOF

# Journal size limit (prevent /var/log filling up)
mkdir -p /etc/systemd/journald.conf.d
cat > /etc/systemd/journald.conf.d/size.conf <<'EOF'
[Journal]
SystemMaxUse=2G
EOF

# Enable chrony
systemctl enable chronyd.service

echo "=== PackageGraph MicroShift node provisioned ==="
echo "After reboot: systemctl start microshift"
echo "Kubeconfig: /var/lib/microshift/resources/kubeadmin/kubeconfig"
%end
