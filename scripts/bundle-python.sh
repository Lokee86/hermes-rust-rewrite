#!/bin/sh
# Copy the CPython runtime that hermes-python-host was linked against (the prefix of the given
# interpreter) to a package, without what plugins never use: tests, IDLE, Tk, headers,
# build configuration and the standalone interpreter executables (the host embeds libpython).
set -eu
python=${1:?Pass the interpreter hermes-python-host was built against}
destination=${2:?Pass the destination folder}
prefix=$("$python" -c 'import sys; print(sys.base_prefix)')
version=$("$python" -c 'import sys; print("%d.%d" % sys.version_info[:2])')
mkdir -p "$destination"
rsync -a \
    --exclude "/bin/" --exclude "/include/" --exclude "/share/" --exclude "/lib/pkgconfig/" \
    --exclude "/lib/tcl*" --exclude "/lib/tk*" --exclude "/lib/itcl*" --exclude "/lib/thread*" \
    --exclude "/lib/python$version/test/" --exclude "/lib/python$version/idlelib/" \
    --exclude "/lib/python$version/tkinter/" --exclude "/lib/python$version/turtledemo/" \
    --exclude "/lib/python$version/config-$version-*/" --exclude "/lib/python$version/lib-dynload/_tkinter*" \
    "$prefix/" "$destination/"
test -f "$destination/lib/python$version/os.py"
