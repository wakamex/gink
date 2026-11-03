"""Prints when gink and Signal Desktop each received messages, from the files they keep on this machine.

Run on the Windows machine by tools/arrival-lag. Each line is tab-separated: which program (G for
gink, S for Signal Desktop), what happened, when in milliseconds, and for a message the time it
was sent, by which the two programs' lines are paired. It prints times alone.
"""

import calendar
import glob
import os
import re
import time

EVENTS = (
    ("queued", re.compile(r"\[MessageReceiver\] queueing envelope .* (\d{13}) \(")),  # (off the connection)
    ("saved", re.compile(r"\[saveAndNotify\] Message saved (\d{13})")),  # (stored, and shown)
    ("connecting", re.compile(r"\[SocketManager\] connecting authenticated socket")),
    ("online", re.compile(r"\[MessageReceiver\] emitting 'empty' event")),
)
LOGGED = re.compile(r'"time":"(\d{4})-(\d\d)-(\d\d)T(\d\d):(\d\d):(\d\d)\.(\d{3})Z"')


def main():
    arrivals = os.path.expandvars(r"%APPDATA%\wakamex\gink\arrivals.txt")
    if os.path.exists(arrivals):
        with open(arrivals, encoding="utf-8") as f:
            for line in f:
                part = line.split()
                if len(part) >= 2:
                    print("G", part[1], part[0], part[2] if len(part) > 2 else "", sep="\t")
    for path in glob.glob(os.path.expandvars(r"%APPDATA%\Signal\logs\app.log*")):
        with open(path, encoding="utf-8", errors="replace") as f:
            for line in f:
                for what, pattern in EVENTS:
                    found = pattern.search(line)
                    when = found and LOGGED.search(line)
                    if when:
                        *date, ms = (int(n) for n in when.groups())
                        print("S", what, calendar.timegm((*date, 0, 0, 0)) * 1000 + ms, found.group(1) if found.groups() else "", sep="\t")
                        break


if __name__ == "__main__":
    main()
