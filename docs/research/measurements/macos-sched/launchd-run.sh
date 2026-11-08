#!/bin/zsh
# Runs COMMAND as a launchd job of this user's GUI session (ProcessType Interactive), and
# waits for it to end: a program started so has no QoS clamp of its starter's, as one a
# user starts from a terminal has none, where a program an agent's background session
# starts inherits that session's (utility, every thread at 20 or below). Its output goes
# to OUT.log. Usage: launchd-run.sh OUT COMMAND...
out=$1; shift
label=com.hyperlight.shards.measure.$$
plist=${out}.plist
{
  print '<?xml version="1.0" encoding="UTF-8"?>'
  print '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">'
  print "<plist version=\"1.0\"><dict><key>Label</key><string>$label</string>"
  print '<key>ProgramArguments</key><array><string>/bin/zsh</string><string>-c</string>'
  print "<string>cd ${(q)PWD} &amp;&amp; ${${(j: :)${(q)@}}//&/&amp;}; print \$? &gt; ${(q)out}.exit</string></array>"
  print "<key>EnvironmentVariables</key><dict><key>PATH</key><string>$PATH</string><key>HOME</key><string>$HOME</string><key>TMPDIR</key><string>$TMPDIR</string></dict>"
  print "<key>ProcessType</key><string>Interactive</string><key>RunAtLoad</key><true/>"
  print "<key>StandardOutPath</key><string>${out}.log</string><key>StandardErrorPath</key><string>${out}.log</string>"
  print '</dict></plist>'
} > $plist
rm -f ${out}.exit ${out}.log
launchctl bootstrap gui/$(id -u) $plist || exit 1
until [[ -e ${out}.exit ]]; do sleep 1; done
launchctl bootout gui/$(id -u)/$label 2>/dev/null
exit $(cat ${out}.exit)
