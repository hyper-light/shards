#!/bin/sh
# Run as root in scripts/ext4/generate's Linux container: makes /work/tree.raw, an ext4
# file system of a tree with every kind of file and structure a guest's layer holds, as
# mke2fs 1.47.0 copies a directory into one (-d) and e2fsck indexes its directories (-D);
# then has debugfs, e2fsprogs' own reader, say what it holds: each file's inode
# (/work/stat.out), its attributes (/work/ea.out, /work/ea/*) and its bytes (/work/dump/*).
# scripts/ext4/record.py records what it said for crates/ext4's reader to be held to.
set -eu
# Every time e2fsprogs writes, and every file's, one moment: the same bytes each time.
export E2FSPROGS_FAKE_TIME=1792486936 E2FSCK_TIME=1792486936

tree=/tree
mkdir "$tree"
cd "$tree"
: >empty
printf 'hello\n' >small
seq 1 3000 >multi
# Eight runs of data with holes between: more extents than an inode holds, a tree of them.
dd if=/dev/zero of=sparse bs=4096 count=0 seek=64 2>/dev/null
for i in 0 5 10 15 20 25 30 35; do
	printf 'block %s' "$i" | dd of=sparse bs=4096 seek="$i" conv=notrunc 2>/dev/null
done
# Hundreds of names: several blocks, which e2fsck -D indexes with a hash tree.
mkdir many
i=0
while [ "$i" -lt 300 ]; do
	: >"many/a-file-with-a-longer-name-than-most-$i"
	i=$((i + 1))
done
mkdir -p deep/er/still
printf 'deep\n' >deep/er/still/file
ln -s small fast-link
ln -s "$(printf 'x%.0s' $(seq 1 100))" slow-link
ln small hard-link
mknod chardev c 1 3
mknod blockdev b 8 1
mknod wide-dev c 300 70000
mkfifo fifo
: >setuid
chmod 4755 setuid
mkdir setgid-dir sticky-dir opaque
chmod 2755 setgid-dir
chmod 1777 sticky-dir
: >noperm
chmod 000 noperm
: >owned
chown 1000:1001 owned
: >wide-ids
chown 100000:200000 wide-ids
: >acl
# 255 bytes, the most a name may have.
: >"$(printf 'n%.0s' $(seq 1 255))"
: >'ünïcödé'
: >future
: >past
find "$tree" -exec touch -h -d @1792486936 {} +
touch -h -d '2001-02-03 04:05:06' small
touch -h -d '2100-01-01 00:00:00' future
touch -h -d '1960-01-01 00:00:00' past
cd /

E2FSPROGS_FAKE_TIME=1792486936 mke2fs -q -F -t ext4 -b 4096 -I 256 \
	-U 0b5e5e7a-9a0e-4b6c-9f3e-2f7c2d1a4b5e -E hash_seed=6d1c3c2a-58f0-4f7e-a9a5-3b9a2e8c1d70,root_owner=0:0 \
	-d "$tree" /work/tree.raw 16384

# Attributes: one in the inode, one too large for it (its own block), a capability's
# binary value, overlayfs' opaque mark, and an ACL, which the file system keeps in its own
# form and readers are given in the VFS's. Times past what 32 bits hold and to the
# nanosecond, which mke2fs -d does not copy, in the inodes' extra fields: 123456789 ns, and
# the epoch past 2038 that 2100's seconds need, with 0.5 s.
printf '\001\000\000\002\040\000\000\000\000\000\000\000\000\000\000\000\000\000\000\000' >/work/cap.bin
seq 1 1000 | tr '\n' ' ' | head -c 3000 >/work/big.bin
printf '\002\000\000\000\001\000\006\000\377\377\377\377\002\000\004\000\350\003\000\000\004\000\004\000\377\377\377\377\020\000\004\000\377\377\377\377\040\000\004\000\377\377\377\377' >/work/acl.bin
debugfs -w /work/tree.raw >/dev/null <<'EOF'
ea_set /small user.note hello
ea_set -f /work/big.bin /multi user.big
ea_set -f /work/cap.bin /setuid security.capability
ea_set /opaque trusted.overlay.opaque y
ea_set -f /work/acl.bin /acl system.posix_acl_access
set_inode_field /small mtime_extra 0x1d6f3454
set_inode_field /future mtime_extra 0x77359401
EOF
# Every change time too, which mke2fs -d copies from the tree and nothing can set there.
{
	echo /
	cd "$tree" && find . -mindepth 1 | sed 's|^\.||'
} | while IFS= read -r p; do
	printf 'set_inode_field "%s" ctime @1792486936\n' "$p"
