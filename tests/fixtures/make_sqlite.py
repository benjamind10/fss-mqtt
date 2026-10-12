"""Writes small SQLite fixtures for the payload decoder tests.
Regenerate: docker run --rm -v "$PWD":/w -w /w python:3.12-slim python make_sqlite.py
"""
import os
import sqlite3


def fresh(name, page_size=4096, encoding=None):
    for suffix in ("", "-wal", "-shm", "-journal"):
        if os.path.exists(name + suffix):
            os.remove(name + suffix)
    db = sqlite3.connect(name, isolation_level=None)
    db.execute(f"PRAGMA page_size = {page_size}")
    if encoding:
        db.execute(f"PRAGMA encoding = '{encoding}'")
    return db


# A typical turbine database: several tables, an index, a view, a trigger,
# an INTEGER PRIMARY KEY, NULLs, REAL, TEXT and BLOB values, quoted names.
db = fresh("turbine.db")
db.executescript("""
PRAGMA user_version = 3;
PRAGMA application_id = 1179865421;
CREATE TABLE turbines (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    model TEXT,
    rated_kw REAL,
    commissioned TEXT,
    photo BLOB
);
CREATE TABLE readings (ts INTEGER NOT NULL, turbine TEXT, power_kw REAL, wind_ms REAL, status TEXT);
CREATE INDEX idx_readings_ts ON readings(ts);
CREATE TABLE events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    turbine_id INTEGER REFERENCES turbines(id),
    kind TEXT CHECK (kind IN ('FAULT', 'SERVICE')),
    at TEXT,
    CONSTRAINT fk_turbine FOREIGN KEY (turbine_id) REFERENCES turbines(id)
);
CREATE TABLE "site config" ([key] TEXT PRIMARY KEY, `value` TEXT, "updated, at" INTEGER DEFAULT (0), UNIQUE (value, key));
CREATE VIEW latest AS SELECT turbine, max(ts) AS ts FROM readings GROUP BY turbine;
CREATE TRIGGER log_fault AFTER INSERT ON readings WHEN NEW.status = 'FAULT'
BEGIN
    INSERT INTO events (turbine_id, kind, at) VALUES (CAST(substr(NEW.turbine, 2) AS INTEGER), 'FAULT', NEW.ts);
END;
""")
db.execute("BEGIN")
for i in range(1, 9):
    photo = bytes([0x89, 0x50, 0x4E, 0x47]) if i == 1 else (bytes(range(40)) if i == 2 else None)
    rated = None if i == 5 else 3600.0 if i < 5 else 4200.5
    db.execute("INSERT INTO turbines VALUES (?, ?, ?, ?, ?, ?)",
               (i, f"T{i:02d}", "V117" if i < 5 else "V136", rated, f"2019-0{i}-15", photo))
for i in range(200):
    status = "FAULT" if i % 50 == 13 else "RUNNING"
    wind = None if i % 40 == 7 else round(3.0 + (i % 130) / 10, 1)
    db.execute("INSERT INTO readings VALUES (?, ?, ?, ?, ?)",
               (1_791_454_800 + i * 10, f"T{i % 8 + 1:02d}", (i * 37.5) % 3600 - 12.5, wind, status))
db.execute("INSERT INTO \"site config\" VALUES ('site', 'Hollandse Kust', 1791454800)")
db.execute("INSERT INTO \"site config\" VALUES ('owner', NULL, -1)")
db.execute("COMMIT")
db.close()

# Enough rows for a three-level table b-tree with 512-byte pages.
db = fresh("deep.db", page_size=512)
db.execute("CREATE TABLE samples (id INTEGER PRIMARY KEY, v INTEGER, tag TEXT)")
db.execute("BEGIN")
for i in range(1, 4001):
    db.execute("INSERT INTO samples VALUES (?, ?, ?)", (i, i % 1000, "s" if i % 2 else None))
db.execute("COMMIT")
db.close()

# A TEXT value and a BLOB much longer than a page: overflow chains.
db = fresh("overflow.db", page_size=1024)
db.execute("CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT, raw BLOB)")
body = "".join(f"line {i:04d} of the service log. " for i in range(300))
db.execute("INSERT INTO notes VALUES (1, 'short', NULL)")
db.execute("INSERT INTO notes VALUES (2, ?, ?)", (body, bytes(i % 256 for i in range(5000))))
db.execute("INSERT INTO notes VALUES (3, 'after', x'00ff')")
db.close()

# A WITHOUT ROWID table whose key isn't the first column, deep enough for
# index interior pages.
db = fresh("without_rowid.db", page_size=512)
db.execute("CREATE TABLE settings (scope TEXT, value TEXT, key TEXT, PRIMARY KEY (key, scope)) WITHOUT ROWID")
db.execute("BEGIN")
for i in range(300):
    db.execute("INSERT INTO settings VALUES (?, ?, ?)", (f"T{i % 3 + 1:02d}", f"value {i}", f"key{i // 3:03d}"))
db.execute("COMMIT")
db.close()

# UTF-16le text encoding.
db = fresh("utf16.db", encoding="UTF-16le")
db.execute("CREATE TABLE places (name TEXT, kind TEXT)")
db.execute("INSERT INTO places VALUES ('Ærøskøbing', 'harbour')")
db.execute("INSERT INTO places VALUES ('風車', NULL)")
db.close()

# WAL mode: the main file after a checkpoint (the -wal file is removed on close).
db = fresh("wal.db")
db.execute("PRAGMA journal_mode = WAL")
db.execute("CREATE TABLE log (ts INTEGER, msg TEXT)")
db.execute("INSERT INTO log VALUES (1, 'started')")
db.execute("PRAGMA wal_checkpoint(TRUNCATE)")
db.close()

# No tables at all: a single page.
db = fresh("empty.db")
db.execute("CREATE TABLE t (x)")
db.execute("DROP TABLE t")
db.execute("VACUUM")
db.close()

# Generated columns: VIRTUAL ones aren't stored in the record, STORED ones are.
db = fresh("generated.db")
db.executescript("""
CREATE TABLE power (
    id INTEGER PRIMARY KEY,
    kw REAL,
    mw REAL GENERATED ALWAYS AS (kw / 1000) VIRTUAL,
    site TEXT,
    label TEXT GENERATED ALWAYS AS (site || ':' || id) STORED,
    note TEXT
);
INSERT INTO power (kw, site, note) VALUES (2500.0, 'falster', 'a'), (1800.0, 'lolland', 'b');
""")
db.close()

for name in ("turbine.db", "deep.db", "overflow.db", "without_rowid.db", "utf16.db", "wal.db", "empty.db", "generated.db"):
    print(name, os.path.getsize(name))
