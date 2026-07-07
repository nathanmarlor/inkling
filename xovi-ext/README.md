# inklingfb — inkling's xovi bridge into xochitl

A small [xovi](https://github.com/asivery/xovi) extension that lets the `inkling`
daemon drive the reMarkable UI (xochitl): read the screen, read and delete the current
stroke selection, switch drawing tools, manage a scratch layer, and inject an **AI**
button into the selection menu. Everything is done through xochitl's own public Qt
entry points — **no function hooks**, which is what keeps it stable.

```
[inklingfb] loaded (clear + selection probe)
[inklingfb] selinfo count=228 rect=[334.0 527.0 865.0 1092.0] portrait=0
[inklingfb] convert button attached beside menu (w=324)
[inklingfb] spinlayer begin (scratch=1, prev=0)
[inklingfb] spinlayer end (deleted 1, restored 0)
```

## Architecture (each rule paid for with a device freeze or crash)

- **No function hooks.** xovi's arm32 trampoline races on hot, multithreaded paths.
  Everything is on-demand visual-tree traversal + Qt meta-method calls instead.
- **All Qt access is on the GUI thread**, via `gui_process` (posted with
  `QMetaObject::invokeMethodImpl` + `QCoreApplication::self`). The worker thread only
  watches trigger files and posts the job. The **one** exception is `grab_screen`
  (`QQuickWindow::grabWindow`), which is panel-safe *only* from the worker thread —
  called from the GUI thread it wedged the e-ink refresh pipeline.
- **Never parent an item into the selection menu.** It's a Qt Quick `Container`, which
  hoovers any added visual child into its content model; teardown over that foreign
  child then crashes xochitl. The AI button is parented to the menu's parent (the
  `SceneSelectionHandler`) as a sibling instead.
- **Never delete strokes via a programmatic selection.** Emit the menu's own
  `deleteSelection()` signal (the trash button's path) instead.

## File-trigger contract

Each trigger file is deleted once handled; the daemon treats the deletion as the ack.

```sh
touch /tmp/inklingfb_clear        # clear the whole page (ink + text), undoable
touch /tmp/inkling_grab           # write the live frame to /tmp/inkling_frame
                                  #   (20-byte header w,h,bpl,fmt,nbytes int32 LE,
                                  #    then raw RGB32 pixels — 1404x1872)
touch /tmp/inkling_selinfo        # selection info -> /tmp/inkling_selinfo_out:
                                  #   "count x y w h portrait" — stroke count, tight
                                  #   view-px bbox (SceneSelectionHandler.viewSelection-
                                  #   Rect), and DocumentView.portrait (1=portrait)
touch /tmp/inkling_seldelete      # NATIVE delete of the current selection: emits the
                                  #   menu's deleteSelection() signal (trash-button
                                  #   path — correct edit id, clean refresh, undoable)
echo pen|sel > /tmp/inkling_tool  # native tool switch: DocumentView.penHandler
                                  #   {lineTool,gestureMode,lineThickness}
                                  #   (pen 15/1/1.0, selection 11/6/2.0)
echo begin|end > /tmp/inkling_spinlayer   # add / delete a scratch layer (for the
                                  #   loading spinner, so removal is one deleteLayer)
# dev tooling only:
touch /tmp/inkling_probe          # introspection dump -> /tmp/inkling_probe_out
echo "[view] x y w h [mode]" > /tmp/inkling_selrect   # programmatic selection
                                  #   (SceneController::addSelectionRect); "view"
                                  #   prefix = screen px, else scene coords
```

Plus the **AI button**: injected beside the selection menu whenever a menu without one
exists. Tapping it fires `GET http://127.0.0.1:9137/convert`, which the daemon's
listener (`spawn_convert_listener`) turns into a convert. It's a `QQmlComponent`-built
`Rectangle` with an `"AI"` label (plain text — the device fonts lack ★, which renders
as tofu) and a `TapHandler` with `gesturePolicy: ReleaseWithinBounds` (a `MouseArea`
never fires — xochitl delivers raw touch with no mouse synthesis; a bare `TapHandler`
lets the tap fall through and dismisses the selection). Dedup is a dynamic C-side
marker property (`inklingMark`) on the button; the component is built once and kept
for the process lifetime (destroying it breaks property resolution on its objects).

## Selection findings (hard-won — read before changing the selection code)

- **The stroke selection lives on `SceneController`** (`selectionItemCount`,
  `deleteSelectedItems(int)`, `clearSelectedItems()`, `addSelectionRect(QRect,mode)`).
  `DocumentView.pageSelection` (class `PageSelection`) is the PAGE organiser's
  selection — unrelated to strokes.
- **`clearSelectedItems()` = deselect**, not delete. **`deleteSelectedItems(int
  editId)`** needs the selection's internal edit-transaction id; wrong ids are silent
  no-ops (a 0..64 sweep found nothing). So neither is used — the clean delete is the
  **`deleteSelection()` signal on the QML `SceneSelectionHandler`** (menu's parent):
  invoking a signal by name emits it, running the trash button's own path. Siblings:
  `cut()`, `copy()`, `convert(QVariant)` (convert-to-text).
- **`SceneSelectionHandler.viewSelectionRect`** = tight bbox in VIEW px;
  `sceneSelectionRect` = same in scene coords. Scene coords are CENTERED and move with
  the viewport, so the extension learns the live offset from
  `sceneSelectionRect − viewSelectionRect` at each selinfo — never hardcode it.
- **`addSelectionRect` only works while the selection tool is active.**
  `LineSelectionMode { InitalSelection=0, ToggleSelection=1 }` (sic); QRect is
  `{x1,y1,x2,y2}`, not w/h.
- **arm32 hard-float ABI trap: `QRectF`/`QPointF`/`QSizeF` are homogeneous double
  aggregates returned in VFP registers d0–d3, NOT via sret.** Model them as C structs
  of doubles returned by value. `QVariant::constData()/typeName()` are inline in Qt 6
  (dlsym → null) — use the exported `toBool/toInt/toDouble/toRectF/...` converters.
- **Only read simple value-type / QObject* properties off the tree.** Reading a
  `QQuickAnchorLine` property crashed xochitl; `dump_qobject` whitelists safe types.

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
`LD_PRELOAD` (Qt symbols are resolved at runtime via `dlsym`).

## Install (revertible)

```sh
# one-time: xovi core installed at /home/root/xovi/
scp inklingfb.so root@<tablet-ip>:/home/root/xovi/extensions.d/

# systemd drop-in so the UI loads xovi (tmpfs — re-apply after a reboot):
#   [Service]
#   Environment="LD_PRELOAD=/home/root/xovi/xovi.so"
#   Environment="XOVI_ROOT=/home/root/xovi"
systemctl restart xochitl        # xovi auto-loads inklingfb
```

To revert, remove the drop-in (or stop xovi) and restart — back to the stock UI.
The included `../deploy.sh` automates the copy + drop-in + restart.

> After a device reboot the extension is present but xochitl does not auto-load it on
> the cold boot — run `systemctl restart xochitl` once, then `systemctl start inkling`.
