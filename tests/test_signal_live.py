"""Live checks of the core's Signal backend against two signal-cli daemons.

The profile in GINK_LIVE_DATA is already linked to the account served by one daemon (its primary
device); the other daemon serves a second account, the peer. signal-cli is an independent
implementation of the protocol, so these checks say more than gsignal talking to itself. They send
real messages between the two accounts and to the account's own Note to Self, and nowhere else.

Run from the repository root after `zig build`:

    GINK_LIVE=1 python3 -m unittest -v tests.test_signal_live

Two checks start a new session with the peer, which needs a fetch of the peer's keys. The service
allows an account a few of those at once and then about one every ten minutes, and refuses for ten
minutes when that is exceeded, so they run only with GINK_LIVE_REPAIR=1 as well, and not in quick
succession. Check 12 deletes the profile's session with the peer first; if the service then refuses
the fetch, the peer's messages stay undecryptable until its first message ten minutes later, which
makes the core try again.

The profile's account is busy between runs, and the messages it missed are worked through when the
core starts, which can take many minutes. Each check gives the account a minute to come online, so
the suite first runs the core once, as long as CATCH_UP_S allows, until the account is online and
the backlog is gone.

Settings (environment): GINK_LIVE_ACCOUNT_UNIT and GINK_LIVE_PEER_UNIT name the systemd user
units of the two daemons, each started as `signal-cli -a NUMBER daemon --socket PATH`;
GINK_LIVE_DATA is the linked profile's folder; GINK_LIVE_DRIVER is the gink-core program.
"""

import json
import os
import re
import socket
import struct
import subprocess
import tempfile
import threading
import sys
import time
import unittest
import uuid
import zlib

LIVE = os.environ.get("GINK_LIVE") == "1"
REPAIR = os.environ.get("GINK_LIVE_REPAIR") == "1"
ACCOUNT_UNIT = os.environ.get("GINK_LIVE_ACCOUNT_UNIT", "signal-cli.service")
PEER_UNIT = os.environ.get("GINK_LIVE_PEER_UNIT", "signal-cli-dots.service")
DATA = os.environ.get("GINK_LIVE_DATA", "dev-data")
DRIVER = os.environ.get("GINK_LIVE_DRIVER", "zig-out/bin/gink-core")
CATCH_UP_S = 3600  # the longest the suite waits for the profile to work through what it missed


def daemon(unit):
    """(number, socket path) of a signal-cli daemon, read from its unit's command line."""
    text = subprocess.run(["systemctl", "--user", "cat", unit], capture_output=True, text=True, check=True).stdout
    line = next(line for line in text.splitlines() if line.startswith("ExecStart="))
    number = re.search(r"-a (\+\d+)", line).group(1)
    path = re.search(r"--socket (\S+)", line).group(1).replace("%t", os.environ.get("XDG_RUNTIME_DIR", f"/run/user/{os.getuid()}"))
    return number, path


def rpc(path, method, params):
    with socket.socket(socket.AF_UNIX) as s:
        s.settimeout(60)
        s.connect(path)
        s.sendall((json.dumps({"jsonrpc": "2.0", "id": "t", "method": method, "params": params}) + "\n").encode())
        buffer = b""
        while True:
            chunk = s.recv(65536)
            if not chunk:
                raise RuntimeError(f"{method}: the daemon closed the connection")
            buffer += chunk
            *lines, buffer = buffer.split(b"\n")
            for line in lines:
                reply = json.loads(line)
                if reply.get("id") == "t":
                    if "error" in reply:
                        raise RuntimeError(f"{method}: {reply['error']}")
                    return reply["result"]


def png(width, height):
    """A PNG image of random pixels, so that no two are alike."""
    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))

    rows = b"".join(b"\0" + os.urandom(width * 3) for _ in range(height))
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)) + chunk(b"IDAT", zlib.compress(rows)) + chunk(b"IEND", b"")


def journal(unit, since):
    """What a daemon logged since a Unix time: it prints every envelope it receives."""
    return subprocess.run(["journalctl", "--user", "-u", unit, "--since", f"@{int(since)}", "-o", "cat", "--no-pager"], capture_output=True, text=True).stdout


class Driver:
    """gink-core running on a profile (the linked one unless told), with each output line and when it arrived."""

    def __init__(self, *args, data=DATA, offline=False, env=None):
        self.lines = []
        # (offline, it runs in a network namespace of its own, which has no way out)
        command = ["unshare", "-rn"] * offline + [DRIVER, "--data", data, *args]
        self.process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, bufsize=1, env={**os.environ, **(env or {})})
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        for line in self.process.stdout:
            self.lines.append((time.time(), line.rstrip("\n")))

    def tell(self, line):
        """Gives the driver one more thing to do, as its first comment lists them."""
        self.process.stdin.write(line + "\n")
        self.process.stdin.flush()

    def wait(self, pattern, seconds, after=0):
        """The first (time, line) matching pattern among the lines later than `after`, or None after `seconds`."""
        deadline = time.time() + seconds
        while time.time() < deadline:
            for when, line in list(self.lines):
                if when > after and re.search(pattern, line):
                    return when, line
            time.sleep(0.01)
        return None

    def stop(self):
        if self.process.poll() is not None:
            return
        self.process.terminate()
        self.process.wait(timeout=10)
        self.process.stdout.close()
        self.process.stdin.close()


