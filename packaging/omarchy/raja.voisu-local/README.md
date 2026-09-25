# Voisu Local Omarchy plugin

`raja.voisu-local` is a bar widget for picking one Voisu development model at a
time. It talks only to the existing CLI:

```text
voisu local-dev status --json
voisu local-dev select <model-id>
```

`select` already writes the single `selection.json`. Activating model B
deactivates model A. This plugin does not keep a second selection store.

## Bar

The pill shows a short label for the selected catalog id, or `Local` when none
is selected. Left click opens the panel. Middle click refreshes status.

## Panel

The panel always lists the seven catalog ids:

- `nemotron-3.5-0.6b`
- `parakeet-tdt-0.6b-v3`
- `qwen3-asr-0.6b`
- `qwen3-asr-1.7b`
- `fun-asr-nano-2512`
- `moonshine-streaming-medium`
- `cohere-transcribe-03-2026`

Exactly one row can be selected. Clicking a registered model runs
`voisu local-dev select`. Unregistered models stay visible and disabled, with
the CLI error text. Selecting while Recording surfaces the CLI busy error and
does not force a switch.

Keyboard: `j`/`k` or arrows move, Enter/Space selects, `r` refreshes, Esc
closes.

## Binary

The packaged `/usr/bin/voisu` does not yet have `local-dev`. The widget setting
`voisuBinary` defaults to:

```text
/home/raja/Anuraj-dev/voisu/target/release/voisu
```

Override it on the bar entry in `~/.config/omarchy/shell.json` if needed.

## Install

After review, copy the plugin directory to:

```text
~/.config/omarchy/plugins/raja.voisu-local/
```

Then:

```bash
omarchy plugin validate ~/.config/omarchy/plugins/raja.voisu-local
omarchy-shell shell rescanPlugins
omarchy plugin enable raja.voisu-local --section right
```

The plugin uses the existing Omarchy bar theme. Do not put an `omarchy.*` id on
it, and do not add symlinks inside the plugin folder.

## Tests

From this directory:

```bash
node test/model-test.js
omarchy plugin validate .
```
