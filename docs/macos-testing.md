# Testing Voisu on a Mac

Thanks for testing! Voisu turns your speech into text. Hold a key, talk, let go, and
the words appear wherever you were typing.

This test build works only on **Apple Silicon Macs** (M1, M2, M3, M4 or newer)
running **macOS 13 Ventura or later**. To check, open the Apple menu  → **About This
Mac**. The "Chip" line should say Apple M-something.

## What you need

- Two files from Raja: `install.sh` and `Voisu-macOS-arm64.dmg`. You might get
  `Voisu-macOS-arm64.app.tar.gz` instead of the `.dmg`; either one works.
- The Groq and Deepgram API keys Raja sends you.

Save both files in your **Downloads** folder.

## 1. Install

1. Open **Terminal**. Press ⌘ Space, type `Terminal`, press Return.
2. Copy this line, paste it into Terminal, and press Return:

   ```sh
   sh ~/Downloads/install.sh ~/Downloads/Voisu-macOS-arm64.dmg
   ```

   If you got the `.app.tar.gz` file, use that name at the end instead.

3. The script closes any running Voisu, puts Voisu in your Applications folder, and
   opens it. If it asks for your Mac password, type it (nothing shows as you type)
   and press Return.

Why a script? This test build isn't from the App Store, so macOS blocks it when you
double-click it. The script tells macOS you trust this one app.

To install a **newer build** later, run the same line again with the new file.

## 2. Set up Voisu

When Voisu opens, its setup window asks for the two API keys. Paste in the keys Raja
sent you.

## 3. Allow the three permissions

Voisu needs three permissions. macOS asks for some of them the first time Voisu needs
them. If it doesn't ask, set them yourself in **System Settings → Privacy & Security**:

| Permission | Where | Why Voisu needs it |
|---|---|---|
| **Microphone** | Privacy & Security → Microphone | to hear you |
| **Accessibility** | Privacy & Security → Accessibility | to type the text into other apps |
| **Input Monitoring** | Privacy & Security → Input Monitoring | to notice when you hold the trigger key |

For each permission, turn **Voisu** on. If Voisu isn't in the list, click **+**, pick
**Voisu** from Applications, and turn it on.

After you change Accessibility or Input Monitoring, **quit Voisu and open it again**
from Applications. macOS often doesn't apply those two until the app restarts.

## 4. How to use it

- **Trigger Key: Control + Option.** Hold both keys down, speak, then let go.
- When you let go, the text appears where your cursor was.

## 5. What to test

Try each of these and note what happens:

1. **Basic dictation.** Open TextEdit or Notes, click in the page, hold Control +
   Option, say a sentence, let go. Did the right text appear?
2. **Other apps.** Try Safari or Chrome (a search box or an email), Messages, Slack or
   Discord, and anything else you use every day.
3. **Short and long.** Try one word. Then try talking for 30 seconds or more.
4. **Punctuation and names.** Say a question, a list, or a name. Is the punctuation and
   spelling right?
5. **Quick presses.** Tap the keys briefly without speaking. Nothing odd should happen.
6. **Restart.** Quit Voisu and open it again. Then restart the Mac. Does Voisu still
   work without asking for permissions again?
7. **Update.** When Raja sends a newer build, install it with the script. Do your
   permissions still work, or did macOS ask again?

For anything that goes wrong, note **what you did**, **what you expected**, **what
happened instead**, and **roughly what time it was**. The time helps Raja find it in the
logs.

## 6. Send the logs

Voisu keeps logs **only on your Mac**. It never sends them anywhere on its own. They are
in this folder:

```
~/Library/Application Support/Voisu/logs
```

You can open that folder from Voisu's settings window with **Open logs folder**.

To send them, paste this into Terminal. It makes `voisu-logs.zip` on your Desktop:

```sh
ditto -c -k --keepParent ~/Library/Application\ Support/Voisu/logs ~/Desktop/voisu-logs.zip
```

Then send `voisu-logs.zip` to Raja on Discord along with your notes.

## If something goes wrong

- **"Voisu can't be opened" or "is damaged".** Run the install line from step 1
  again. Don't open the app straight from the `.dmg`.
- **Holding Control + Option does nothing.** Check that Voisu is on under Input
  Monitoring and Accessibility, then quit and reopen Voisu.
- **Text doesn't appear, but Voisu reacted.** Check Accessibility, then quit and
  reopen Voisu.
- **Nothing gets heard.** Check Microphone. Also check System Settings → Sound →
  Input to make sure the right microphone is selected.
- **To remove Voisu.** Quit it, then drag **Voisu** from Applications to the Trash. To
  remove its settings and logs too, delete `~/Library/Application Support/Voisu`.
