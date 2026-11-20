#!/usr/bin/env bash
# Which C runtime shards' x86_64 Windows build links (D36, amended 2026-11-19). `build` builds
# `shards` from the checkout SRC into the target directory TGT as release.yml does (`cargo
# build --locked --release`; with GPU_LIBS, the renderer scripts/gpu-libs builds there first,
# carried as shards-vm-gpu), then inspects it. `inspect` reads a build of TGT:
# - the DLLs each executable `shards` carries imports (dumpbin /DEPENDENTS);
# - the C runtime libraries every static library compiled for the target asks the linker for
#   (dumpbin /DIRECTIVES, its /DEFAULTLIB:s), shards' and its helpers' nested builds', and the
#   GPU's;
# - `shards --version`.
# Git for Windows' or MSYS2's bash on x86_64 Windows with Visual Studio's C++ tools, NASM and
# the pinned zig on PATH (and, for GPU_LIBS, LLVM's clang-cl and a Python of 3.10 or later).
#
#   crt-build.sh build SRC TGT [GPU_LIBS]
#   crt-build.sh inspect TGT [GPU_LIBS]
set -u
mode=$1
shift
dumpbin=$("/c/Program Files (x86)/Microsoft Visual Studio/Installer/vswhere.exe" -latest -products '*' \
	-find 'VC/Tools/MSVC/**/bin/Hostx64/x64/dumpbin.exe' | tr -d '\r' | head -1)
[ -n "$dumpbin" ] || { echo "no dumpbin.exe (Visual Studio's C++ tools)" >&2; exit 2; }
dumpbin() { MSYS2_ARG_CONV_EXCL='*' "$dumpbin" /NOLOGO "$1" "$(cygpath -w "$2")" | tr -d '\r'; }

inspect() {
	local tgt=$1 gpu=${2:-} rel out f lib
	rel=$tgt/x86_64-pc-windows-msvc/release
	out=$(ls -dt "$rel"/build/shards-*/out/helpers | head -1)
	for f in "$rel/shards.exe" "$out"/*; do
		if [ ! -s "$f" ]; then
			echo "== $(basename "$f"): empty, not carried"
			continue
		fi
		echo "== $(basename "$f"), $(stat -c %s "$f") bytes, sha256 $(sha256sum "$f" | cut -c1-16)"
		dumpbin /DEPENDENTS "$f" | awk '/following (delay load )?dependencies/,/Summary/' |
			grep -i -E '\.dll$' | sed 's/^ */  /'
	done
	find "$tgt" -path '*/x86_64-pc-windows-msvc/release/build/*/out/*' -name '*.lib' | sort | while read -r lib; do
		echo "== ${lib#"$tgt"/}: $(dumpbin /DIRECTIVES "$lib" | grep -o -i -E '/DEFAULTLIB:"?[A-Za-z0-9_.]+' |
			tr 'A-Z' 'a-z' | sed -E 's|/defaultlib:"?||; s|\.lib$||' | sort | uniq -c | xargs)"
	done
	if [ -n "$gpu" ]; then
		for lib in "$gpu"/*.lib; do
			echo "== GPU $(basename "$lib"): $(dumpbin /DIRECTIVES "$lib" | grep -o -i -E '/DEFAULTLIB:"?[A-Za-z0-9_.]+' |
				tr 'A-Z' 'a-z' | sed -E 's|/defaultlib:"?||; s|\.lib$||' | sort | uniq -c | xargs)"
		done
	fi
	version=$("$rel/shards.exe" --version 2>&1)
	echo "== shards --version: exit $?, $version"
}

case $mode in
build)
	src=$1 tgt=$2 gpu=${3:-}
	echo "== $(git -C "$src" rev-parse HEAD), $(rustc --version), $(nasm -v | head -1)"
	if [ -n "$gpu" ]; then
		start=$(date +%s)
		"$src/scripts/gpu-libs" "$gpu" || exit
		echo "gpu-libs: $(($(date +%s) - start)) s"
		export SHARDS_GPU_LIBS=$gpu
	fi
	start=$(date +%s)
	(cd "$src" && CARGO_TARGET_DIR=$tgt cargo build --locked --release -p shards --target x86_64-pc-windows-msvc) ||
		exit
	echo "build: $(($(date +%s) - start)) s"
	inspect "$tgt" "$gpu"
	;;
inspect) inspect "$@" ;;
*) echo "crt-build.sh build SRC TGT [GPU_LIBS] | inspect TGT [GPU_LIBS]" >&2; exit 2 ;;
esac
