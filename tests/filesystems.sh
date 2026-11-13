#!/bin/sh
# End-to-end test of the ffd daemon on real filesystems.
#
# For each filesystem it creates a small disk image, mounts it, starts a
# private ffd on it, then creates, renames, rewrites, moves and deletes
# files and checks that the published index follows every step.
#
# Needs root (mounting, fanotify). Nothing outside a temporary folder in
# /var/tmp is touched, and the system's own ffd service is left alone.
#
#   tests/filesystems.sh                 test every filesystem available
#   tests/filesystems.sh ext4 btrfs      test only these
#
# Filesystems: tmpfs ext4 xfs btrfs btrfs-subvol f2fs vfat exfat
# (btrfs-subvol mounts a subvolume instead of the top level, the layout
# openSUSE and others use, which needs ffd's top-level fallback.)
set -u
PATH=/usr/sbin:/sbin:/usr/bin:/bin:$PATH

here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
bin="$here/../target/x86_64-unknown-linux-musl/release"
all="tmpfs ext4 xfs btrfs btrfs-subvol f2fs vfat exfat"

case "${1:-}" in
    -h|--help) sed -n '2,16p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
esac
if [ ! -x "$bin/ffd" ] || [ ! -x "$bin/ff" ]; then
    echo "filesystems: build first with 'cargo build --release'" >&2
    exit 1
fi
if [ "$(id -u)" -ne 0 ]; then
    command -v sudo >/dev/null 2>&1 || { echo "filesystems: run this as root" >&2; exit 1; }
    exec sudo sh "$here/filesystems.sh" "$@"
fi

work=$(mktemp -d /var/tmp/ff-fstest.XXXXXX)
ffd_pid=
passed=0
failed=0
summary=

stop_daemon() {
    if [ -n "$ffd_pid" ]; then
        kill "$ffd_pid" 2>/dev/null
        wait "$ffd_pid" 2>/dev/null
        ffd_pid=
    fi
}

cleanup() {
    stop_daemon
    for m in "$work"/mnt-*; do
        [ -d "$m" ] && umount "$m" 2>/dev/null
    done
    rm -rf "$work"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

have() { command -v "$1" >/dev/null 2>&1; }

# Creates and mounts filesystem $1 at $mnt. Returns 2 with $why set when
# the filesystem cannot be tested on this machine.
make_fs() {
    img="$work/$1.img"
    case "$1" in
        tmpfs)
            mount -t tmpfs -o size=64m tmpfs "$mnt" 2>/dev/null || { why="cannot mount tmpfs"; return 2; }
            return 0 ;;
        ext4)   tool=mkfs.ext4;  size=256M; set -- -q -F ;;
        xfs)    tool=mkfs.xfs;   size=512M; set -- -q -f ;;
        btrfs|btrfs-subvol)
                kind=$1; tool=mkfs.btrfs; size=512M; set -- -q -f ;;
        f2fs)   tool=mkfs.f2fs;  size=256M; set -- -q -f ;;
        vfat)   tool=mkfs.vfat;  size=128M; set -- ;;
        exfat)  tool=mkfs.exfat; size=128M; set -- ;;
        *)      why="unknown filesystem"; return 2 ;;
    esac
    have "$tool" || { why="$tool is not installed"; return 2; }
    truncate -s "$size" "$img"
    "$tool" "$@" "$img" >/dev/null 2>&1 || { why="$tool failed"; return 2; }
    mount -o loop "$img" "$mnt" 2>/dev/null || { why="the kernel cannot mount it"; return 2; }
    if [ "${kind:-}" = btrfs-subvol ]; then
        have btrfs || { why="the btrfs tool is not installed"; return 2; }
        btrfs subvolume create "$mnt/@data" >/dev/null 2>&1 || { why="cannot create a subvolume"; return 2; }
        umount "$mnt"
        mount -o loop,subvol=@data "$img" "$mnt" 2>/dev/null || { why="cannot mount the subvolume"; return 2; }
    fi
    kind=
    return 0
}

count() { "$bin/ff" --db "$db" --count "$@" 2>/dev/null; }

