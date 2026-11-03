# Windows: user-chosen Trigger Key (research note)

> 2026-11-03 · research only, no code changed · for voisu-d4 (implementer) and voisu-79 (lead).
> Tags: **[C]** confirmed from a cited source or from Voisu source, **[I]** inference, **[U]** unverified and needs the Win11 real-host gate.

## TL;DR

1. **Use a constrained set, not free choice.** Allow four shapes: a modifier-only chord; one right-side modifier; one dedicated key; modifiers plus one key. Reject combinations that Windows owns, that ordinary typing produces, or that cannot report a key-up. Wispr Flow, PowerToys and the AutoHotkey docs all work this way.
2. **The real cost of Ctrl+Win is the false start, not a broken shortcut.** Ctrl+Win+Left still reaches Windows today. Before it does, Voisu runs a whole Recording and then cancels it: overlay shown, mic opened, Deepgram and Groq streams opened. A **commit delay** fixes this cheaply and should ship whatever key the user picks (§5).
3. **Rules the hook needs for new keys:**
   - Inject the mask key whenever Alt or Win is down, or whenever Voisu swallows a key under held modifiers.
   - Swallow the trigger key's down, repeats and up together.
   - Never treat Pause as hold-capable.
   - Revisit the blanket `LLKHF_INJECTED` drop, because users who remap keys with PowerToys produce injected events.

## 1. How other apps do it

| App | Choice model | Rules worth copying | Recorder |
|---|---|---|---|
| **Wispr Flow** (Win) | Free choice inside validation. Default PTT is Ctrl+Win. | Must include a modifier, Caps Lock, or a mouse button. At most 3 keys. Cannot use both sides of one modifier. **Standalone Ctrl (either side) is reserved.** Caps Lock alone is allowed for PTT and Hands-free only. Reserved list: Win+E/R/L/D/Tab/I/S/X/P/Up/Down/Q, most Ctrl+letter edit keys, Alt+Tab/F4, Ctrl+Alt+Del, Ctrl+Shift+Esc, F5, F11, PrtScn. "Ctrl+Win prevents the Start/Search action while held; Win alone still works." [C] | "Press your combination; keyboard shortcuts save on release". Up to 4 bindings per action. Esc cancels the capture. An overlap between Hands-free and PTT **warns but still saves**. [C] |
| **SuperWhisper** | Rebindable PTT and toggle. A quick tap of PTT toggles. The docs are Mac-centric with no Windows key rules. A feature request asks for Ctrl+Win PTT on Windows, so it was not supported when filed. [C] | Same tap-toggles idea as Voisu. | Not documented. |
| **Aqua Voice** | Free choice. Up to 5 keys per combination and 5 alternatives per action. Keys are swallowed ("will NOT bubble down to your active application") and take priority over system shortcuts. Suggested alternates are F1–F12 and Numpad +. [C] | Allowing multiple alternates. | Settings → Keybindings → "Add Alternative" → press the keys. [C] |
| **Discord** PTT | Any key or mouse button. A release-delay slider (third-party guides say 0–2000 ms, default 20 ms). Guides advise against bare Ctrl, Alt or Shift because recording only a modifier is fragile. [C, secondary; the official page returned 403] | A release delay, which is the mirror of a commit delay. | "Record Keybind" → press → "Stop Recording". [C, secondary] |
| **OBS** PTT | Any key, per-source PTT with a delay. Forum reports say modifier-only and Win-key PTT bind oddly: the Win key shows a blank label. [C, forum] | — | Click the field and press the keys. |
| **Win+H voice typing** | Fixed OS shortcut, not rebindable. Win+Alt+H moves focus to the voice-typing keyboard. Win+Ctrl+S toggles Voice Access in newer builds. [C, MS support + third-party] | — | — |
| **PowerToys** KBM | Shortcuts "must begin with a modifier" and end with an action key, at most 4 keys. "⊞ Win+L and Ctrl+Alt+Del cannot be remapped". The Fn key cannot be remapped; F13–F24 can. "Pause will only send a single key-down event." "AltGr behaves as (L)Ctrl + (R)Alt". [C] | The reserved pair, the Pause trap, the AltGr warning. | Press to capture. |

