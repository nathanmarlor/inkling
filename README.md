# inkling

Draw something on a [reMarkable 2](https://remarkable.com), circle it, and tap a button
in the tablet's own menu — a moment later your scribble is replaced, on the same page,
with a polished line illustration of what you drew.

Scribble a bike; get a clean pen-and-ink bike. The rough idea goes in, an *inkling* of
the finished thing comes back. And if you write a **question** instead of drawing, it
reads your handwriting and writes the answer underneath in ink.

> **Status: proof-of-concept / hobby project.** It runs on a real device and does the
> full loop, but it's rough around the edges and targets one specific tablet + OS. Not
> affiliated with reMarkable.

## How it works

Three pieces:

- **`inkling`** — a small Rust daemon on the tablet. It reads the selected strokes,
  sends them to a model, and draws the result back as real pen strokes.
- **`inklingfb`** — a tiny [xovi](https://github.com/asivery/xovi) extension that runs
  inside the tablet UI (`xochitl`). It injects the **AI** button into xochitl's own
  selection menu, reads/deletes the selection, captures the screen, and manages a
  scratch layer for the loading spinner — all through xochitl's own Qt entry points.
- **`inkling-core`** — the pure, host-testable logic (the raster→pen-stroke vectorizer,
  geometry/calibration, the finished-sketch state machine for the legacy mode). No
  device dependencies, unit tested.

The flow (selection mode):

```
lasso some strokes ─▶ tap the AI button in the selection menu
   │
   ├─ a drawing?      ─▶ image model turns it into a clean illustration
   │                     ─▶ delete the sketch ─▶ draw the illustration in its place
   │
   └─ handwriting?    ─▶ a vision model reads and answers the question
                         ─▶ write the answer in ink below it (question kept)
```

The AI button decides which path to take by looking at the selection. A loading
hourglass shows on a temporary layer while it thinks, then it hands you back the pen.

There's also a legacy **inactivity** mode (`[mode] trigger = "inactivity"`) that turns
the whole page automatically a few seconds after you stop drawing — no button, no
selection. Selection mode is the one that's actively developed.

## Requirements

- A **reMarkable 2** with root SSH access (password in Settings → Help → About).
- **[xovi](https://github.com/asivery/xovi)** installed on the tablet.
- An **[OpenRouter](https://openrouter.ai) API key** — the daemon calls an image model
  (default `google/gemini-2.5-flash-image`) and, for questions, a vision model
  (`google/gemini-2.5-flash`).
- A cross toolchain on your build machine:
  - Rust with the `armv7-unknown-linux-musleabihf` target.
  - An `armv7-unknown-linux-gnueabihf-gcc` (e.g. via Homebrew) to build the extension.

## Build

**Daemon** (static musl binary for the tablet):

```sh
cd daemon
cargo build --release --target armv7-unknown-linux-musleabihf -p inkling
# → target/armv7-unknown-linux-musleabihf/release/inkling
cargo test -p inkling-core        # run the pure-logic tests on your host
```

**Extension** (see [`xovi-ext/README.md`](xovi-ext/README.md) for details):

```sh
cd xovi-ext/inklingfb
python3 xovi/util/xovigen.py -o xovi.c -H xovi.h inklingfb.xovi
CC=armv7-unknown-linux-gnueabihf-gcc
$CC -std=gnu11 -D_GNU_SOURCE -fPIC -c main.c -o main.o
$CC -std=gnu11 -D_GNU_SOURCE -fPIC -c xovi.c  -o xovi.o
$CC -shared -o inklingfb.so main.o xovi.o -lpthread
```

## Install & run

`xovi-ext/deploy.sh` copies both artifacts to the tablet, wires up the extension, and
restarts the UI:

```sh
RM2_HOST=<tablet-ip> RM2_PASS='<root-password>' ./xovi-ext/deploy.sh
```

Then create the config (see below) at `/home/root/.config/inkling/config.toml`, run a
one-time calibration, and start the daemon:

```sh
ssh root@<tablet-ip> ./inkling calibrate        # follow the on-screen taps
ssh root@<tablet-ip> systemctl start inkling
```

Now draw something, lasso it, and tap **AI**.

> After a tablet reboot the extension is present but xochitl doesn't auto-load it on the
> cold boot — run `systemctl restart xochitl` once, then `systemctl start inkling`.

## Configuration

`config.toml` — see [`config.example.toml`](config.example.toml):

```toml
[imagegen]
api_key = "sk-or-..."                 # your OpenRouter key
model   = "google/gemini-2.5-flash-image"

[mode]
trigger     = "selection"             # "selection" (AI button) | "inactivity" (auto)
orientation = "auto"                  # "portrait" (default) | "landscape" | "auto"

[ink]
draw_pps   = 4000.0                   # pen-stroke injection speed (points/sec)
max_points = 30000                    # cap on injected points

[archive]
dir = "/home/root/.local/share/inkling"   # keeps sketch + result pairs

[control]
pause_file = "/tmp/inkling.pause"     # touch this to pause the daemon
```

## Technical notes

Bits that might be useful if you're hacking on a reMarkable 2 (OS 3.x, Qt 6, `xochitl`).
None of this needs the device opened up or a special developer mode — just root SSH, and
the reMarkable hacking community's older guides are mostly out of date against current
firmware, so most of this was re-derived from the running device. See
[`xovi-ext/README.md`](xovi-ext/README.md) for the extension internals.

**Driving the UI.** `xochitl` is a closed, stripped Qt 6 binary with no API. The
extension gets inside it at runtime: it walks the live QtQuick **visual** tree
(`QQuickItem::childItems`) to the active page's objects and calls their own Qt
meta-methods by name (`QMetaObject::invokeMethod`) — clear, delete-selection, layers,
tool switch — all resolved via `dlsym` against the exported Qt symbols. It installs
**no function hooks** (xovi's arm32 trampoline races on hot paths and crashes). All Qt
access happens on the GUI thread; the worker thread only watches trigger files.

**Injecting pen strokes.** A hotplugged `uinput` device doesn't work — `xochitl` ignores
it. You have to write `input_event`s directly into the *real* digitizer node
(`/dev/input/event1`), Wacom-style. Screen→pen is a per-device affine transform, so
there's a one-time `calibrate` step that drops three marks and reads back where they
landed. The pen tool must be active or injected strokes are treated as lasso gestures.

**Reading the screen.** Capture asks the extension to render the live window with
`QQuickWindow::grabWindow()` and writes the frame to a file — portrait **1404×1872**,
RGB32. This is drift-free across restarts (an earlier `/proc/<pid>/mem` framebuffer read
worked but its address moved after every restart and occasionally fed the model garbage).
Do **not** grab while pen strokes are being injected — `grabWindow` contends with
xochitl's render and wedges the e-ink panel.

**The AI button.** Injected as a QML item *beside* the selection menu (parented to the
menu's parent), never *into* it — the menu is a Qt Quick `Container` that absorbs added
children into its content model, and a foreign child there crashes xochitl on teardown.
Tapping it fires a loopback XHR to the daemon.

**Clearing / deleting.** rM2 ink is binary black strokes with no per-pixel alpha, so you
can't "fade" it. Instead the extension drives xochitl's own clear/delete, which repaints
the e-ink itself (the rM2 has no hardware EPDC; the waveform lives in software inside
`xochitl`, so third-party framebuffer writes never refresh the panel). The clean
selection delete is emitting the selection menu's own `deleteSelection()` signal — the
trash button's path, with the correct internal edit id; a programmatic
`deleteSelectedItems`/rect-select delete crashes xochitl.

**Answer handwriting.** Question answers are drawn with a vendored **Hershey**
single-stroke plotter font (`daemon/inkling/src/futural.jhf`) — centre-line letters that
read as clean handwriting. Rasterising a normal font and skeleton-tracing it fragmented
the letters; drawing glyph outlines gave hollow "bubble" letters.

## Privacy

The daemon sends an image of the selected strokes to the OpenRouter API to generate the
illustration or answer — so whatever you convert leaves the device. Your API key stays on
the tablet (in the config file); nothing else is sent anywhere.

## License

MIT — see [LICENSE](LICENSE).