check() { # description, then a command that must succeed within 10 s
    desc=$1; shift
    tries=0
    while [ "$tries" -lt 40 ]; do
        if "$@"; then
            passed=$((passed + 1)); case_pass=$((case_pass + 1))
            printf '    ok    %s\n' "$desc"
            return 0
        fi
        sleep 0.25
        tries=$((tries + 1))
    done
    failed=$((failed + 1)); case_fail=$((case_fail + 1))
    printf '    FAIL  %s\n' "$desc"
    return 1
}

# The index holds exactly $1 entries matching the remaining search terms.
has() { want=$1; shift; [ "$(count "$@")" = "$want" ]; }
# The entry matching the terms has size $1.
size_is() { want=$1; shift; [ "$("$bin/ff" --db "$db" -l "$@" 2>/dev/null | awk '{print $2}')" = "$want" ]; }

run_case() {
    name=$1
    mnt="$work/mnt-$name"
    db="$work/out-$name/index.db"
    case_pass=0
    case_fail=0
    mkdir -p "$mnt"
    why=
    make_fs "$name"
    if [ $? -ne 0 ]; then
        umount "$mnt" 2>/dev/null
        printf '%s: skipped (%s)\n' "$name" "$why"
        summary="$summary$(printf '\n  %-14s skipped: %s' "$name" "$why")"
        return
    fi
    printf '%s:\n' "$name"

    # Content that exists before the daemon starts.
    mkdir "$mnt/pre"
    echo x > "$mnt/pre/ffprobe_existing.txt"

    "$bin/ffd" --output "$db" --interval 0.3 "$mnt" > "$work/$name.log" 2>&1 &
    ffd_pid=$!
    in="$mnt/"   # a path term: restricts every query to this filesystem

    if check "daemon starts and publishes an index" test -s "$db"; then
        check "initial scan sees an existing file" has 1 ffprobe_existing "$in"

        echo hi > "$mnt/ffprobe_one.txt"
        check "file created" has 1 ffprobe_one "$in"

        echo hi > "$mnt/pre/ffprobe_inpre.txt"
        check "file created in a folder that already existed" has 1 ffprobe_inpre "$in"

        mkdir "$mnt/ffprobe_dir"
        echo hi > "$mnt/ffprobe_dir/ffprobe_inner.txt"
        check "folder created" has 1 -d ffprobe_dir "$in"
        check "file created in the new folder" has 1 ffprobe_inner "$in"

        mv "$mnt/ffprobe_one.txt" "$mnt/ffprobe_two.txt"
        check "rename: new name appears" has 1 ffprobe_two "$in"
        check "rename: old name is gone" has 0 ffprobe_one "$in"

        head -c 5000 /dev/zero > "$mnt/ffprobe_two.txt"
        check "rewrite updates the size" size_is 5000 ffprobe_two "$in"

        mv "$mnt/ffprobe_dir" "$mnt/pre/ffprobe_moved"
        check "folder moved: its content follows" has 1 ffprobe_inner "$mnt/pre/ffprobe_moved/"
        check "folder moved: old location is gone" has 0 "$mnt/ffprobe_dir"

        rm "$mnt/ffprobe_two.txt"
        check "file deleted" has 0 ffprobe_two "$in"

        rm -r "$mnt/pre/ffprobe_moved"
        check "folder deleted together with its content" has 0 ffprobe_inner "$in"

        i=0
        while [ "$i" -lt 200 ]; do : > "$mnt/ffprobe_bulk_$i"; i=$((i + 1)); done
        check "200 files created at once" has 200 ffprobe_bulk_ "$in"
        rm "$mnt"/ffprobe_bulk_*
        check "200 files deleted at once" has 0 ffprobe_bulk_ "$in"
    fi

    if [ "$case_fail" -ne 0 ]; then
        echo "    --- daemon log ---"
        sed 's/^/    /' "$work/$name.log"
    fi
    stop_daemon
    umount "$mnt" 2>/dev/null
    summary="$summary$(printf '\n  %-14s %d passed, %d failed' "$name" "$case_pass" "$case_fail")"
}

[ $# -eq 0 ] && set -- $all
for fs in "$@"; do
    run_case "$fs"
done

printf '\nSummary:%s\n\n' "$summary"
if [ "$failed" -ne 0 ]; then
    echo "$failed check(s) FAILED, $passed passed."
    exit 1
fi
echo "All $passed checks passed."
