# Windows executables without Visual C++'s redistributable

`shards` is the one file to install (D36). On Windows it was not: Rust's
`*-pc-windows-msvc` targets link the C runtime as DLLs unless the `crt-static` target
feature is on ([Static and dynamic C runtimes](https://doc.rust-lang.org/reference/linkage.html#static-and-dynamic-c-runtimes)),
so `shards.exe` and the helpers it carries imported `VCRUNTIME140.dll`, which Visual C++'s
redistributable installs ([Redistribute Visual C++ Files](https://learn.microsoft.com/en-us/cpp/windows/redistributing-visual-cpp-files)),
and the UCRT's `api-ms-win-crt-*` sets, `ucrtbase.dll`'s, a part of Windows 10 and later
([CRT .lib files](https://learn.microsoft.com/en-us/cpp/c-runtime-library/crt-library-features)).

## The change

- `.cargo/config.toml`: `-Ctarget-feature=+crt-static` for `x86_64-pc-windows-msvc` and
  `aarch64-pc-windows-msvc`. Each executable links libcmt, libvcruntime and libucrt, the
  runtime's static libraries (`/MT`'s, Microsoft's table above).
- `crates/shards/build.rs`: the nested builds of `shards-vm`, `shards-net` and
  `shards-vm-gpu` take the outer build's flags (`CARGO_ENCODED_RUSTFLAGS`, which a nested
  cargo takes in place of its configuration's) with `+crt-static` added, whatever those say,
  as Linux's helpers are musl's whatever `shards` links. shards-init is a Linux guest's
  (musl) and takes none of them.
- C follows the flag. cc-rs 1.5.1 compiles `-MT` where `CARGO_CFG_TARGET_FEATURE` holds
  `crt-static`, else `-MD` (`src/lib.rs`, the MSVC branch of its flags). AWS-LC
  (vendor/aws-lc-sys) is compiled by cc-rs where its builder's needs are met (`get_builder`;
  NASM on x86_64 Windows), else by CMake, whose runtime it sets from the same variable
  (`is_crt_static`, `CMAKE_MSVC_RUNTIME_LIBRARY`). `scripts/gpu-libs` compiles the
  renderer, libepoxy and the identity bridge with `-Db_vscrt=mt`: every object of a link
  must use one runtime, and meson's default follows a project's buildtype (`/MD`; 1f86d092
  named `/MD` for the bridge to match Rust's DLL runtime then,
  [hex-upstream-v1](../hex-upstream-v1/README.md)).
- Each executable holds its own copy of the runtime: a fix to it reaches users in a build of
  shards, not through the redistributable's servicing. Microsoft's caution about several
  runtimes in one process concerns runtime objects passed between images; each of shards'
  executables is a process of its own, and the GPU's Vulkan loader, a DLL it opens, keeps
  its own runtime as it did.

## Before: main, 9e649df34e, on jupiter

Windows 10.0.26200, rustc 1.98.1, NASM 3.02, Visual Studio 2022 Build Tools (MSVC
14.44.35207); `crt-build.sh build` of a fresh clone, 25 min 31 s.

| Executable | Bytes | Imports of the C runtime |
| --- | --- | --- |
| `shards.exe` | 40,334,848 | `VCRUNTIME140.dll`; `api-ms-win-crt-` math, string, runtime, convert, environment, stdio, heap, utility, locale |
| `shards-vm.exe` (carried) | 2,413,568 | `VCRUNTIME140.dll`; `api-ms-win-crt-` runtime, math, stdio, locale, heap |
| `shards-net.exe` (carried) | 2,851,328 | `VCRUNTIME140.dll`; `api-ms-win-crt-` string, runtime, convert, environment, stdio, heap, utility, math, locale |

Their other imports are Windows' own (kernel32, ntdll, advapi32, ws2_32, bcryptprimitives,
the `api-ms-win-core-*` sets and the like). AWS-LC's library
(`aws_lc_0_45_0_crypto.lib`) asks for `MSVCRT` 250 times and `OLDNAMES` 250: `/MD`. The GPU's
VM process, not built here, imported `VCRUNTIME140.dll` and the release UCRT, its libraries
asking for `msvcrt.lib` ([hex-upstream-v1](../hex-upstream-v1/README.md), S1 and S4).

## After

Owed: `crt-build.sh build` of the pushed branch on jupiter, with the GPU's renderer.
