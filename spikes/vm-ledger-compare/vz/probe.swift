// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Minimal headless Virtualization.framework VM: EFI-boot a raw Fedora disk,
// virtio-blk + NAT networking, 8 GiB / 4 vCPUs. The control leg of the
// per-pmap ledger comparison — see ../README.md.

import Foundation
import Virtualization

guard CommandLine.arguments.count == 2 else {
    FileHandle.standardError.write("usage: vz-probe <disk.raw>\n".data(using: .utf8)!)
    exit(2)
}
let diskPath = CommandLine.arguments[1]

let cfg = VZVirtualMachineConfiguration()
cfg.cpuCount = 4
cfg.memorySize = 8 * 1024 * 1024 * 1024
cfg.platform = VZGenericPlatformConfiguration()

let boot = VZEFIBootLoader()
let storeURL = URL(fileURLWithPath: "vz-efi-vars.fd")
if FileManager.default.fileExists(atPath: storeURL.path) {
    boot.variableStore = VZEFIVariableStore(url: storeURL)
} else {
    boot.variableStore = try VZEFIVariableStore(creatingVariableStoreAt: storeURL)
}
cfg.bootLoader = boot

let attachment = try VZDiskImageStorageDeviceAttachment(
    url: URL(fileURLWithPath: diskPath), readOnly: false)
cfg.storageDevices = [VZVirtioBlockDeviceConfiguration(attachment: attachment)]

let net = VZVirtioNetworkDeviceConfiguration()
net.attachment = VZNATNetworkDeviceAttachment()
cfg.networkDevices = [net]

try cfg.validate()

let vm = VZVirtualMachine(configuration: cfg)
print("vz-probe pid: \(ProcessInfo.processInfo.processIdentifier)")
print("guest IP: grep the MAC in /var/db/dhcpd_leases after boot")

vm.start { result in
    switch result {
    case .success:
        print("VM started")
    case .failure(let error):
        FileHandle.standardError.write("VM start failed: \(error)\n".data(using: .utf8)!)
        exit(1)
    }
}

RunLoop.main.run()