Takeaway: no serious app offers unvalidated free choice. The norm is **press-to-record plus validation**, with a hard reject for OS-owned or typing combinations and a soft warning for risky ones.

## 2. Windows 11 combinations to reject or warn about

Sources: Microsoft Support "Keyboard shortcuts in Windows" (Windows 11 tab, read 2026-11-03); PowerToys Keyboard Manager docs; IME notes. [C unless tagged]

**Reject (hard):**
- **Unremappable / secure attention:**
  - Win+L and Ctrl+Alt+Del. PowerToys cannot remap these, and WinRT documents them as SAS that the system always handles.
  - Ctrl+Shift+Esc (Task Manager).
- **Any Win+key**, meaning a letter, digit, arrow, Tab, Space, punctuation, PrtScn, Home, Esc or Pause. Windows owns this namespace and keeps growing it (Win+J Recall, Win+C Copilot, Win+Alt+K mic mute, Win+H dictation). Also Win+number (taskbar slots), plus Win+Shift/Ctrl/Alt/Ctrl+Shift with a number. Win **inside a modifier-only chord** is fine (Ctrl+Win).
- **Alt+Tab, Alt+F4, Alt+Esc, Alt+Space** (window menu; also the PowerToys Run default), Alt+PrtScn.
- **Typing keys without a non-Shift modifier:** letters, digits, punctuation, Space, Enter, Tab, Backspace, Delete, arrows, Home/End, and the same with Shift.
- **Common edit chords:** Ctrl+C/V/X/Z/Y/A/S/F/P/N/T/W/O/R/B/I/U and Ctrl+Backspace/Delete/Home/End. This is Wispr's list.
- **Anything containing Esc.** Esc is Voisu's Cancel.
- **Bare Ctrl** (left or either side), **bare Shift** (either side), **bare Win**, **bare left Alt**:
  - Bare Shift: every capital letter would start and then interrupt a Recording.
  - Bare Win: a Win tap must still open Start.
  - Bare left Alt: menu bar, and the prefix of every Alt shortcut.
  - Bare Ctrl: Wispr reserves it. It is the prefix of every Ctrl shortcut.
- **Pause/Break**: it sends no key-up (PowerToys: "a single key-down event"), so it cannot hold to talk. [C] Allow it only if a tap-toggle-only mode exists. v1 should reject it.
- **The Copilot key**: it arrives as LShift+LWin+F23 (PowerToys, Deskthority). It is not a held modifier, and its hold/release sequence is [U]. Defer it; Windows also has its own "Customize Copilot key" setting.

**Allow with a warning (soft):**
- **Ctrl+Alt and Right Alt alone:**
  - On AltGr layouts (German, Polish, French, Nordic and others), AltGr = LCtrl(synthetic)+RAlt.
  - The hook already filters the synthetic LCtrl (`hotkey.rs` `ALTGR_SCAN_BIT`). [C]
  - **A RAlt trigger on an AltGr layout** would start and then interrupt a Recording on every AltGr character (@, €, ą). [I]
  - Hide or warn about RAlt when the active layout has AltGr. Detection via `VkKeyScanEx` on a known AltGr character, or the layout flags, is [U].
- **Ctrl+Shift** and **Alt+Shift**: these switch the keyboard layout or input language when more than one is installed. MS lists Ctrl+Shift. Alt+Shift is a long-standing default and [U] on Win11. Warn when more than one layout is installed. The mask key in §3 should stop the switch on release. [I/U]
- **Ctrl+Space**: it toggles the Chinese IME (MS support), and it is the IDE completion shortcut. Warn.
- **Ctrl+Shift+Space**: the common fallback, but also 1Password Quick Access and other app shortcuts. Warn once and allow.
- **Ctrl+Win, Win+Shift, Win+Alt, Ctrl+Win+Alt chords**: the virtual-desktop flicker in §5 applies to all of them. Once the commit delay exists, the flicker is cosmetic and needs no warning.
- **F1–F12 alone**: F1 opens help, F5 refreshes, F11 toggles fullscreen and F12 opens devtools. On laptops with Fn-lock off, F-keys are media keys. Reject F5 and F11 as Wispr does, and warn on the rest.
- **Ctrl+Alt+Shift+Win** (the "Office/Hyper" key): allowed, but Office keys send it as one combination. [C PowerToys]
- **Shift tapped 5 times** (Sticky Keys) and **right Shift held 8 s** (Filter Keys) are accessibility shortcuts. Bare Shift is already rejected. Shift inside a chord with double-tap is 2 presses, well under 5. [I]

