-- A script that mixes dot commands and SQL, which is what tests/shell.rs runs to check that they
-- happen in the order they are written and that a mode set part way through applies to what comes
-- after it and not to what came before.
CREATE TABLE t(a INTEGER, b VARCHAR, c DOUBLE);
INSERT INTO t VALUES (1, 'one', 1.5), (2, 'two', 2.25), (30, 'a longer one', -3.0);
.mode csv
SELECT * FROM t ORDER BY a;
.mode list
.headers off
SELECT count(*) FROM t;
