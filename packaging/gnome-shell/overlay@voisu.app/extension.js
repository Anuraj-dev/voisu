import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import Meta from 'gi://Meta';
import Shell from 'gi://Shell';
import St from 'gi://St';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';
import {MAX_RESPONSE_BYTES, RequestLifecycle, capsuleForResponse, parseBoundedResponse} from './state.mjs';

const PROTOCOL_VERSION = 1;
const VISIBLE_MS = 1400;
const ROUND_TRIP_MS = 750;

export default class VoisuOverlay extends Extension {
  enable() {
    this._requestLifecycle ??= new RequestLifecycle();
    this._requestGeneration = this._requestLifecycle.enable();
    this._settings = this.getSettings();
    this._capsule = new St.Label({style_class: 'voisu-capsule', visible: false});
    Main.layoutManager.addTopChrome(this._capsule);
    this._position();
    this._lastEvent = null;
    this._pollInFlight = false;
    this._settingsChanged = this._settings.connect('changed::voisu-trigger-key', () => this._installBinding());
    this._installBinding();
    this._pollId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, 150, () => {
      this._poll();
      return GLib.SOURCE_CONTINUE;
    });
  }

  disable() {
    this._requestLifecycle?.disable();
    if (this._pollId) GLib.source_remove(this._pollId);
    this._pollId = 0;
    if (this._hideId) GLib.source_remove(this._hideId);
    this._hideId = 0;
    if (this._settingsChanged) this._settings.disconnect(this._settingsChanged);
    this._settingsChanged = 0;
    this._pollInFlight = false;
    this._removeBinding();
    this._settings?.set_boolean('trigger-active', false);
    this._capsule?.destroy();
    this._capsule = null;
  }

  _position() {
    const monitor = Main.layoutManager.primaryMonitor;
    const [, width] = this._capsule.get_preferred_width(-1);
    this._capsule.set_position(
      Math.floor(monitor.x + (monitor.width - width) / 2),
      monitor.y + monitor.height - 110);
  }

  _installBinding() {
    this._removeBinding();
    this._settings.set_boolean('trigger-active', false);
    try {
      this._bindingAttempted = true;
      const action = Main.wm.addKeybinding('voisu-trigger-key', this._settings, Meta.KeyBindingFlags.NONE,
        Shell.ActionMode.NORMAL | Shell.ActionMode.OVERVIEW,
        () => this._sendCommand('toggle'));
      if (action === Meta.KeyBindingAction.NONE)
        throw new Error('GNOME could not register the Voisu Trigger Key');
      this._settings.set_boolean('trigger-active', true);
    } catch (error) {
      console.warn(`Voisu Trigger Key not installed: ${error.message}`);
    }
  }

  _removeBinding() {
    if (!this._bindingAttempted) return;
    Main.wm.removeKeybinding('voisu-trigger-key');
    this._bindingAttempted = false;
  }

  _socketPath() {
    return `${GLib.get_user_runtime_dir()}/voisu/v${PROTOCOL_VERSION}/daemon.sock`;
  }

  _sendCommand(command) {
    this._request(command, response => this._render(response),
      () => this._show('Voisu unavailable', 'voisu-failure', true));
  }

  _poll() {
    if (this._pollInFlight) return;
    this._pollInFlight = true;
    this._request('overlaystatus', response => {
      this._pollInFlight = false;
      this._render(response);
    }, () => {
      // Daemon availability is diagnosed by `voisu doctor`; idle polling stays quiet.
      this._pollInFlight = false;
    });
  }

  _request(command, onReply, onError) {
    const client = new Gio.SocketClient();
    const address = new Gio.UnixSocketAddress({path: this._socketPath()});
    const cancellable = new Gio.Cancellable();
    let settled = false;
    let connection = null;
    let timeoutId = 0;
    let request = null;
    const cleanup = () => {
      if (timeoutId) {
        GLib.source_remove(timeoutId);
        timeoutId = 0;
      }
      connection?.close_async(GLib.PRIORITY_DEFAULT, null, null);
      connection = null;
    };
    const cancel = () => {
      if (settled) return;
      settled = true;
      cancellable.cancel();
      cleanup();
    };
    const finish = (reply, failed = false) => {
      if (settled) return;
      settled = true;
      cleanup();
      if (!this._requestLifecycle.finish(request)) return;
      if (failed) onError?.(); else onReply?.(reply);
    };
    request = this._requestLifecycle.register(this._requestGeneration, cancel);
    if (settled) return;
    timeoutId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, ROUND_TRIP_MS, () => {
      cancellable.cancel();
      finish(null, true);
      return GLib.SOURCE_REMOVE;
    });
    client.connect_async(address, cancellable, (_client, result) => {
      try {
        const connected = client.connect_finish(result);
        if (settled) {
          connected.close_async(GLib.PRIORITY_DEFAULT, null, null);
          return;
        }
        connection = connected;
      } catch (_) {
        finish(null, true);
        return;
      }
      const request = new TextEncoder().encode(JSON.stringify({version: PROTOCOL_VERSION, command}) + '\n');
      connection.output_stream.write_all_async(request, GLib.PRIORITY_DEFAULT, cancellable, (output, writeResult) => {
        try {
          output.write_all_finish(writeResult);
          if (settled) return;
          const chunks = [];
          let size = 0;
          const readNext = () => connection.input_stream.read_bytes_async(
            4096, GLib.PRIORITY_DEFAULT, cancellable, (stream, readResult) => {
              try {
                const bytes = stream.read_bytes_finish(readResult);
                if (settled) return;
                const data = bytes.get_data();
                if (!data.length) throw new Error('daemon closed without a response');
                size += data.length;
                if (size > MAX_RESPONSE_BYTES) throw new Error('daemon response exceeds bound');
                chunks.push(...data);
                const newline = chunks.indexOf(10);
                if (newline < 0) return readNext();
                const line = new TextDecoder().decode(new Uint8Array(chunks.slice(0, newline)));
                finish(parseBoundedResponse(line));
              } catch (_) {
                finish(null, true);
              }
            });
          readNext();
        } catch (_) {
          finish(null, true);
        }
      });
    });
  }

  _render(response) {
    const capsule = capsuleForResponse(response, this._lastEvent);
    if (capsule.eventIdentity) this._lastEvent = capsule.eventIdentity;
    if (capsule.visible)
      return this._show(capsule.text, capsule.styleClass, capsule.terminal);
    if (response.overlay_event) {
      // A retained terminal event already shown stays hidden instead of
      // resurfacing on every observer poll.
      return;
    }
    this._capsule.hide();
  }

  _show(text, phaseClass, terminal) {
    for (const name of ['voisu-recording', 'voisu-processing', 'voisu-success', 'voisu-failure', 'voisu-nospeech'])
      this._capsule.remove_style_class_name(name);
    this._capsule.add_style_class_name(phaseClass);
    this._capsule.text = text;
    this._capsule.show();
    this._position();
    if (this._hideId) GLib.source_remove(this._hideId);
    if (terminal) this._hideId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, VISIBLE_MS, () => {
      this._capsule.hide();
      this._hideId = 0;
      return GLib.SOURCE_REMOVE;
    });
  }
}