## 3. Hook mechanics for the new shapes

**Alt needs masking, as Win does.** [C AHK]
- A bare Alt down/up activates the window's menu bar.
- AutoHotkey masks the next Alt or Win key-up with an unassigned key (`vkE8`, the same key Voisu uses).
- AHK conditions: "Alt is not masked if Ctrl was down when the hotkey was pressed, since Ctrl+Alt does not activate the menu bar." "Win is not masked if the most recent Win keydown was modified with Ctrl, Shift or Alt."
- AHK also warns: "Microsoft can assign an effect to an unassigned key code at any time" (vk07 became Game Bar in 1909).
- **Rule:** when a chord forms with Alt or Win held, inject the mask once. Today's unconditional mask on Ctrl+Win is harmless and simpler than AHK's conditions. Keep it, and extend it to Alt.

**Swallowing a non-modifier key that belongs to the chord (Space, Caps Lock, F13–F24) is safe if:**
1. **Down, every auto-repeat down, and up are swallowed together.** Track "swallowed" per key, as `escape_swallowed` already does. A stray up of a non-modifier is harmless. A stray up of a modifier risks a stuck key, which is why the modifiers keep passing through. [C current design, I]
2. **The mask is injected while the modifiers are still down.**
   - Swallowed events never reach the system's hotkey processing, so Ctrl+Shift with a swallowed Space looks like a bare Ctrl+Shift tap (layout switch), and Win+swallowed key looks like a bare Win tap (Start).
   - This is why AHK masks whenever a suppressed hotkey has Alt or Win down. [C AHK, I for Ctrl+Shift → layout switch, **U: test on real host**]
3. **Caps Lock:** swallowing its down prevents the Caps toggle, which is the desired effect. Offer Shift+Caps Lock as a pass-through so users can still toggle caps. Raja's own Linux trigger is Caps Lock (`~/.config/hypr/hyprland.lua`), so it is a natural preset. [C STATE.md, I]
4. **Modifiers keep passing.** Apps see Ctrl+Shift down then up with nothing in between. Most ignore that. **PowerToys Find My Mouse** defaults to a double tap of Left Ctrl, so a double-tap hands-free gesture on a Ctrl chord could trigger it. [I/U]

**Key repeat:** holding any key repeats key-downs. The tracker already ignores modifier repeats through `SideSet`. A swallowed trigger key needs the same guard, or each repeat becomes a new press, and a double tap could fire from one long hold. [C chord.rs, I]

**Sticky Keys:** users who need it often cannot hold two keys. Whether the LL hook sees the physical events or the latched state is [U]. Mitigation either way:
- Offer single-key presets (Right Ctrl, Caps Lock, F13+).
- Keep double-tap hands-free, which needs no hold.

**`LLKHF_INJECTED` filter:**
- `on_key` drops **all** injected events. [C hotkey.rs]
- PowerToys remaps and AHK remaps arrive injected. A user who maps the Copilot key → F24 with PowerToys (the common workaround in Windows Latest) would see Voisu ignore F24. [I]
- The tag is already in `dwExtraInfo` (`MASK_TAG`). **Option:** drop only Voisu-tagged events, and accept injected events for the *non-modifier trigger key only*. Keep dropping injected modifiers, so a password manager's typing never forms a chord.

**Left/right:**
- Expose sides **only** for single-modifier triggers: Right Ctrl and Right Alt. These are the useful ones because they are rarely used.
- Multi-modifier chords stay side-agnostic ("Ctrl" = either side), which is today's `SideSet.any()` behaviour.
- Reject "LCtrl+RCtrl", as Wispr does.

**Recorder capture:**
- WebView2 key events will not reliably see Win, Win chords or Caps Lock, and the live hook would start a Recording while the user records a key.
- Put the hook into a **capture mode** while the recorder is open: report raw transitions to the UI, swallow nothing except Esc (Cancel), and form no chord. [I]
- Save on release, as Wispr does.

