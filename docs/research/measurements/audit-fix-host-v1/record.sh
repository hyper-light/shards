#!/bin/sh
# Runs one light check from the worktree's root and keeps its evidence: the command line,
# the source state it ran against (HEAD, the uncommitted diff and its SHA-256, untracked
# files' hashes), the host's free space where it runs cargo, and its raw output and exit.
#
#   record.sh NAME COMMAND...
#
# Writes logs/NAME.log and logs/NAME.diff beside this script. A cargo command is refused
# while the home volume has less than 30 GiB free (R16).
set -u
dir=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$dir/../../../.." && pwd)
name=$1
shift
mkdir -p "$dir/logs"
log="$dir/logs/$name.log"
git -C "$root" diff HEAD -- . ":(exclude)docs/research/measurements/audit-fix-host-v1" >"$dir/logs/$name.diff"
{
	printf '# command:'
	printf ' %s' "$@"
	printf '\n# cwd: %s\n' "$root"
	printf '# head: %s\n' "$(git -C "$root" rev-parse HEAD)"
	printf '# source diff vs HEAD (excluding this evidence directory): %s, sha256 %s\n' \
		"$(wc -l <"$dir/logs/$name.diff" | tr -d ' ') lines" \
		"$(shasum -a 256 "$dir/logs/$name.diff" | cut -d' ' -f1)"
	git -C "$root" ls-files --others --exclude-standard -- . ":(exclude)docs/research/measurements/audit-fix-host-v1" |
		while read -r f; do printf '# untracked: %s sha256 %s\n' "$f" "$(shasum -a 256 "$root/$f" | cut -d' ' -f1)"; done
	printf '# host: %s\n' "$(uname -srm)"
	printf '# started: %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
} >"$log"
case " $* " in
*" cargo "*)
	free=$(df -g /Users/adalundhe | awk 'NR==2 {print $4}')
	printf '# free GiB on /Users/adalundhe: %s\n' "$free" >>"$log"
	if [ "$free" -lt 30 ]; then
		printf '# refused: below the 30 GiB floor (R16)\n' >>"$log"
		exit 3
	fi
	;;
esac
(cd "$root" && "$@") >>"$log" 2>&1
code=$?
printf '# exit: %s at %s\n' "$code" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" >>"$log"
exit "$code"
