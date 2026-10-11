#!/bin/sh
# MAN-268: keep the final 1 MiB of a manta log once it exceeds 10 MiB.
# Run once a minute by com.hagaletechnologies.manta-logrotate.plist as
# /usr/local/libexec/manta-rotate-log.sh; policy in packaging/README.md.
# The log is overwritten through its own inode, never renamed or replaced:
# manta keeps writing to the stdout/stderr launchd opened for it and does
# not reopen them (SIGHUP reloads its lists, MAN-78). Bytes manta writes between the
# copy and the overwrite are lost. Silent on success; any failure exits
# nonzero so `launchctl print` shows it.
set -eu

max_bytes=10485760
keep_bytes=1048576

fail() {
    printf 'manta-rotate-log: %s\n' "$*" >&2
    exit 1
}

if [ "$#" -ne 1 ]; then
    printf 'usage: rotate-log.sh LOG_PATH\n' >&2
    exit 2
fi
log=$1
# A leading ./ keeps a relative name like -x from reading as an option.
case $log in
    /*) ;;
    *) log=./$log ;;
esac

# -L first: a dangling symlink fails -e and must not pass as a missing log.
if [ -L "$log" ]; then
    fail "refusing a symlink: $log"
fi
if [ ! -e "$log" ]; then
    exit 0
fi
if [ ! -f "$log" ]; then
    fail "not a regular file: $log"
fi

# Byte count of a regular file; wc pads it with spaces on macOS.
size_of() {
    n=$(wc -c < "$1") || return 1
    n=$(printf '%s' "$n" | tr -d '[:space:]')
    case $n in
        '' | *[!0-9]*) return 1 ;;
    esac
    printf '%s\n' "$n"
}

size=$(size_of "$log") || fail "cannot read the size of $log"
if [ "$size" -le "$max_bytes" ]; then
    exit 0
fi

dir=$(dirname "$log") || fail "cannot find the directory of $log"
tmp=
trap 'if [ -n "$tmp" ]; then rm -f "$tmp"; fi' EXIT
trap 'exit 1' HUP INT TERM
tmp=$(mktemp "$dir/.manta-rotate.XXXXXX") || fail "cannot create a temporary file in $dir"

tail -c "$keep_bytes" "$log" > "$tmp" || fail "cannot copy the end of $log"
kept=$(size_of "$tmp") || fail "cannot read the size of $tmp"
if [ "$kept" -ne "$keep_bytes" ]; then
    fail "copied $kept bytes of $log, expected $keep_bytes; log left unchanged"
fi
cat "$tmp" > "$log" || fail "cannot overwrite $log in place"
rm -f "$tmp" || fail "cannot remove $tmp"
tmp=
