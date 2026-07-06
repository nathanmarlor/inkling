# inklingfb — page-clear xovi extension

A small [xovi](https://github.com/asivery/xovi) extension that clears the current
reMarkable notebook page cleanly and instantly, then lets the tablet UI repaint the
e-ink panel itself. `inkling`'s daemon uses it to wipe the sketch before drawing the
generated illustration back on the page.

```
[inklingfb] loaded (page-clear via SceneController)
[inklingfb] cleared page (1 controller(s), 3 view(s))
```

## What it does

On `touch /tmp/inklingfb_clear`, a worker thread finds the active page's scene controller
in the running UI and asks it to clear the page's ink and text, then refreshes the view.
The tablet performs the erase and the panel refresh itself — so there's no framebuffer
poking, no crashes, and no restart needed to use it. The clear is undoable (the on-screen
undo button restores the page).

Everything is done through the UI's own (public Qt) entry points; the extension installs
**no function hooks**, which keeps it stable.

## Contract

```sh
touch /tmp/inklingfb_clear      # inklingfb picks it up within ~120ms and deletes it
```

That's the whole interface between the daemon and the extension.

## Build

Toolchain: `armv7-unknown-linux-gnueabihf-gcc` (e.g. Homebrew). Needs the xovi repo
(`git clone https://github.com/asivery/xovi`) for `util/xovigen.py`.

```sh
python3 xovi/util/xovigen.py -o xovi.c -H xovi.h inklingfb.xovi
CC=armv7-unknown-linux-gnueabihf-gcc
$CC -std=gnu11 -D_GNU_SOURCE -fPIC -c main.c -o main.o
$CC -std=gnu11 -D_GNU_SOURCE -fPIC -c xovi.c  -o xovi.o
$CC -shared -o inklingfb.so main.o xovi.o -lpthread
```

`inklingfb.xovi` declares no imports/overrides, so xovi auto-loads it with plain
`LD_PRELOAD`.

## Install (revertible)

```sh
# one-time: xovi core installed at /home/root/xovi/
scp inklingfb.so root@<tablet-ip>:/home/root/xovi/extensions.d/

# systemd drop-in so the UI loads xovi (tmpfs — re-apply after a reboot):
#   [Service]
#   Environment="LD_PRELOAD=/home/root/xovi/xovi.so"
#   Environment="XOVI_ROOT=/home/root/xovi"
systemctl restart xochitl        # xovi auto-loads inklingfb
touch /tmp/inklingfb_clear        # clears the current page
```

To revert, remove the drop-in (or stop xovi) and restart — you're back to the stock UI.

The included `../deploy.sh` automates the copy + drop-in + restart.