done >/work/ctime.cmds
debugfs -w -f /work/ctime.cmds /work/tree.raw >/dev/null 2>&1
# Directories of more than a block indexed with hash trees, then checked.
set +e
e2fsck -fyD /work/tree.raw >/work/e2fsck-D.out 2>&1
e2fsck -fn /work/tree.raw >/work/e2fsck.out 2>&1
echo $? >/work/e2fsck.exit
set -e

# Every path, as find walks the tree, and lost+found, which mke2fs makes.
{
	echo /lost+found
	cd "$tree" && find . -mindepth 1 | sed 's|^\.||'
} | LC_ALL=C sort >/work/paths
mkdir /work/dump /work/ea
n=0
: >/work/stat.cmds
: >/work/ea.cmds
while IFS= read -r p; do
	printf 'stat "%s"\n' "$p" >>/work/stat.cmds
	printf 'ea_list "%s"\n' "$p" >>/work/ea.cmds
	# A file's bytes; a symbolic link's target, where it has a block of its own.
	if { [ -f "$tree$p" ] || [ -L "$tree$p" ]; } && [ "$p" != /lost+found ]; then
		printf 'dump "%s" /work/dump/%s\n' "$p" "$n" >>/work/stat.cmds
	fi
	n=$((n + 1))
done </work/paths
debugfs -f /work/stat.cmds /work/tree.raw >/work/stat.out 2>&1
debugfs -f /work/ea.cmds /work/tree.raw >/work/ea.out 2>&1
# Each attribute's value, by its name, which ea_list said.
: >/work/eaget.cmds
n=0
while IFS= read -r p; do
	for name in user.note user.big security.capability trusted.overlay.opaque system.posix_acl_access; do
		printf 'ea_get -f /work/ea/%s.%s "%s" %s\n' "$n" "$name" "$p" "$name" >>/work/eaget.cmds
	done
	n=$((n + 1))
done </work/paths
debugfs -f /work/eaget.cmds /work/tree.raw >/work/eaget.out 2>&1 || true

# A journal to replay: the tree's file system with transactions written to its journal and
# not yet in place (/work/journal.raw), as a guest's killed microVM leaves its layer, and
# what debugfs makes of it, replayed (/work/replayed.raw). /multi's blocks: the first two
# and the fourth rewritten by transaction 1, whose first block begins with jbd2's magic
# (escaped in the log); its second revoked by transaction 2, which leaves what it held; its
# third rewritten by transaction 3, after transaction 1's version of it.
cp /work/tree.raw /work/journal.raw
debugfs -R 'blocks /multi' /work/journal.raw >/work/multi.blocks 2>/dev/null
set -- $(cat /work/multi.blocks)
b1=$1 b2=$2 b3=$3 b4=$4
{
	printf '\300\073\071\230'
	head -c 4092 /dev/zero | tr '\0' 'e'
	head -c 4096 /dev/zero | tr '\0' 'f'
	head -c 4096 /dev/zero | tr '\0' 'g'
} >/work/t1.bin
head -c 4096 /dev/zero | tr '\0' 'h' >/work/t3.bin
debugfs -w /work/journal.raw >/dev/null 2>&1 <<CMDS
jo -c
jw -b $b1,$b2,$b4 /work/t1.bin
jw -r $b2
jw -b $b3 /work/t3.bin
jc
CMDS
debugfs -R 'logdump' /work/journal.raw >/work/logdump.out 2>&1
cp /work/journal.raw /work/replayed.raw
debugfs -w -R 'jr' /work/replayed.raw >/work/jr.out 2>&1
e2fsck -fn /work/replayed.raw >/work/replayed-e2fsck.out 2>&1
echo $? >/work/replayed-e2fsck.exit
