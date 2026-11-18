# ClipForge

A fast, Medal-style clip manager and trimmer for [GPU Screen Recorder](https://git.dec05eba.com/gpu-screen-recorder/about/) on Linux. Written in Rust with GTK4, libadwaita and GStreamer.

## Features

- **Library**: card grid with thumbnails, favorites, search, sort, and auto-refresh when new clips appear.
- **Editor**: live preview, filmstrip timeline with draggable trim handles, keyboard shortcuts (Space, I, O, arrows), fast (copy) or precise (re-encode) export, all audio tracks kept.
- **Auto-clip**: saves a clip when you get a kill in **Counter-Strike 2** (via Valve's Game State Integration). Kill streaks become one clip, named like `🔥 3 Kills · Mirage · 20-31-02.mp4` or `👑 Ace · …`. Spectating is ignored.
- **Fullscreen for 4:3**: preview and export 4:3 gameplay stretched to 16:9, remove black bars, batch-convert a whole library, or do it automatically for new auto-clips (originals are kept in `Originals/`).
- **Replay buffer**: use an existing GPU Screen Recorder replay buffer, or let ClipForge run one.
- **Hotkey saves**: `clipforge --save 60` saves the last minute and flashes a border around the screen (Wayland layer-shell).

## Install (Arch Linux)

```sh
./install.sh
```

This installs dependencies with pacman (`gtk4 libadwaita gstreamer gst-plugin-gtk4 gtk4-layer-shell ffmpeg …`), builds a release binary to `~/.local/bin/clipforge`, and installs the desktop entry and icon.

## Usage

| Command | What it does |
| --- | --- |
| `clipforge` | Open the app |
| `clipforge --save [SECONDS]` | Save the replay buffer now (SECONDS: 10, 30, 60, 300, 600, 1800; default whole buffer). Bind this to a hotkey |
| `clipforge --batch-fullscreen` | Convert every 4:3 clip in the clips folder to 16:9 without opening a window |

### Auto-clip for CS2

1. Open **Auto-Clip Settings** (Ctrl+,), click **Install** on "CS2 integration", then restart CS2 once.
2. Turn on **Counter-Strike 2 kills**.
3. Make sure a replay buffer is running (or enable "Run the replay buffer with ClipForge").

ClipForge listens on `127.0.0.1:31777` for CS2's game-state updates. It only reads your own player's kill and round-kill counts and the map name.

### Settings

Stored in `~/.config/clipforge/config.json`. The clips folder defaults to `~/Videos` (or `$GSR_CLIPS_DIR`).

## Development

```sh
cargo test        # needs ffmpeg/ffprobe on PATH
cargo run --release
```

`legacy/` contains the original Python/GTK prototype.

## License

MIT
