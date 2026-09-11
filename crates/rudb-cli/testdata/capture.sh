#!/bin/sh
# Recaptures the golden output in this directory from a real DuckDB binary.
#
# Run it from anywhere with `duckdb` on the path. It overwrites every mode-*.txt file and rewrites
# duckdb-version.txt, so the version recorded there is always the version that produced the bytes
# beside it. The shell test in tests/shell.rs diffs against these, which is the only reason any of
# the claims in src/format.rs about DuckDB are worth believing.
#
# The `duckdb` it wants is the pinned one, which `scripts/oracle` installs and which no package
# manager has. That means one of the machines rather than a laptop, and it means the files in here
# are Linux output: `.mode csv` ends its lines with a carriage return and a newline there.
set -e
here=$(cd "$(dirname "$0")" && pwd)
duckdb -version >"$here/duckdb-version.txt"
for mode in duckbox box table markdown line list csv tabs json jsonlines quote insert html ascii column; do
  duckdb -batch -init /dev/null \
    -cmd ".read $here/setup.sql" \
    -cmd ".mode $mode" \
    -c "SELECT * FROM t ORDER BY a" >"$here/mode-$mode.txt"
done
duckdb -batch -init /dev/null -cmd ".read $here/setup.sql" -c "SELECT * FROM t WHERE a > 9000" >"$here/empty.txt"
duckdb -batch -init /dev/null -c ".show" >"$here/show.txt"
# One file per line of counts.sql, which is where the row count and the column count and the row of
# dots get their shapes. The test reads the same file, so a query added here needs nothing else.
at=0
while IFS= read -r query; do
  at=$((at + 1))
  duckdb -batch -init /dev/null -c "$query" >"$here/counts-$at.txt"
done <"$here/counts.sql"
