#!/bin/bash
# Waits (at most $2 seconds) while a process matching $1 runs.
for ((i=0; i<$2; i+=5)); do pgrep -f "$1" >/dev/null || exit 0; sleep 5; done; exit 1
