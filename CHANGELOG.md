## v0.3.10

- tests: fix send_packet call for load_serial arg
- player-core: live video_hw flag on HW->SW fallback, SW drain debug log
- player-core: hw stall watchdog 60 -> 10 packets, keep SW state on fallback
- Revert "ui: move stats line into top chrome bar below title row"
- ui: move stats line into top chrome bar below title row (save)
- player-core: feed fallback keyframe to fresh SW decoder; ui: single-line stats text
- ui: playback stats overlay (i key + info button)
- player-core: fall back to track duration when num_frames unknown
- player-core: patch videoson h264 with IDR streaming fix
- player-core: log SW send failures with packet shape
- player-core: remove stall-debug logging
- player-core: log SW drain receive count
- player-core: log SW send failures with packet shape
- player-core: warn when stall fallback unavailable
- player-core: HW stall watchdog with SW fallback
- hw: fix misleading Annex-B prefix comment
- hw: separate Annex-B start code before prefixed config
- wasm hw: decode on calling thread like Miniter, drop supervisor/pump


## v0.3.9

- snap fix


## v0.3.8

- No user-facing changes were mentioned since previous release


## v0.3.7

- No user-facing changes were mentioned since previous release


## v0.3.6

- No user-facing changes were mentioned since previous release


## v0.3.5

- Revise README for clarity and demo note


## v0.3.4

- No user-facing changes were mentioned since previous release


## v0.3.3

- fix: drop audio/midi from advertised types
- fix(test): portable missing-file assertion for Windows
- fix(test): self-contained playlist test (synth wav, no local path)
- feat: expand M3U/M3U8/PLS playlists into queue entries
- ci(aur): take .desktop from flathub file
- feat: mpv-parity mime types + Video category
- fix: setup-android packages '' (legacy tools pkg removed)


## v0.3.2

- No user-facing changes were mentioned since previous release


## v0.3.1

- Update AUR deploy action to use main branch
- use videoson from crates
- baa4ba fix test
- use ropfs
- save test
- use the crates version of baaba


## v0.3.0

- fix: gate hw deps for wasm, add linux vaapi deps
- fix: gate hw deps for wasm, add linux vaapi deps
- hw crate test
- add vp8/9


## v0.2.3

- cargo update


# Changelog

## v0.2.2

- bump repose
- fix: later bump to 0.28.11 to fix icon color issue
- chec
- cargo bump vers
- use web-workers 0.3
- theme
- fix wasm err and fmt
- update repose version
- partial qol stuff
- screenshots

