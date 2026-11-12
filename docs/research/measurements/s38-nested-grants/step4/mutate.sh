# $1 name, $2 file, $3 perl expression; runs real-kernel.sh on the mutant, restores the file.
cd ~/s38-init-step4/src
cp "$2" "$2.orig"; perl -0pi -e "$3" "$2"
if cmp -s "$2" "$2.orig"; then echo "$1 NOT APPLIED"; mv "$2.orig" "$2"; touch "$2"; exit 0; fi
touch "$2"
systemd-run --user --scope -p Delegate=yes -p MemoryMax=16G -p CPUQuota=400% --quiet bash ~/s38-init-step4/real-kernel.sh ~/s38-init-step4/real-kernel-$1.log > /dev/null
echo "$1: $(grep -E '^test .* (ok|FAILED)$' ~/s38-init-step4/real-kernel-$1.log | sed 's/^test //') :: $(grep -A2 'assertion' ~/s38-init-step4/real-kernel-$1.log | tr '\n' ' ' | cut -c1-260)"
mv "$2.orig" "$2"; touch "$2"
