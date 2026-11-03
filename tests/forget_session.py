"""Deletes a linked profile's sessions with one person, so their next message cannot be decrypted,
and lets the core start a new session with them at once.

    uv run --with sqlcipher3-binary python tests/forget_session.py DATA_FOLDER SERVICE_ID

The profile's store is a SQLCipher database, which Python's own sqlite3 cannot open. Prints how many
sessions were deleted. For tests/test_signal_live.py, on a profile no gsignal process has open.
"""

import sys

import sqlcipher3

folder, person = sys.argv[1:3]
with open(f"{folder}/database.key", "rb") as f:
    key = f.read().hex()
db = sqlcipher3.connect(f"{folder}/account-1.db")
db.execute(f"PRAGMA key = \"x'{key}'\"")
print(db.execute("DELETE FROM sessions WHERE address = ?", (person,)).rowcount)
db.commit()

# The core starts a new session with a person at most once an hour; forget when it last did.
db = sqlcipher3.connect(f"{folder}/gsignal.db")
db.execute(f"PRAGMA key = \"x'{key}'\"")
db.execute("UPDATE people SET repair_after_ms = 0 WHERE key = ?", (person,))
db.commit()
