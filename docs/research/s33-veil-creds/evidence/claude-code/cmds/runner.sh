#!/bin/bash
# Usage: runner.sh CMDFILE OUTFILE "HEADER"
# Runs each line of CMDFILE with bash eval (B and GG come from the environment) and records
# "## command: <line>", its output, and "## end". printf, not echo, so backslashes are kept as written.
cmdfile="$1"; out="$2"; header="$3"
{
  printf '%s\n' "$header"
  printf '%s\n' "# B=$B"
  printf '%s\n' "# GG=$GG"
  while IFS= read -r c; do
    printf '## command: %s\n' "$c"
    eval "$c" 2>&1
    printf '## end\n'
  done < "$cmdfile"
} > "$out"