## 4. Recommendation

### Allowed shapes (canonical `hotkey` string → parsed `Trigger`)
| Shape | Examples | Notes |
|---|---|---|
| A. **2–3 distinct modifiers**, side-agnostic | `Ctrl+Win` (default), `Ctrl+Alt+Win`, `Win+Alt`, `Ctrl+Shift` (warn) | No key swallowed. Mask whenever Alt or Win is down. |
| B. **One right-side modifier** | `RCtrl`, `RAlt` (warn or hide on AltGr layouts) | Mask RAlt (Alt menu). RCtrl needs no mask. |
| C. **One dedicated key** | `CapsLock`, `F13`–`F24`, `F1`–`F12` minus F5/F11 (warn), `Apps`/Menu key, `ScrollLock` | Key swallowed (down + repeats + up). |
| D. **1–2 modifiers + one key**, with at least one non-Shift modifier | `Ctrl+Shift+Space` (warn), `Ctrl+Alt+Space` (warn on AltGr), `Alt+Shift+F12` | Key swallowed. Mask injected. Win+key is rejected. |

At most 3 keys in total. One binding in v1, because multiple alternates are YAGNI. An unparseable or now-rejected stored value falls back to `Ctrl+Win` with a log line.

### Rejection rules (in order, first hit wins, with the reason shown in the UI)
1. Contains Esc → "Esc cancels a Recording".
2. Contains Pause or the Copilot key → "This key can't be held".
3. Win plus any non-modifier key → "Windows uses Win+key shortcuts".
4. In the reserved list: Win+L, Ctrl+Alt+Del, Ctrl+Shift+Esc, Alt+Tab, Alt+F4, Alt+Esc, Alt+Space, Alt+PrtScn, Ctrl+edit letters, F5, F11.
5. Bare Ctrl, LCtrl, Shift (either side), Win, or LAlt.
6. A typing key without a non-Shift modifier.
7. Both sides of the same modifier.

**Warnings** (savable): RAlt or Ctrl+Alt on an AltGr layout; Ctrl+Shift or Alt+Shift with multiple layouts installed; Ctrl+Space; Ctrl+Shift+Space; bare F1–F12.

### Hands-free tap-toggle
- **Keep it for every hold-capable shape.**
- For shapes A and B it works exactly as today.
- For C and D, drive the gesture from the **non-modifier key's** press and release. "Hold Ctrl+Shift, tap Space twice" is a natural double tap, and releasing the modifiers mid-Recording should not stop it. [I]
- The modifiers going up end the chord only when the key itself is still held.
- Esc and Interrupted rules are unchanged: for D, any *other* key pressed while the chord is held interrupts.

### Presets for the UI
1. **Ctrl + Win** (default)
2. **Right Ctrl**: no AltGr risk, no menu.
3. **Caps Lock**: Shift+Caps Lock still toggles caps. Raja's own Linux binding.
4. **Right Alt**: shown only when the layout has no AltGr.
5. **Ctrl + Shift + Space**
6. **Custom…**: the recorder plus the validation above.

F13–F24 are reachable only through macro keys or a remap, so they belong under Custom rather than as presets.

## 5. Is Ctrl+Win's real problem the false start? Yes. The cheaper fix is a commit delay.

**What happens today on Ctrl+Win+Left** [C source]:
- `chord.rs`: Pressed fires when Win goes down, then the mask is injected.
- `gesture.rs`: `Start(id)` is sent immediately ("A Recording starts on the first press so no speech is clipped").
- `engine.rs` `record()`:
  - `emit_state(Recording)`: the overlay appears.
  - The cpal mic opens: the Windows mic-in-use indicator appears. [I]
  - `start_providers`: the Deepgram WS and the Groq stream open.
- Left arrives → `Interrupted` → `Cancel`, then `cleanup.drain`.
- Left still reaches Windows, so the desktop switch works.

The user sees an overlay and mic flash on every desktop switch, and every switch opens a paid provider connection. That is what makes the binding feel like a clash.

