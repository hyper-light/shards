#!/bin/bash
# Reads "FILE<TAB>QUOTE" lines on stdin; prints each quote's line number in FILE, or MISSING.
while IFS=$'\t' read -r file quote; do
  [ -z "$file" ] && continue
  n=$(/usr/bin/grep -n -F -- "$quote" "$file" | head -1 | cut -d: -f1)
  printf '%s\t%s\t%s\n' "${n:-MISSING}" "$(basename "$file")" "$quote"
done
