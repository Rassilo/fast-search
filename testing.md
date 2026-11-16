# Testing strategy

Fast search has three kinds of tests. Each one covers what the others
cannot, and each runs in a different place.

| Kind | Command | Checks | Needs | Runs in CI |
|---|---|---|---|---|
| Unit tests | `cargo test` | The logic: searching, the index file, parsing | Nothing | On a release |
| Filesystem tests | `sudo tests/filesystems.sh` | The daemon following real file changes | Root | No |
| Drawing tests | `tests/gui-smoke.sh` | The window starts and draws on a display server | A display, or a headless one | Every push and pull request |

## Why three kinds

The program has three parts that fail in different ways:

- **The search engine and index** are pure logic. A unit test can feed
  them input and compare the output, quickly and on any machine.
- **The daemon, `ffd`,** depends on the kernel: mounts, fanotify events
  and the quirks of each filesystem. Only real filesystems show whether
  it keeps up, and mounting them needs root.
- **The window, `ff-gui`,** speaks the X11 and Wayland protocols itself,
  with no toolkit in between. A mistake in a protocol message does not
  show in a unit test; it shows when a real display server refuses the
  window. Compositors also differ from each other, so one is not enough.

## Unit tests

```sh
cargo test
```

37 tests and one documentation example, no root, a few seconds.

- **ff-core (22 tests):** the query engine (terms, wildcards, regular
  expressions, filters, Unicode case), time zones, the index file
  format, the C API, patching a loaded index, deleting files, and the
  readers that index ext2/ext4 and NTFS drives directly.
- **ff-daemon (8 tests):** decoding kernel events, the mount table and
  drive settings.
- **ff-gui (7 tests):** folder totals and the tree, the treemap layout,
  the XKB keymap parser, global shortcut parsing, drive settings, and
  that every translation keeps its `{}` placeholders.

The drive-reader tests build small disk images with `mkfs.ext4`,
`mkfs.ntfs` and `ntfscp`. Where those tools are not installed, these
tests skip themselves and still pass.

## Filesystem tests

```sh
sudo tests/filesystems.sh                 # every filesystem available
sudo tests/filesystems.sh ext4 btrfs      # only these
```

For each filesystem the machine can create (tmpfs, ext4, xfs, btrfs, a
btrfs subvolume, f2fs, vfat, exfat) the script:

1. creates a small disk image and mounts it,
2. starts a private `ffd` on it,
3. creates, renames, rewrites, moves and deletes files,
4. checks after each step that the published index followed.

Two more cases: `hotplug` mounts and unmounts a drive while the daemon
runs, and `raw` indexes an unmounted ext4 drive directly.

It works in a temporary folder under `/var/tmp` and leaves the system's
own `ffd` service alone. It is not in CI because it needs root, loop
devices and kernel filesystem drivers that hosted runners do not
reliably offer. Run it by hand before a release and after any change to
`ffd` or to the live index.

## Drawing tests

```sh
tests/gui-smoke.sh wayland        # on your own Wayland session
tests/gui-smoke.sh x11            # on your own X server
```

The script opens the window once per view: results, the two menus,
About, Properties, four Options pages, Drives, Help, folder sizes, the completion list, the Bookmarks menu and the bookmark
and export dialogs.
For each view the window must:

- connect with the expected backend (X11 or Wayland),
- draw itself and save that picture,
- exit with status 0 within 30 seconds.

The picture must not be blank, and must differ from the plain results
view (except the menus and the regex toggle, see the limits below).

The window needs an index to show, so the script first builds a small
one from this repository's `crates` folder, in a temporary directory.
That is preparation, not a test. Settings come from an empty temporary
folder and the scale is fixed at 1, so the result does not depend on
who runs it.

### Without a screen

The script can start a display server that needs no screen, keyboard or
graphics card:

```sh
xvfb-run -a tests/gui-smoke.sh x11
tests/gui-smoke.sh wayland sway      # also: labwc, weston, mutter
```

`KEEP=1` keeps the pictures (PPM files) and the logs, and prints their
folder.

### What it catches, and what it does not

It catches a window that cannot start on a display server, sends a
message the server rejects, crashes while drawing, or draws nothing.

It does not catch:

- **Input bugs.** No keys or clicks are sent, so typing, key repeat,
  scrolling, drag and drop, the clipboard and resizing are untested.
- **Wrong pictures.** A view that draws the wrong thing passes, as long
  as it draws something. Pictures are not compared with reference
  images, because fonts differ between machines.
- **Menus on Wayland.** The compositor gives a new window its size just
  after it opens, and that closes an open menu. The menu views are
  therefore only checked for starting and drawing.

Input is tested by hand on a real desktop.

## Continuous integration

`.github/workflows/ci.yml` runs on every push to `master` and on every
pull request. It runs the drawing tests only:

1. **Build:** `ff` and `ff-gui` are compiled once. They are static, so
   the same binaries run in every job that follows.
2. **Draw:** one job per display server.

| Display server | Family |
|---|---|
| X11 (Xvfb) | X server |
| Sway | wlroots |
| labwc | wlroots |
| Weston | reference compositor |
| Mutter | GNOME |

All five must pass. If a job fails, its pictures and logs are attached
to the run.

The same jobs can be run locally in an `ubuntu:24.04` container, which
is the system the runners use: install the job's packages, copy in the
two binaries, `tests/gui-smoke.sh` and the `crates` folder, and run the
job's command as a normal user (Sway refuses to run as root).

Not covered: KWin (KDE) and Hyprland. Hyprland is not packaged for the
runners' Ubuntu version.

`.github/workflows/release.yml` runs when a version tag is pushed. It
runs `cargo test`, builds the packages, installs the `.deb` and runs
what it installed.

## Before a release

1. `cargo test`
2. `sudo tests/filesystems.sh`
3. `tests/gui-smoke.sh wayland` and `tests/gui-smoke.sh x11`
4. By hand in the window: type a search, scroll, open a result, drag
   one into a file manager, copy and paste, resize, open folder sizes.
5. Check that CI is green on `master`, then push the tag.
