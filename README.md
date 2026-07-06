# inkling

Draw a rough sketch on a [reMarkable 2](https://remarkable.com), and a moment after
you lift the pen it's replaced — on the same page — with a polished line illustration
of what you drew.

Scribble a bike; get a clean pen-and-ink bike. The rough idea goes in, an *inkling* of
the finished thing comes back.

> **Status: proof-of-concept / hobby project.** It runs on a real device and does the
> full loop, but it's rough around the edges and targets one specific tablet + OS. Not
> affiliated with reMarkable.

## How it works

Three pieces:

- **`inkling`** — a small Rust daemon that runs on the tablet. It watches the pen for
  when you've *finished* a sketch (a short quiet period after you stop drawing and lift
  the pen), grabs the current page, sends it to an image model, clears the page, and
  draws the result back as real pen strokes.
- **`inklingfb`** — a tiny [xovi](https://github.com/asivery/xovi) extension that clears
  the current page cleanly by driving the tablet UI's own scene, so the device repaints
  the e-ink panel itself. This replaces the crude "erase everything with the eraser
  tool" approach and is instant and undoable.
- **`inkling-core`** — the pure, host-testable logic (the finished-sketch state machine,
  the raster→pen-stroke vectorizer, geometry/calibration). No device dependencies, unit
  tested.

The loop:

```
you draw ─▶ inkling detects you finished ─▶ capture the page
         ─▶ image model turns the sketch into a clean illustration
         ─▶ inklingfb clears the page ─▶ inkling redraws the result as pen strokes
```

If you pick the pen back up mid-cycle, the request is abandoned and the page is left
exactly as you drew it.

## Requirements

- A **reMarkable 2** with root SSH access (it ships with SSH; password is in the tablet's
  Settings → Help → About).
- **[xovi](https://github.com/asivery/xovi)** installed on the tablet (for the page-clear
  extension).
- An **[OpenRouter](https://openrouter.ai) API key** (the daemon calls an image model —
  default `google/gemini-2.5-flash-image`).
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
ssh root@<tablet-ip> systemctl start inkling     # or run ./inkling run directly
```

Now draw something and lift the pen.

## Configuration

`config.toml` — see [`config.example.toml`](config.example.toml):

```toml
[imagegen]
api_key = "sk-or-..."                 # your OpenRouter key
model   = "google/gemini-2.5-flash-image"

[watch]
dwell_s        = 3.0                   # quiet seconds after you stop before it fires
rate_limit_s   = 15.0                  # minimum gap between requests
min_new_ink_px = 400                   # ignore trivial marks

[ink]
draw_pps   = 800.0                     # pen-stroke injection speed (points/sec)
max_points = 6000                      # cap on injected points per illustration

[archive]
dir = "/home/root/.local/share/inkling"   # keeps sketch+result pairs

[control]
pause_file = "/tmp/inkling.pause"      # touch this to pause the daemon
```

## Privacy

The daemon sends an image of the current page to the OpenRouter API to generate the
illustration — so whatever is on the page at that moment leaves the device. Your API key
stays on the tablet (in the config file); nothing else is sent anywhere. It only acts on
the page you're actively drawing on.

## License

MIT — see [LICENSE](LICENSE).