@unittest.skipUnless(LIVE, "set GINK_LIVE=1 to send real messages between the test accounts")
class SignalLive(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.account, cls.account_socket = daemon(ACCOUNT_UNIT)
        cls.peer, cls.peer_socket = daemon(PEER_UNIT)
        # (once online, the core has dealt with what was kept from before and what the service held)
        driver = Driver()
        try:
            started = time.time()
            if driver.wait(r"^account \d+ online", CATCH_UP_S) is None:
                raise unittest.SkipTest(f"the profile did not catch up with its account within {CATCH_UP_S} s")
            print(f"the profile caught up with its account in {time.time() - started:.0f} s", file=sys.stderr)
        finally:
            driver.stop()

    def online(self, *args):
        driver = Driver(*args)
        self.addCleanup(driver.stop)
        self.assertIsNone(driver.wait(r"^sign-in-code", 0.5), "the profile asked to be linked again")
        self.assertIsNotNone(driver.wait(r"^account \d+ online", 60), "the account did not come online")
        return driver

    def test_1_restart_keeps_the_session_and_receives_within_two_seconds(self):
        driver = self.online()
        text = f"gsignal test {uuid.uuid4().hex[:12]}"
        asked = time.time()
        rpc(self.peer_socket, "send", {"recipient": [self.account], "message": text})
        sent = time.time()
        got = driver.wait(rf"^message in .* \| {re.escape(text)}$", 20)
        self.assertIsNotNone(got, "the peer's message did not arrive")
        # The limit is on the time after the peer's daemon returned from sending, which is when the
        # service had the message: the daemon's own work before that varies from 0.1 s to several.
        print(f"\n  received {got[0] - asked:.2f} s after the peer was asked to send, {got[0] - sent:.2f} s after it had sent")
        self.assertLess(got[0] - sent, 2.0)

    def test_2_message_sent_from_the_primary_device_appears(self):
        driver = self.online()
        text = f"gsignal test {uuid.uuid4().hex[:12]}"
        rpc(self.account_socket, "send", {"noteToSelf": True, "message": text})
        self.assertIsNotNone(driver.wait(rf"^message out .* \| {re.escape(text)}$", 20), "the primary device's message did not appear")

    def test_3_message_sent_while_closed_arrives_on_the_next_start(self):
        text = f"gsignal test {uuid.uuid4().hex[:12]}"
        rpc(self.peer_socket, "send", {"recipient": [self.account], "message": text})
        time.sleep(2)
        driver = self.online()
        self.assertIsNotNone(driver.wait(rf"^message in .* \| {re.escape(text)}$", 20), "the message sent while closed did not arrive")

    def peer_conversation(self):
        """The title gsignal has for the conversation with the peer: its name once its profile is known."""
        probe = f"gsignal test {uuid.uuid4().hex[:12]}"
        rpc(self.peer_socket, "send", {"recipient": [self.account], "message": probe})
        driver = self.online()
        got = driver.wait(rf"^message in .* \| {re.escape(probe)}$", 20)
        self.assertIsNotNone(got, "the peer's message did not arrive")
        driver.stop()
        return re.match(r"^message in (.*) \| .* \| ", got[1]).group(1)

    def test_4_message_sent_from_gsignal_reaches_the_peer_and_the_primary_device(self):
        title = self.peer_conversation()
        text = f"gsignal test {uuid.uuid4().hex[:12]}"
        started = time.time() - 1
        driver = self.online("--send", title, text)
        self.assertIsNotNone(driver.wait(rf"^message out .* \| {re.escape(text)}$", 30), "gsignal did not report the message as sent")
        # (the account's daemon takes about 5 s over each message it receives, and earlier tests leave it a queue)
        deadline = time.time() + 120
        while time.time() < deadline and not (text in journal(PEER_UNIT, started) and text in journal(ACCOUNT_UNIT, started)):
            time.sleep(1)
        self.assertIn(text, journal(PEER_UNIT, started), "the peer did not receive it")
        self.assertIn(text, journal(ACCOUNT_UNIT, started), "the account's primary device did not receive a copy")

    def test_5_sent_message_becomes_delivered_then_read(self):
        title = self.peer_conversation()
        text = f"gsignal test {uuid.uuid4().hex[:12]}"
        started = time.time() - 1
        driver = self.online("--send", title, text)
        self.assertIsNotNone(driver.wait(rf"^message out .* \| {re.escape(text)}$", 30), "gsignal did not report the message as sent")
        self.assertIsNotNone(driver.wait(rf"^conversation {re.escape(title)} .* newest=delivered", 30), "no delivery receipt was applied")
        # The peer's daemon logs each envelope with its timestamp, which is how a receipt names a message.
        log = journal(PEER_UNIT, started)
        sent_ms = int(re.findall(r"Timestamp: (\d+)", log[: log.index(text)])[-1])
        rpc(self.peer_socket, "sendReceipt", {"recipient": self.account, "targetTimestamp": [sent_ms], "type": "read"})
        self.assertIsNotNone(driver.wait(rf"^conversation {re.escape(title)} .* newest=read", 30), "no read receipt was applied")

    def test_6_reading_clears_the_unread_count_and_tells_the_primary_device(self):
        title = self.peer_conversation()
        driver = self.online("--read", title)
        text = f"gsignal test {uuid.uuid4().hex[:12]}"
        started = time.time() - 1
        rpc(self.peer_socket, "send", {"recipient": [self.account], "message": text})
        self.assertIsNotNone(driver.wait(rf"^message in .* \| {re.escape(text)}$", 20), "the peer's message did not arrive")
        self.assertIsNotNone(driver.wait(rf"^conversation {re.escape(title)} unread=0 ", 20), "the conversation was not marked read")
        deadline = time.time() + 120
        while time.time() < deadline and "read messages" not in journal(ACCOUNT_UNIT, started).lower():
            time.sleep(1)
        if "read messages" not in journal(ACCOUNT_UNIT, started).lower():
            self.fail("the primary device was not told the message was read; gsignal reported:\n" + "\n".join(line for _, line in driver.lines))

    def test_7_disappearing_message_is_removed_when_its_timer_ends(self):
        title = self.peer_conversation()
        seconds = 30
        rpc(self.peer_socket, "updateContact", {"recipient": self.account, "expiration": seconds})
        self.addCleanup(rpc, self.peer_socket, "updateContact", {"recipient": self.account, "expiration": 0})
        driver = self.online("--read", title)
        text = f"gsignal test {uuid.uuid4().hex[:12]}"
        rpc(self.peer_socket, "send", {"recipient": [self.account], "message": text})
        self.assertIsNotNone(driver.wait(rf"^message in .* \| {re.escape(text)}$", 20), "the peer's message did not arrive")
        read = driver.wait(rf"^conversation {re.escape(title)} unread=0 messages=(\d+) newest=received timer=yes", 20)
        self.assertIsNotNone(read, "the message was not read with its timer running")
        before = int(re.search(r"messages=(\d+)", read[1]).group(1))
        # (the driver reports at most 200 messages, so the count only drops while the conversation is shorter)
        # (looked for after the reading: the notice that the timer was set stands before the message, with no timer of its own)
        gone = driver.wait(rf"^conversation {re.escape(title)} unread=0 messages={before - 1} " if before < 200 else rf"^conversation {re.escape(title)} .* timer=no", seconds + 10, after=read[0])
        self.assertIsNotNone(gone, "the message was not removed")
        print(f"\n  removed {gone[0] - read[0]:.1f} s after being read, with a {seconds} s timer")
        self.assertGreaterEqual(gone[0] - read[0], seconds - 0.5)
        self.assertLess(gone[0] - read[0], seconds + 3)

    def log_has(self, unit, since, pattern, seconds=30):
        """Whether a daemon logs a line matching pattern within `seconds`."""
        deadline = time.time() + seconds
        while time.time() < deadline and not re.search(pattern, journal(unit, since)):
            time.sleep(1)
        return re.search(pattern, journal(unit, since)) is not None

    def settings_on(self, driver):
        """Has the primary device tell gsignal that the account sends read receipts and typing notices."""
        rpc(self.account_socket, "updateConfiguration", {"readReceipts": True, "typingIndicators": True})
        time.sleep(5)

    def test_8_typing_by_the_peer_is_reported(self):
        title = self.peer_conversation()
        driver = self.online()
        rpc(self.peer_socket, "sendTyping", {"recipient": [self.account]})
        self.assertIsNotNone(driver.wait(rf"^typing {re.escape(title)} \| .* \| started$", 20), "the peer's typing was not reported")
        rpc(self.peer_socket, "sendTyping", {"recipient": [self.account], "stop": True})
        self.assertIsNotNone(driver.wait(rf"^typing {re.escape(title)} \| .* \| stopped$", 20), "the end of the peer's typing was not reported")

    def test_9_typing_and_read_receipts_reach_the_peer_when_the_account_sends_them(self):
        title = self.peer_conversation()
        # (each driver is stopped before the next starts: the service gives a device one connection,
        # and two programs on one profile take it from each other, so that a message reaches either)
        driver = self.online()
        self.settings_on(driver)
        driver.stop()
        started = time.time() - 1
        driver = self.online("--typing", title)
        self.assertTrue(self.log_has(PEER_UNIT, started, r"Action: STARTED"), "the peer was not told gsignal's user is typing")
        driver.stop()
        driver = self.online("--read", title)
        text = f"gsignal test {uuid.uuid4().hex[:12]}"
        started = time.time() - 1
        rpc(self.peer_socket, "send", {"recipient": [self.account], "message": text})
        self.assertIsNotNone(driver.wait(rf"^conversation {re.escape(title)} unread=0 ", 20), "the conversation was not marked read")
        self.assertTrue(self.log_has(PEER_UNIT, started, r"Is read receipt"), "the peer got no read receipt; gsignal reported:\n" + "\n".join(line for _, line in driver.lines))

    def test_10_long_text_arrives_whole_in_both_directions(self):
        title = self.peer_conversation()
        # 6,000 bytes with two-byte letters, so the 2,048-byte cut must respect a letter's boundary.
        text = f"gsignal test {uuid.uuid4().hex[:12]} " + "é" * 2985 + " end"
        driver = self.online()
        rpc(self.peer_socket, "send", {"recipient": [self.account], "message": text})
        self.assertIsNotNone(driver.wait(rf"^message in .* \| {re.escape(text)}$", 30), "the peer's long text did not arrive whole")
        driver.stop()
        text = f"gsignal test {uuid.uuid4().hex[:12]} " + "é" * 2985 + " end"
        started = time.time() - 1
        driver = self.online("--send", title, text)
        self.assertIsNotNone(driver.wait(rf"^message out .* \| {re.escape(text)}$", 30), "gsignal did not report the long text as sent")
        self.assertTrue(self.log_has(PEER_UNIT, started, re.escape(text[:40])), "the peer did not receive the long text's beginning")
        self.assertTrue(self.log_has(PEER_UNIT, started, r"text/x-signal-plain"), "the peer did not receive the whole text as an attachment")

    def test_11_mention_is_shown_as_a_name(self):
        self.peer_conversation()
        driver = self.online()
        tag = uuid.uuid4().hex[:12]
        # (the emoji is two UTF-16 units, so the mention's placeholder is at position 3)
        rpc(self.peer_socket, "send", {"recipient": [self.account], "message": f"\U0001F600 X {tag}", "mention": [f"3:1:{self.account}"]})
        got = driver.wait(rf"^message in .* \| \U0001F600 @.+ {tag}$", 20)
        self.assertIsNotNone(got, "the mention was not replaced by a name")
        self.assertNotIn("\ufffc", got[1])

    @unittest.skipUnless(REPAIR, "set GINK_LIVE_REPAIR=1 to spend one of the service's key fetches")
    def test_12_undecryptable_message_is_noted_and_later_ones_arrive(self):
        title = self.peer_conversation()
        peer_id = rpc(self.account_socket, "getUserStatus", {"recipient": [self.peer]})[0]["uuid"]
        # Deleting gsignal's half of the session with the peer makes the peer's next message undecryptable.
        done = subprocess.run(["uv", "run", "-q", "--with", "sqlcipher3-binary", "python", "tests/forget_session.py", DATA, peer_id], capture_output=True, text=True)
        self.assertGreaterEqual(int(done.stdout.strip() or 0), 1, f"no session with the peer was deleted: {done.stderr}")
        driver = self.online()
        rpc(self.peer_socket, "send", {"recipient": [self.account], "message": f"gsignal test {uuid.uuid4().hex[:12]}"})
        self.assertIsNotNone(driver.wait(rf"^message in {re.escape(title)} \| .* \| A message from .* could not be decrypted\.$", 30), "the undecryptable message was not noted")
        self.assertIsNotNone(driver.wait(r"^account \d+ online .* undecryptable=1 ", 5), "the account's count of undecryptable messages did not rise")
        # gsignal starts a new session at once; the peer's daemon needs a moment to receive it.
        time.sleep(10)
        self.assertIsNone(driver.wait(r"^account .* error=.*Rate limit", 0.1), "the service refused the key fetch a new session needs; the session repairs itself with the peer's first message ten minutes from now")
        text = f"gsignal test {uuid.uuid4().hex[:12]}"
        rpc(self.peer_socket, "send", {"recipient": [self.account], "message": text})
        self.assertIsNotNone(driver.wait(rf"^message in .* \| {re.escape(text)}$", 30), "the peer's next message did not arrive")

    @unittest.skipUnless(REPAIR, "set GINK_LIVE_REPAIR=1 to spend one of the service's key fetches")
    def test_15_reset_the_user_asks_for_leaves_the_conversation_working(self):
        title = self.peer_conversation()
        started = time.time() - 1
        driver = self.online("--reset", title)
        time.sleep(10)
        # When the service refuses the key fetch, the old session must have been put back, so the
        # rest holds either way.
        refused = driver.wait(r"^account .* error=.*Rate limit", 0.1) is not None
        print(f"\n  the reset was {'refused by the service, and the old session kept' if refused else 'made'}")
        text = f"gsignal test {uuid.uuid4().hex[:12]}"
        rpc(self.peer_socket, "send", {"recipient": [self.account], "message": text})
        self.assertIsNotNone(driver.wait(rf"^message in .* \| {re.escape(text)}$", 30), "the peer's message after the reset did not arrive")
        self.assertIsNone(driver.wait(r"^account \d+ \S+ \S+ undecryptable=[1-9]", 1), "a message could not be decrypted after the reset")
        driver.stop()
        text = f"gsignal test {uuid.uuid4().hex[:12]}"
        self.online("--send", title, text)
        self.assertTrue(self.log_has(PEER_UNIT, started, re.escape(text)), "the peer did not receive gsignal's message after the reset")

    def test_13_newly_linked_device_lists_the_accounts_groups(self):
        groups = [g["name"] for g in rpc(self.account_socket, "listGroups", {}) if g.get("isMember") and g.get("name")]
        self.assertTrue(groups, "the account is in no group to check with")
        before = {d["id"] for d in rpc(self.account_socket, "listDevices", {})}

        def unlink():
            for device in rpc(self.account_socket, "listDevices", {}):
                if device["id"] not in before:
                    rpc(self.account_socket, "removeDevice", {"deviceId": device["id"]})
            # The peer forgets the removed device on its next send. Left until a new device has
            # taken the same number, signal-cli 0.14.9 fails every send to the account instead.
            rpc(self.peer_socket, "send", {"recipient": [self.account], "message": "gsignal test: a device was removed"})

        self.addCleanup(unlink)
        with tempfile.TemporaryDirectory() as fresh:
            driver = Driver(data=fresh)
            self.addCleanup(driver.stop)
            code = driver.wait(r"^sign-in-code ", 60)
            self.assertIsNotNone(code, "no code to link with was offered")
            rpc(self.account_socket, "addDevice", {"uri": code[1].split(" ", 1)[1]})
            self.assertIsNotNone(driver.wait(r"^account \d+ online", 120), "the new device did not come online")
            # No message has arrived in these groups since linking: they come from the account's own records.
            for name in groups:
                self.assertIsNotNone(driver.wait(rf"^listed group {re.escape(name)}$", 120), f"the group {name} was not listed")
            driver.stop()

    def test_14_group_messages_pass_in_both_directions(self):
        # A group of the two test accounts alone, made on first use, so that nobody else is written to.
        name = "gsignal test"
        group = next((g["id"] for g in rpc(self.peer_socket, "listGroups", {}) if g["name"] == name and g.get("isMember")), None)
        if group is None:
            group = rpc(self.peer_socket, "updateGroup", {"name": name, "member": [self.account]})["groupId"]
            time.sleep(15)
        driver = self.online()
        self.assertIsNotNone(driver.wait(rf"^listed group {re.escape(name)}$", 60), "the group was not listed")
        text = f"gsignal test {uuid.uuid4().hex[:12]}"
        rpc(self.peer_socket, "send", {"groupId": group, "message": text})
        self.assertIsNotNone(driver.wait(rf"^message in {re.escape(name)} \| .+ \| {re.escape(text)}$", 30), "the peer's message to the group did not arrive under the group, with its sender named")
        rpc(self.peer_socket, "sendTyping", {"groupId": [group]})
        self.assertIsNotNone(driver.wait(rf"^typing {re.escape(name)} \| .* \| started$", 20), "the peer's typing in the group was not reported")
        driver.stop()
        text = f"gsignal test {uuid.uuid4().hex[:12]}"
        started = time.time() - 1
        driver = self.online("--send", name, text)
        self.assertIsNotNone(driver.wait(rf"^message out {re.escape(name)} \| .* \| {re.escape(text)}$", 30), "gsignal did not report the group message as sent")
        self.assertTrue(self.log_has(PEER_UNIT, started, re.escape(text)), "the peer did not receive the group message")
        log = journal(PEER_UNIT, started)
        self.assertIn(name, log[log.index(text) :], "the peer did not receive it as a message to the group")
        # The account's primary device gets its copy, as a message sent to the group.
        self.assertTrue(self.log_has(ACCOUNT_UNIT, started, re.escape(text), 120), "the account's primary device did not receive a copy of the group message")
        log = journal(ACCOUNT_UNIT, started)
        copy = log[max(0, log.index(text) - 1500) : log.index(text) + 600]
        self.assertIn("Received sync sent message", copy, "the primary device's copy did not come as a message the account sent")
        self.assertIn(name, copy, "the primary device's copy did not name the group")

    def test_16_reactions_replies_edits_and_deletions_pass_in_both_directions(self):
        title = self.peer_conversation()
        driver = self.online()
        word = lambda: f"gsignal test {uuid.uuid4().hex[:12]}"
        to_account = {"recipient": [self.account]}
        newest = lambda rest: rf"^newest {re.escape(title)} \| {rest} change=none$"

        # The peer writes, gives its own message a reaction and edits it; gsignal adds a reaction of its own.
        first, edited = word(), word()
        at = rpc(self.peer_socket, "send", {**to_account, "message": first})["timestamp"]
        self.assertIsNotNone(driver.wait(rf"^message in .* \| {re.escape(first)}$", 30), "the peer's message did not arrive")
        rpc(self.peer_socket, "sendReaction", {**to_account, "emoji": "\U0001F44D", "targetAuthor": self.peer, "targetTimestamp": at})
        self.assertIsNotNone(driver.wait(newest(rf"{re.escape(first)} \| text edited=no mine= reactions=\U0001F44D who=\U0001F44D {re.escape(title)}"), 30), "the peer's reaction was not shown with its name")
        started = time.time() - 1
        driver.tell(f"react {title} | {first} | \u2764\uFE0F")
        self.assertIsNotNone(driver.wait(newest(rf"{re.escape(first)} \| text edited=no mine=\u2764\uFE0F reactions=\U0001F44D  \u2764\uFE0F who=\U0001F44D {re.escape(title)}   \u2764\uFE0F me"), 30), "gsignal's reaction was not recorded beside the peer's")
        self.assertTrue(self.log_has(PEER_UNIT, started, "Emoji: \u2764"), "the peer did not receive gsignal's reaction")
        rpc(self.peer_socket, "send", {**to_account, "message": edited, "editTimestamp": at})
        self.assertIsNotNone(driver.wait(newest(rf"{re.escape(edited)} \| text edited=yes mine=\u2764\uFE0F .*"), 30), "the peer's edit did not replace the text and keep the reactions")
        driver.tell(f"react {title} | {edited} | ")
        self.assertIsNotNone(driver.wait(newest(rf"{re.escape(edited)} \| text edited=yes mine= reactions=\U0001F44D who=\U0001F44D {re.escape(title)}"), 30), "gsignal's reaction was not taken back")
        # A reaction given on the account's primary device is the account's own here too.
        rpc(self.account_socket, "sendReaction", {"recipient": [self.peer], "emoji": "\U0001F602", "targetAuthor": self.peer, "targetTimestamp": at})
        self.assertIsNotNone(driver.wait(newest(rf"{re.escape(edited)} \| text edited=yes mine=\U0001F602 reactions=\U0001F44D  \U0001F602 who=\U0001F44D {re.escape(title)}   \U0001F602 me"), 120), "a reaction from the primary device was not taken as the account's")

        # gsignal replies to it, edits the reply and deletes it.
        reply, changed = word(), word()
        started = time.time() - 1
        driver.tell(f"reply {title} | {edited} | {reply}")
        self.assertIsNotNone(driver.wait(rf"^message out .* \| {re.escape(reply)}$", 30), "gsignal's reply was not reported as sent")
        self.assertIsNotNone(driver.wait(rf"^reply {re.escape(title)} \| .+ \| {re.escape(edited)}$", 5), "the reply did not record what it answers")
        self.assertTrue(self.log_has(PEER_UNIT, started, rf"Text: {re.escape(edited)}"), "the peer did not receive the reply with the message it answers")
        started = time.time() - 1
        driver.tell(f"edit {title} | {reply} | {changed}")
        self.assertIsNotNone(driver.wait(newest(rf"{re.escape(changed)} \| text edited=yes .*"), 30), "gsignal's edit was not recorded")
        self.assertTrue(self.log_has(PEER_UNIT, started, re.escape(changed)), "the peer did not receive the edit")
        started = time.time() - 1
        driver.tell(f"delete {title} | {changed}")
        self.assertIsNotNone(driver.wait(newest(r" \| deleted .*"), 30), "gsignal's deletion was not recorded")
        self.assertTrue(self.log_has(PEER_UNIT, started, "Remote delete"), f"the peer did not receive the deletion: {driver.lines[-8:]}")

        # The peer replies, then deletes its reply.
        answer, quoted = word(), word()
        at = rpc(self.peer_socket, "send", {**to_account, "message": answer, "quoteTimestamp": at, "quoteAuthor": self.peer, "quoteMessage": quoted})["timestamp"]
        self.assertIsNotNone(driver.wait(rf"^message in .* \| {re.escape(answer)}$", 30), "the peer's reply did not arrive")
        self.assertIsNotNone(driver.wait(rf"^reply {re.escape(title)} \| .+ \| {re.escape(quoted)}$", 5), "the peer's reply did not show what it answers")
        rpc(self.peer_socket, "remoteDelete", {**to_account, "targetTimestamp": at})
        deadline = time.time() + 30
        deletions = lambda: sum(1 for _, line in list(driver.lines) if re.search(newest(r" \| deleted .*"), line))
        while deletions() < 2 and time.time() < deadline:
            time.sleep(0.1)
        self.assertEqual(deletions(), 2, f"the peer's deletion was not shown: {driver.lines[-6:]}")

    def test_17_images_and_files_pass_in_both_directions(self):
        title = self.peer_conversation()
        driver = self.online()
        with tempfile.TemporaryDirectory() as folder:
            # An image from the peer is fetched, described with its size, and saved as it was sent.
            image, text = png(64, 48), f"gsignal test {uuid.uuid4().hex[:12]}"
            sent = os.path.join(folder, "from-peer.png")
            with open(sent, "wb") as f:
                f.write(image)
            rpc(self.peer_socket, "send", {"recipient": [self.account], "message": text, "attachments": [sent]})
            self.assertIsNotNone(driver.wait(rf"^message in .* \| {re.escape(text)}$", 30), "the peer's message with an image did not arrive")
            described = rf"^attachment {re.escape(title)} \| from-peer\.png \| image/png \| {len(image)} \| 64x48 \| here$"
            self.assertIsNotNone(driver.wait(described, 30), f"the image was not fetched and described: {driver.lines[-6:]}")
            saved = os.path.join(folder, "saved.png")
            driver.tell(f"save {title} | {text} | {saved}")
            self.assertIsNotNone(driver.wait(rf"^saved {re.escape(saved)}$", 10), "the image was not saved")
            with open(saved, "rb") as f:
                self.assertEqual(f.read(), image, "the saved image differs from the one sent")

            # A file with no text from the peer shows as a message of its own.
            other = os.path.join(folder, "notes.txt")
            with open(other, "wb") as f:
                f.write(os.urandom(40_000).hex().encode())
            rpc(self.peer_socket, "send", {"recipient": [self.account], "message": "", "attachments": [other]})
            self.assertIsNotNone(driver.wait(rf"^attachment {re.escape(title)} \| notes\.txt \| text/plain \| 80000 \| 0x0 \| here$", 30), f"a file without text was not shown: {driver.lines[-6:]}")

            # An image from gsignal reaches the peer whole, with its name, type and size.
            image, text = png(80, 30), f"gsignal test {uuid.uuid4().hex[:12]}"
            path = os.path.join(folder, "from-gsignal.png")
            with open(path, "wb") as f:
                f.write(image)
            started = time.time() - 1
            driver.tell(f"file {title} | {path} | {text}")
            self.assertIsNotNone(driver.wait(rf"^message out .* \| {re.escape(text)}$", 60), f"gsignal did not report the image as sent: {driver.lines[-6:]}")
            self.assertIsNotNone(driver.wait(rf"^attachment {re.escape(title)} \| from-gsignal\.png \| image/png \| {len(image)} \| 80x30 \| here$", 5), "gsignal did not keep the image it sent")
            self.assertTrue(self.log_has(PEER_UNIT, started, r"Stored plaintext in: \S+"), "the peer did not receive the image")
            log = journal(PEER_UNIT, started)
            log = log[log.index(text) :]
            for expected in ("image/png", "from-gsignal.png", "80x30"):
                self.assertIn(expected, log, f"the peer's copy lacks {expected}")
            with open(re.search(r"Stored plaintext in: (\S+)", log).group(1), "rb") as f:
                self.assertEqual(f.read(), image, "the peer's copy differs from the image sent")

    def test_18_a_member_named_in_a_group_message_is_told(self):
        name = "gsignal test"
        peer_name = self.peer_conversation()
        text = f"gsignal test {uuid.uuid4().hex[:12]}"
        started = time.time() - 1
        driver = self.online()
        self.assertIsNotNone(driver.wait(rf"^listed group {re.escape(name)}$", 60), "the group was not listed")
        driver.tell(f"mention {name} | {peer_name} | {text}")
        self.assertIsNotNone(driver.wait(rf"^message out {re.escape(name)} \| .* \| @{re.escape(peer_name)} {re.escape(text)}$", 30), f"gsignal did not report the message as sent: {driver.lines[-6:]}")
        self.assertTrue(self.log_has(PEER_UNIT, started, re.escape(text)), "the peer did not receive the message")
        log = journal(PEER_UNIT, started)
        log = log[: log.index(text) + 400][log.index(text) - 200 :]
        # The peer is sent a placeholder where its name was, and told that the placeholder means it.
        self.assertIn("\uFFFC " + text, log, "the name was not sent as a placeholder")
        self.assertRegex(log, r"Mentions:\s*\n\s*- .*: 0 \(length: 1\)", "the peer was not told who was mentioned")

    def test_19_message_written_while_offline_is_sent_on_the_next_start(self):
        title = self.peer_conversation()
        text = f"gsignal test {uuid.uuid4().hex[:12]}"
        started = time.time() - 1
        offline = Driver(offline=True)
        self.addCleanup(lambda: offline.process.poll() is None and offline.stop())
        self.assertIsNotNone(offline.wait(rf"^listed direct {re.escape(title)}$", 30), "the conversation was not listed while offline")
        offline.tell(f"send {title} | {text}")
        self.assertIsNotNone(offline.wait(rf"^message out .* \| {re.escape(text)}$", 10), "the message was not stored while offline")
        time.sleep(3)
        self.assertIsNone(offline.wait(r"^account \d+ online", 0), "the account came online without a network")
        offline.stop()
        self.assertNotIn(text, journal(PEER_UNIT, started), "the peer received a message sent without a network")
        # Online again, the stored message goes out by itself and stops waiting.
        driver = self.online()
        self.assertIsNotNone(driver.wait(rf"^conversation {re.escape(title)} .* newest=(sent|delivered|read)", 60), f"the message kept waiting: {driver.lines[-6:]}")
        self.assertTrue(self.log_has(PEER_UNIT, started, re.escape(text)), "the peer did not receive it")

    def test_20_a_photo_is_sent_without_where_and_with_what_it_was_taken(self):
        title = self.peer_conversation()
        text = f"gsignal test {uuid.uuid4().hex[:12]}"
        # A JPEG with an Exif block (the camera on its side, and its maker), a comment, the
        # picture's data, and something after its end.
        exif = b"Exif\0\0MM\0\x2a\0\0\0\x08\0\x02\x01\x12\0\x03\0\0\0\x01\0\x06\0\0\x01\x0f\0\x02\0\0\0\x0a\0\0\0\x26\0\0\0\0SecretCam\0"
        picture = b"\xff\xc0\0\x0b\x08\0\x30\0\x40\x01\x01\x11\0\xff\xda\0\x08\x01\x01\0\0\x3f\0" + os.urandom(2000).replace(b"\xff", b"\x7f") + b"\xff\xd9"
        photo = b"\xff\xd8" + b"\xff\xe1" + struct.pack(">H", len(exif) + 2) + exif + b"\xff\xfe\0\x10secret comment" + picture + b"secret trailer"
        driver = self.online()
        with tempfile.TemporaryDirectory() as folder:
            path = os.path.join(folder, "photo.jpg")
            with open(path, "wb") as f:
                f.write(photo)
            # (a sending that fails is tried again 20 s later, so the wait for the peer allows for two of those)
            started = time.time() - 1
            driver.tell(f"file {title} | {path} | {text}")
            self.assertIsNotNone(driver.wait(rf"^message out .* \| {re.escape(text)}$", 60), f"gsignal did not report the photo as sent: {driver.lines[-6:]}")
            self.assertTrue(self.log_has(PEER_UNIT, started, rf"{re.escape(text)}[\s\S]*Stored plaintext in: \S+", 90), f"the peer did not receive the photo: {driver.lines[-8:]}")
            log = journal(PEER_UNIT, started)
            with open(re.search(r"Stored plaintext in: (\S+)", log[log.index(text) :]).group(1), "rb") as f:
                received = f.read()
        self.assertTrue(received.startswith(b"\xff\xd8") and received.endswith(picture), "the peer's copy lacks the picture")
        self.assertNotIn(b"ecret", received, "the peer's copy says what the photo was taken with")
        # (what is left of the Exif block is which way up the picture is shown)
        self.assertIn(b"\x01\x12\0\x03\0\0\0\x01\0\x06", received, "the peer's copy does not say which way up the photo is")

    def test_21_a_changed_profile_picture_shows_after_the_persons_next_message(self):
        title = self.peer_conversation()
        driver = self.online()
        self.addCleanup(lambda: rpc(self.peer_socket, "updateProfile", {"removeAvatar": True}))
        with tempfile.TemporaryDirectory() as folder:
            got = os.path.join(folder, "got.png")
            # (each follows a message from the peer within the minute in which gsignal does not ask
            # about a person again, so it shows once that minute is up, with no further message)
            for size, seconds in ((96, 100), (128, 100)):
                picture = png(size, size)
                path = os.path.join(folder, f"picture-{size}.png")
                with open(path, "wb") as f:
                    f.write(picture)
                rpc(self.peer_socket, "updateProfile", {"avatar": path})
                text = f"gsignal test {uuid.uuid4().hex[:12]}"
                rpc(self.peer_socket, "send", {"recipient": [self.account], "message": text})
                self.assertIsNotNone(driver.wait(rf"^message in .* \| {re.escape(text)}$", 30), "the peer's message did not arrive")
                deadline, shown = time.time() + seconds, b""
                while time.time() < deadline and shown != picture:
                    del driver.lines[:]
                    driver.tell(f"picture {title} | {got}")
                    if driver.wait(r"^picture .* \| [1-9]", 2):
                        with open(got, "rb") as f:
                            shown = f.read()
                    time.sleep(1)
                self.assertEqual(shown, picture, f"the conversation does not show the peer's {size} px picture within {seconds} s")

    def test_22_a_view_once_picture_is_not_fetched_or_shown(self):
        title = self.peer_conversation()
        driver = self.online()
        with tempfile.TemporaryDirectory() as folder:
            path = os.path.join(folder, "once.png")
            with open(path, "wb") as f:
                f.write(png(40, 40))
            rpc(self.peer_socket, "send", {"recipient": [self.account], "message": "", "attachments": [path], "viewOnce": True})
            self.assertIsNotNone(driver.wait(rf"^message in {re.escape(title)} \| .* \| View-once media\.", 30), f"a view-once picture was not announced: {driver.lines[-6:]}")
            # (a picture that was taken in would be described within a few seconds, as test 17's is)
            self.assertIsNone(driver.wait(r"^attachment .* \| once\.png ", 8), "the view-once picture was taken in as a file")
            self.assertIsNone(driver.wait(rf"^attachment {re.escape(title)} \| .* \| image/png \| \d+ \| 40x40 ", 0), "the view-once picture was taken in as a file")

    def test_23_a_groups_new_name_and_a_changed_timer_are_said_in_the_conversation(self):
        name, other = "gsignal test", f"gsignal test {uuid.uuid4().hex[:4]}"
        title = self.peer_conversation()
        driver = self.online()
        group = next(g["id"] for g in rpc(self.peer_socket, "listGroups", {}) if g["name"].startswith(name))
        # (the driver's line has an empty place for who wrote it, which nobody did)
        said = lambda to: rf'^message in {re.escape(to)} \|  \| The group is now called "{re.escape(to)}"\.$'
        rpc(self.peer_socket, "updateGroup", {"groupId": group, "name": other})
        self.assertIsNotNone(driver.wait(said(other), 40), f"the group's new name was not said: {driver.lines[-6:]}")
        # (changed back while the driver still runs, so that the next check finds the group under its own name)
        rpc(self.peer_socket, "updateGroup", {"groupId": group, "name": name})
        self.assertIsNotNone(driver.wait(said(name), 40), f"the group's old name was not said on its return: {driver.lines[-6:]}")
        # The timer of the conversation with the peer, changed by the peer and changed back.
        self.addCleanup(lambda: rpc(self.peer_socket, "updateContact", {"recipient": self.account, "expiration": 0}))
        rpc(self.peer_socket, "updateContact", {"recipient": self.account, "expiration": 3600})
        self.assertIsNotNone(driver.wait(rf"^message in {re.escape(title)} \|  \| .*disappearing messages are now set to 1 hour\.$", 40), f"the new timer was not said: {driver.lines[-6:]}")
        rpc(self.peer_socket, "updateContact", {"recipient": self.account, "expiration": 0})
        self.assertIsNotNone(driver.wait(rf"^message in {re.escape(title)} \|  \| .*disappearing messages are now off\.$", 40), f"the timer's end was not said: {driver.lines[-6:]}")

    def test_24_a_spoiler_arrives_covered(self):
        title = self.peer_conversation()
        driver = self.online()
        secret, rest = uuid.uuid4().hex[:10], f"gsignal test {uuid.uuid4().hex[:12]}"
        rpc(self.peer_socket, "send", {"recipient": [self.account], "message": f"{secret} {rest}", "textStyle": [f"0:{len(secret)}:SPOILER"]})
        covered = "\u2592" * len(secret)
        self.assertIsNotNone(driver.wait(rf"^message in {re.escape(title)} \| .* \| {covered} {re.escape(rest)}$", 30), f"the message did not arrive with its spoiler covered: {driver.lines[-4:]}")
        time.sleep(2)
        self.assertFalse([line for _, line in driver.lines if secret in line], "the spoiler is shown in the clear somewhere")

    def test_25_a_reaction_given_while_offline_is_sent_on_the_next_start(self):
        title = self.peer_conversation()
        text = f"gsignal test {uuid.uuid4().hex[:12]}"
        driver = self.online()
        rpc(self.peer_socket, "send", {"recipient": [self.account], "message": text})
        self.assertIsNotNone(driver.wait(rf"^message in .* \| {re.escape(text)}$", 30), "the peer's message did not arrive")
        driver.stop()
        offline = Driver(offline=True)
        self.addCleanup(offline.stop)
        self.assertIsNotNone(offline.wait(rf"^listed direct {re.escape(title)}$", 30), "the conversation was not listed while offline")
        started = time.time() - 1
        offline.tell(f"react {title} | {text} | \u2764\uFE0F")
        # (the message says that a change of it waits)
        self.assertIsNotNone(offline.wait(rf"^newest {re.escape(title)} \| {re.escape(text)} \| .* change=waiting$", 10), f"the message did not say that its reaction waits: {offline.lines[-3:]}")
        time.sleep(1)
        offline.stop()
        self.assertFalse("Emoji: \u2764" in journal(PEER_UNIT, started), "the peer received a reaction sent without a network")
        # Online again, the reaction goes out by itself.
        self.online()
        self.assertTrue(self.log_has(PEER_UNIT, started, "Emoji: \u2764", 60), "the peer did not receive the reaction given while offline")

    def test_26_a_message_received_and_not_dealt_with_is_dealt_with_on_the_next_start(self):
        title = self.peer_conversation()
        text = f"gsignal test {uuid.uuid4().hex[:12]}"
        # A run that takes messages off the connection, which tells the service they arrived, and
        # goes no further with them: what being ended at that moment leaves behind.
        held = Driver(env={"GINK_HOLD_RECEIVED": "1"})
        self.addCleanup(held.stop)
        self.assertIsNotNone(held.wait(r"^account \d+ online", 60), "the account did not come online")
        rpc(self.peer_socket, "send", {"recipient": [self.account], "message": text})
        self.assertIsNone(held.wait(rf"^message in .* \| {re.escape(text)}$", 8), "the run that was to hold the message dealt with it")
        held.stop()
        with open(os.path.join(DATA, "gsignal.log"), encoding="utf-8", errors="replace") as f:
            before = f.read().count("messages received before the program last ended")
        driver = self.online()
        self.assertIsNotNone(driver.wait(rf"^message in {re.escape(title)} \| .* \| {re.escape(text)}$", 20), "the message the service had already handed over was lost")
        # (the log says it came from what was kept, not from the service a second time)
        with open(os.path.join(DATA, "gsignal.log"), encoding="utf-8", errors="replace") as f:
            self.assertGreater(f.read().count("messages received before the program last ended"), before, "the message was not taken from what the client had kept")

    def test_27_a_timer_set_in_gsignal_reaches_the_peer(self):
        title = self.peer_conversation()
        driver = self.online()
        self.addCleanup(lambda: rpc(self.peer_socket, "updateContact", {"recipient": self.account, "expiration": 0}))
        for seconds, said in ((3600, "now set to 1 hour"), (0, "now off")):
            started = time.time() - 1
            driver.tell(f"timer {title} | {seconds}")
            self.assertIsNotNone(driver.wait(rf"^message in {re.escape(title)} \|  \| You changed the timer: disappearing messages are {said}\.$", 30), f"the new timer was not said: {driver.lines[-4:]}")
            # (the peer's log says how long a timer is, and of one turned off only that it changed)
            told = rf"Expires in: {seconds} seconds" if seconds else r"Is Expiration update: true"
            self.assertTrue(self.log_has(PEER_UNIT, started, told), f"the peer was not told of the timer of {seconds} s")

    def test_28_files_that_are_not_pictures_go_one_to_a_message(self):
        title = self.peer_conversation()
        text = f"gsignal test {uuid.uuid4().hex[:12]}"
        driver = self.online()
        with tempfile.TemporaryDirectory() as folder:
            paths = []
            for name, data in (("picture.png", png(20, 20)), ("one.mp3", os.urandom(3000)), ("two.mp3", os.urandom(3000))):
                paths.append(os.path.join(folder, name))
                with open(paths[-1], "wb") as f:
                    f.write(data)
            started = time.time() - 1
            driver.tell(f"files {title} | {','.join(paths)} | {text}")
            self.assertTrue(self.log_has(PEER_UNIT, started, r"Filename: two\.mp3", 90), "the second sound file did not reach the peer")
        # Three messages: the text with the picture, then each sound file alone, as Signal's programs send them.
        log = journal(PEER_UNIT, started)
        envelopes = [e for e in log.split("Envelope from:") if "Filename:" in e]
        files = [re.findall(r"Filename: (\S+)", e) for e in envelopes]
        self.assertEqual(files, [["picture.png"], ["one.mp3"], ["two.mp3"]], "the files were not sent one kind to a message")
        self.assertIn(text, envelopes[0], "the text did not go with the picture")


if __name__ == "__main__":
    unittest.main()
