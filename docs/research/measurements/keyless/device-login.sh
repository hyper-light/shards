#!/bin/sh
# What a person does with a device code at Dex, by curl: the code entered, the one
# connector followed (its ID escaped once), Dex's page then saying the login succeeded.
#
#   device-login.sh CODE
dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT
curl -s -c "$dir/jar" -b "$dir/jar" -L -o "$dir/page" -d "user_code=$1" \
	http://dex-idp:8888/auth/device/auth/verify_code
link=$(grep -o 'href="/auth/auth/[^"]*"' "$dir/page" | head -1 | sed -e 's/^href="//' -e 's/"$//' \
	-e 's/&amp;/\&/g' -e 's/&#43;/+/g' -e 's/%25/%/g')
curl -s -c "$dir/jar" -b "$dir/jar" -L -o "$dir/done" "http://dex-idp:8888$link"
grep -q "Login Successful" "$dir/done"