**The fix:** add `COMMIT_DELAY` (start around 150–200 ms and tune it on the real host).
- On Pressed, start the **mic only**, into a local ring buffer.
- Defer the overlay and the provider streams until the chord has been held for `COMMIT_DELAY` without Interrupted or Escape.
- On commit, replay the buffer into the providers, so no speech is clipped.
- If Interrupted or Escape arrives first, drop silently: no overlay, no network.
- The gesture timing (`MIN_HOLD` 300 ms, `DOUBLE_TAP` 400 ms) is unchanged. The engine gains a `Pending` phase.
- To also hide the mic indicator, defer the mic itself. The cost is up to `COMMIT_DELAY` of clipped onset. Measure before choosing.
- **Measure first:** log the Win-down → arrow-down gap for Ctrl+Win+arrow / D / F4 on Raja's Win11 machine. Typed shortcut gaps are usually well under 200 ms [I/U]. The delay must sit above the 95th percentile of that gap.

This fix makes Ctrl+Win livable for virtual-desktop users. It also benefits every Win or Alt chord a user might choose, so ship it **alongside** the picker rather than instead of it. Raja still wants user choice.

## Sources
- Wispr Flow, "Customize keyboard and mouse shortcuts": https://docs.wisprflow.ai/articles/2612050838-supported-unsupported-keyboard-hotkey-shortcuts
- SuperWhisper shortcuts: https://superwhisper.com/docs/get-started/settings-shortcuts ; Ctrl+Win request: https://superwhisper.userjot.com/board/p/allow-combinations-like-ctrl-win-for-push-to-talk-and-turning-recording-on-and-off
- Aqua Voice activation keys: https://aquavoice.com/guide/how-to-add-multiple-activation-keys
- Discord PTT (secondary; the official page returned 403): https://www.howtogeek.com/662101/how-to-enable-push-to-talk-in-discord/ , https://wisechecker.com/?p=5471
- OBS PTT forum: https://obsproject.com/forum/threads/use-windows-key-for-ptt.2707 , https://obsproject.com/forum/threads/ptt-hotkey-vs-modifiers.165102/
- Microsoft, Keyboard shortcuts in Windows: https://support.microsoft.com/en-us/help/12445/windows-keyboard-shortcuts
- PowerToys Keyboard Manager: https://learn.microsoft.com/en-us/windows/powertoys/keyboard-manager
- WinRT KeyboardDeliveryInterceptor (SAS keys): https://learn.microsoft.com/ko-kr/uwp/api/windows.ui.input.keyboarddeliveryinterceptor?view=winrt-15063
- AutoHotkey A_MenuMaskKey: https://www.autohotkey.com/docs/v2/lib/A_MenuMaskKey.htm
- KBDLLHOOKSTRUCT: https://learn.microsoft.com/en-us/windows/win32/api/winuser/ns-winuser-kbdllhookstruct ; LowLevelKeyboardProc: https://learn.microsoft.com/en-us/windows/win32/winmsg/lowlevelkeyboardproc
- Copilot key = LShift+LWin+F23: https://deskthority.net/wiki/Copilot_key , https://www.windowslatest.com/2026/10/02/i-turned-windows-11s-useless-copilot-key-into-a-screenshot-button-heres-how/
- Ctrl+Space IME / Ctrl+Shift+Space conflicts: https://public.dhe.ibm.com/software/rational/web/hod/en/help/dbcs_zhx.html , https://www.1password.community/1password-at-home-31/conflicts-of-global-shortcuts-11185
- Voisu source: `apps/windows/src-tauri/src/{chord.rs,gesture.rs,trigger.rs,engine.rs,platform/hotkey.rs,settings.rs}`; `internal/specs/2026-11-02-windows-cloud-port.md`; `internal/scratch/voisu-windows/research/{01,03,04}`.

## Real-host checks this note depends on (unrun)
1. The Win-down → key-down gap for the Ctrl+Win virtual-desktop shortcuts (this sets `COMMIT_DELAY`).
2. Whether Ctrl+Shift with a swallowed Space switches the keyboard layout without a mask, and whether `vkE8` prevents it.
3. Whether RAlt alone opens the menu bar without a mask, and whether the mask fixes it.
4. Whether the hook sees physical or latched events under Sticky Keys.
5. Whether PowerToys-remapped F24 arrives with `LLKHF_INJECTED`.
6. The Copilot key's down/up sequence while held.
