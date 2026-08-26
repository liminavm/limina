# A workspace whose only job is to enable Xcode's Debug menu

Xcode greys out **Debug -> Attach to Process** unless a project or workspace is open, and a GPU
capture sent to the `MTLCaptureDestinationDeveloperTools` destination needs a developer tool
attached to the target process. Neither requirement has anything to do with building code, so this
is an empty SwiftPM package that Xcode opens as a workspace.

    open Package.swift        # Xcode opens the package; the Debug menu becomes live
    # Debug -> Attach to Process -> by PID or Name -> the limina-vmm pid

The worker must have been started with `MTL_CAPTURE_ENABLED=1` (the boot script forwards it), or
Metal refuses programmatic capture and the trace never appears.

Used by the notification-text investigation: the file-writing capture destination segfaults inside
Apple's `GPUToolsCapture` on this command stream, and the DeveloperTools destination is the only
other way to aim a capture at the pass that fails. See ../RESULTS.md.
