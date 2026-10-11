#!/bin/sh
# The harness's browser, as `xdg-open` on PATH: what a person does at Dex's mock connector,
# by curl, in the background as a browser opens. The authorization URL's page lists the one
# connector; following its link (its ID escaped once: Dex's page escapes it twice) signs
# in, and Dex redirects to the listener waiting on this host.
url=$1
dir=$(mktemp -d)
(
	curl -s -c "$dir/jar" -b "$dir/jar" -o "$dir/page" "$url"
	link=$(grep -o 'href="/auth/auth/[^"]*"' "$dir/page" | head -1 | sed -e 's/^href="//' -e 's/"$//' \
		-e 's/&amp;/\&/g' -e 's/&#43;/+/g' -e 's/%25/%/g')
	curl -s -c "$dir/jar" -b "$dir/jar" -L -o /dev/null "http://dex-idp:8888$link"
	rm -rf "$dir"
) >/dev/null 2>&1 &
