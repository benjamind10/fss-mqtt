"""Writes small SQLite session changeset/patchset fixtures for the payload decoder tests.
Python's sqlite3 has no session extension; apsw's bundled SQLite does.
Regenerate: docker run --rm -v "$PWD":/w -w /w python:3.12-slim sh -c "pip -q install apsw==3.53.4.0 && python make_changeset.py"
"""
import apsw


def capture(setup, changes):
    con = apsw.Connection(":memory:")
    con.execute(setup)
    s = apsw.Session(con, "main")
    s.attach()
    con.execute(changes)
    return s.changeset(), s.patchset()


setup = """
CREATE TABLE sensors(id INTEGER PRIMARY KEY, name TEXT, reading REAL, raw BLOB, note TEXT);
CREATE TABLE events(id INTEGER PRIMARY KEY, kind TEXT, level INTEGER);
INSERT INTO sensors VALUES (1, 'boiler', 71.5, x'00ff', NULL);
INSERT INTO sensors VALUES (2, 'pump', 3.25, NULL, 'spare');
INSERT INTO sensors VALUES (3, 'fan', -1.0, x'cafe', 'old');
INSERT INTO events VALUES (10, 'start', 1);
"""
changes = """
INSERT INTO sensors VALUES (4, 'valve', 0.5, x'0102030405', NULL);
UPDATE sensors SET reading = 72.0, note = 'hot' WHERE id = 1;
DELETE FROM sensors WHERE id = 3;
INSERT INTO events VALUES (11, 'alarm', -9000000000);
UPDATE events SET kind = 'started' WHERE id = 10;
"""
cs, ps = capture(setup, changes)
open("sensors.changeset", "wb").write(cs)
open("sensors.patchset", "wb").write(ps)

cs, _ = capture(
    """
CREATE TABLE readings(site TEXT, ts INTEGER, value REAL, PRIMARY KEY(site, ts));
INSERT INTO readings VALUES ('oslo', 100, 1.5), ('oslo', 200, 2.5);
""",
    """
INSERT INTO readings VALUES ('bergen', 100, 9.75);
UPDATE readings SET value = 3.0 WHERE site = 'oslo' AND ts = 200;
DELETE FROM readings WHERE site = 'oslo' AND ts = 100;
""",
)
open("composite-pk.changeset", "wb").write(cs)

# A session without changes yields an empty changeset (zero bytes), so there's no fixture for it.
cs, _ = capture("CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT);", "SELECT 1;")
assert cs == b"", cs
