# Live audio device switching: manual test checklist

Real devices, the OS default and hot-plugging cannot be unit-tested. Run this
list by hand on each platform and paste the results into the PR's
"## Testing".

## Prerequisites

- A build of this branch (`--features desktop`, plus `--features aec` for the
  Linux AEC check) and a working API key, so a conversation can run.
- Two output devices (built-in speakers and a headset) and two input devices
  (built-in mic and the headset mic). A Bluetooth or USB headset that can be
  switched off and on is ideal.
- Know where the OS default output and input are changed (macOS: System
  Settings > Sound; Windows: Sound settings; Ubuntu: Settings > Sound).
- Settings > Audio is where devices are picked. "Default" means follow the OS
  default; a named device is pinned.

## All platforms (repeat on macOS, Windows and Ubuntu)

- [ ] Switch output from speakers to a headset in Settings mid-conversation;
      after Settings closes, the assistant is heard on the headset.
- [ ] Mixed pair: speaker output with headset microphone input.
- [ ] With "Default", change the OS default during a reply: the audio moves
      within about 2 s and the conversation stays connected.
- [ ] With a pinned headset, change the OS default: nothing moves.
- [ ] Turn the pinned headset off mid-conversation: UIA waits, does not
      switch, and the HUD names the device. Turn it back on: audio resumes and
      the notice clears.
- [ ] Toggle echo cancellation in Settings: it applies with no restart and no
      echo (not on Ubuntu, where the toggle is disabled).
- [ ] A pinned device missing at launch, and one missing when Settings closes:
      UIA waits, the other direction works, and it resumes when the device is
      plugged in.
- [ ] Settings opens while a pinned mic is dead (the session parks), and
      switching to Default works.
- [ ] No restart notice on the Audio tab; Providers, MCP and General still
      show one.
- [ ] Quitting while a device is being waited for exits at once.
- [ ] A "Default" device that disappears without the stream reporting an
      error, and a pinned device unplugged with no error: UIA must show it is
      waiting, not look healthy (observe; report if it doesn't).
- [ ] Barge-in still feels immediate while the assistant is speaking a long
      reply (capture now runs in its own task; listen for delayed
      interruption).
- [ ] Save a new device, then move only the barge-in slider and Save again
      before closing Settings: the new device is still applied on close.

## macOS

- [ ] With a pinned mic missing, output plays through the default speaker;
      plugging the mic in brings back echo cancellation (no echo).
- [ ] The Audio tab shows "Saved." with no restart notice; Providers still
      shows one; the echo-cancellation toggle is enabled.

## Windows

- [ ] The "Default" hint is shown in Settings > Audio.
- [ ] "Default" follows the default communication device, with echo
      cancellation both on and off.

## Ubuntu

- [ ] The echo-cancellation toggle is disabled with its note.
- [ ] A name saved before the PipeWire host may not match PipeWire's device
      names: re-pick the device.
- [ ] Linux developer builds with `--features aec` only: a pair opened while a
      device was waiting runs without AEC3 until the next Save or restart
      (expected, not a bug).

## Before merging

- [ ] Aikido scan clean.
- [ ] The live voice check the webrtc pins require.
- [ ] This checklist run on a real Mac, Windows PC and Ubuntu.
- [ ] CI green, including the `test-windows`, `test-ubuntu`, `desktop-build`
      and `aec-feature` jobs, which have never run on this branch.
