// clipboard@limina — the GNOME tier of limina's host↔guest clipboard bridge.
//
// Why this exists: core Wayland gates clipboard access on keyboard focus, so a
// background client (limina-agent-session) cannot touch the selection at all. The
// standardized answer, ext-data-control-v1, is rejected by GNOME upstream on privacy
// grounds (mutter#524), and the only stock door — mutter's RemoteDesktop D-Bus API —
// keeps the "screen is shared" indicator lit for the whole session. A shell extension
// runs *inside* the compositor, where Meta.Selection is directly scriptable: no mutter
// patch to rebase every distro bump, no indicator, and a distro mutter update can't
// displace it (that displacement is exactly what demoted the dogfood guest to the
// indicator tier on 2026-07-11).
//
// Shape: exports org.limina.Clipboard on the session bus; limina-agent-session probes
// it as the middle backend (after ext-data-control, before RemoteDesktop) and speaks
// plain method calls/signals. Unlike the other two backends there is no transfer
// choreography: Set() carries the full content and parks it in a
// Meta.SelectionSourceMemory, so the compositor itself serves every guest paste.
//
// Loop prevention mirrors the other backends: we keep a reference to the source WE
// own the selection with; the owner-changed echo for our own Set() then reports
// isOwner=true, which the agent ignores (otherwise the host would be offered its own
// clipboard back).

import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import Meta from 'gi://Meta';

import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

const TEXT_MIME = 'text/plain;charset=utf-8';

const BRIDGE_NAME = 'org.limina.Clipboard';
const BRIDGE_PATH = '/org/limina/Clipboard';

// Interface version, for forward-compatible agent probing (property `Version`).
const BRIDGE_VERSION = 1;

const BRIDGE_IFACE = `
<node>
  <interface name="org.limina.Clipboard">
    <method name="Read">
      <arg type="s" direction="in" name="mimeType"/>
      <arg type="ay" direction="out" name="data"/>
    </method>
    <method name="Set">
      <arg type="ay" direction="in" name="data"/>
    </method>
    <signal name="OwnerChanged">
      <arg type="b" name="hasText"/>
      <arg type="b" name="isOwner"/>
    </signal>
    <property name="Version" type="u" access="read"/>
  </interface>
</node>`;

class ClipboardBridge {
    constructor() {
        this._selection = global.display.get_selection();
        this._source = null; // our live source while we own the selection
        this._cancellable = new Gio.Cancellable();
        this._dbusImpl = Gio.DBusExportedObject.wrapJSObject(BRIDGE_IFACE, this);
        this._dbusImpl.export(Gio.DBus.session, BRIDGE_PATH);
        this._nameId = Gio.DBus.session.own_name(
            BRIDGE_NAME, Gio.BusNameOwnerFlags.NONE, null, null);
        this._ownerChangedId = this._selection.connect(
            'owner-changed', (_selection, selectionType, owner) => {
                if (selectionType !== Meta.SelectionType.SELECTION_CLIPBOARD)
                    return;
                const hasText =
                    owner !== null && owner.get_mimetypes().includes(TEXT_MIME);
                const isOwner = this._source !== null && owner === this._source;
                if (!isOwner)
                    this._source = null; // someone else took the selection
                this._dbusImpl.emit_signal(
                    'OwnerChanged', new GLib.Variant('(bb)', [hasText, isOwner]));
            });
    }

    destroy() {
        this._cancellable.cancel();
        this._selection.disconnect(this._ownerChangedId);
        Gio.DBus.session.unown_name(this._nameId);
        this._dbusImpl.unexport();
        // If we own the selection, leave it owned: the source lives in the shell
        // process (not in this object), and yanking it would eat the user's last copy.
        this._source = null;
    }

    get Version() {
        return BRIDGE_VERSION;
    }

    /// Read the current selection content (any owner: a guest app's copy). No size
    /// cap here — the agent enforces its own frame limit and answers TOO_LARGE.
    ReadAsync(params, invocation) {
        const [mimeType] = params;
        const output = Gio.MemoryOutputStream.new_resizable();
        this._selection.transfer_async(
            Meta.SelectionType.SELECTION_CLIPBOARD, mimeType, -1, output,
            this._cancellable, (selection, res) => {
                try {
                    selection.transfer_finish(res);
                    output.close(null);
                    const data = output.steal_as_bytes().toArray();
                    invocation.return_value(GLib.Variant.new('(ay)', [data]));
                } catch (e) {
                    invocation.return_error_literal(
                        Gio.DBusError, Gio.DBusError.FAILED,
                        `selection transfer failed: ${e.message}`);
                }
            });
    }

    /// Own the selection with host clipboard content. The memory source serves every
    /// subsequent guest paste in-process; the agent is not involved again until the
    /// next owner change.
    Set(data) {
        this._source = Meta.SelectionSourceMemory.new(
            TEXT_MIME, new GLib.Bytes(data));
        this._selection.set_owner(
            Meta.SelectionType.SELECTION_CLIPBOARD, this._source);
    }
}

export default class LiminaClipboardExtension extends Extension {
    enable() {
        this._bridge = new ClipboardBridge();
    }

    disable() {
        this._bridge?.destroy();
        this._bridge = null;
    }
}
