# s33 Claude Code evidence index

POLICY_READ revision=99d88fccf6107d1929c1115eb220a732b182fd37 sha256=df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3 role=researcher(read-only) task=s33-claude-code owned=docs/research/s33-veil-creds/evidence/claude-code/

Read in full before work: AGENTS.md (sha256 f66d304c033ad4001d3d7347b67faa3bb3137d637860793e8e74f1eb6266db41), docs/development-rules.md (sha256 above), docs/development-evidence.md (sha256 61613a244106d95b2759b38c8b6660ff29a616983e7afd6e9faabdc38368fb63), worktree s33-veil-creds at origin/main 99d88fccf.

## Subject

- kiln pin: `git -C /Users/adalundhe/Projects/kiln show origin/main:.kiln/images/harness-claude/pin.json` at kiln origin/main 7a6827d8ee9be7bd10800d62132478f42d7412f5, saved as kiln-pin.json (raw) and kiln-pin.json.txt (with provenance header). Claude Code 2.1.311, channel stable.
- Binary: https://downloads.claude.ai/claude-code-releases/2.1.311/linux-x64/claude (linux/amd64, gnu), downloaded 2026-11-12T23:09:20Z-23:09:31Z into a mktemp scratch dir, 293776752 bytes, sha256 86dc1fcc69498159eaf15f446613e847010f9036bfbbdffd4c49350be8489ada = pin.json value (e_download.txt). Never executed; searched with GNU grep 3.12 (/opt/homebrew/bin/ggrep -a -o -b) and llvm-readelf -S. Deleted 2026-11-12T23:35:15Z (e_download.txt).
- Runtime: Bun v1.4.4 (0ae49f8b6) Linux x64, a bun --compile executable (.bun ELF section holds the app; e_bun_runtime.txt, e_elf_sections.txt). Commit 0ae49f8b6 is not in public oven-sh/bun (GitHub API 422); public tag bun-v1.4.4 = 38afee6c84fcf48c0edb2466aa4b27d287a59c30 (bun-source/github-*.json). Bun source quoted below is that public tag, not the exact runtime commit.

## Claude Code docs (code.claude.com, .md form, current docs as of retrieval; not version-pinned to 2.1.311)

| file | URL | retrieved (UTC) | HTTP | bytes | sha256 |
| - | - | - | - | - | - |
| docs/llms.txt | https://code.claude.com/docs/llms.txt | 2026-11-12T23:10:14Z | 200 | 58034 | d366085b7bdd639edbb94b706cfa4815462d7e6d5922747e051183baba1009d1 |
| docs/network-config.md | https://code.claude.com/docs/en/network-config.md | 2026-11-12T23:10:15Z | 200 | 35690 | f8772ba8eb25afded1255706e59abfee95ab4e3bf3ea456bbc564d32e3dc0921 |
| docs/authentication.md | https://code.claude.com/docs/en/authentication.md | 2026-11-12T23:10:15Z | 200 | 36322 | 86c28d9f9a994974ca6ad7bd6050fd0d9bc7fc5e7172ac79af7ca98bc7b92278 |
| docs/settings.md | https://code.claude.com/docs/en/settings.md | 2026-11-12T23:10:16Z | 200 | 45980 | bc72fdd5dc45fdf7ea3ce5d480628ce1fcb31aa09b6b796316f1db8ac02eb5e9 |
| docs/settings-reference.md | https://code.claude.com/docs/en/settings-reference.md | 2026-11-12T23:10:16Z | 200 | 458029 | 164320461acfc2ab93d6f7dfddb859fe3d36aef77ac9aeca28fe744d523c65fd |
| docs/env-vars.md | https://code.claude.com/docs/en/env-vars.md | 2026-11-12T23:10:16Z | 200 | 189603 | c039289e785ab7bcf45f216866c0a9447d0af8ec57dde2e9c6f03a7830c677fa |
| docs/setup.md | https://code.claude.com/docs/en/setup.md | 2026-11-12T23:10:16Z | 200 | 34487 | ead92d4891fa3354fbb377439cee8fa7db2e4488b971ab4208510d2c438a75a6 |
| docs/cli-reference.md | https://code.claude.com/docs/en/cli-reference.md | 2026-11-12T23:10:17Z | 200 | 51918 | f622315862db657768e66ba01d7d29ac205c2d8e194ede7971457d6c2d05072d |
| docs/commands.md | https://code.claude.com/docs/en/commands.md | 2026-11-12T23:10:17Z | 200 | 57335 | 7092d83ff34ae9fea6680a8e4b313df6e61f8b5328d0e18df5720fadf6e87ff4 |
| docs/data-usage.md | https://code.claude.com/docs/en/data-usage.md | 2026-11-12T23:10:17Z | 200 | 18833 | 696b41d99d39c84d8c2972ef32ba837d952f6f3d0fbbcf26362a0fde6ec23cd8 |
| docs/devcontainer.md | https://code.claude.com/docs/en/devcontainer.md | 2026-11-12T23:10:17Z | 200 | 18658 | 03ccfa8b04a8dd9d6fd12c24dafa976ee026a165f83a2b893dcbce0df4558547 |
| docs/troubleshoot-install.md | https://code.claude.com/docs/en/troubleshoot-install.md | 2026-11-12T23:10:18Z | 200 | 82168 | 391fec2a162a4b10926eb8df807e81ddc07a9028d7e7fc2ed804645d57915d0c |
| docs/llm-gateway-protocol.md | https://code.claude.com/docs/en/llm-gateway-protocol.md | 2026-11-12T23:10:18Z | 200 | 53084 | 85373bf3f6b6a9a0c8b38a5abd3820e27bd89ef6e755146957eea9f7a2a1807e |
| docs/llm-gateway-connect.md | https://code.claude.com/docs/en/llm-gateway-connect.md | 2026-11-12T23:10:18Z | 200 | 46732 | 4a1b0776f1157a5b236e915ecff59ce486580dc55910687153f3fea2403a7656 |
| docs/llm-gateway.md | https://code.claude.com/docs/en/llm-gateway.md | 2026-11-12T23:10:19Z | 200 | 7734 | 0e859ef81b786b71bfa8c31cdadae66cf5ae9d006e1519058b05ff127f81426f |
| docs/hipaa-setup.md | https://code.claude.com/docs/en/hipaa-setup.md | 2026-11-12T23:10:19Z | 200 | 23033 | 60c4ce4cc2f5a0d20df411d0e99c67109bb78915047a251f217f7914a1de5f9b |
| docs/security.md | https://code.claude.com/docs/en/security.md | 2026-11-12T23:10:20Z | 200 | 15469 | b9cbdaa3db416b1bdd759e48c367d4273947713151eebed423c95f59ff5a71db |
| docs/agent-sdk__secure-deployment.md | https://code.claude.com/docs/en/agent-sdk/secure-deployment.md | 2026-11-12T23:10:20Z | 200 | 21646 | 1e465d76bfb03b5776ff5cd80addc5bdf385236f3b20f2bfca366f9f002fb843 |
| docs/self-hosted-environments-deploy.md | https://code.claude.com/docs/en/self-hosted-environments-deploy.md | 2026-11-12T23:10:20Z | 200 | 132914 | c49e52600627ea05fd6851d8aff5d5c8ad3a2b403215bafdaa796505d959d4f9 |
| docs/cloud-environments.md | https://code.claude.com/docs/en/cloud-environments.md | 2026-11-12T23:10:21Z | 200 | 66774 | cca41467ed3797697820b11281dd792196091e04d55cfa5d9621a76a431addbe |
| docs/monitoring-usage.md | https://code.claude.com/docs/en/monitoring-usage.md | 2026-11-12T23:10:21Z | 200 | 175703 | f4e05bb2e9411b62d9dc638d574556325532d81aa999d8f28e184c89aa0b95a4 |
| docs/sandbox-environments.md | https://code.claude.com/docs/en/sandbox-environments.md | 2026-11-12T23:10:21Z | 200 | 18874 | 7b8c4af4ccc5e55f32db3b0c5adb9c64e236a534e99715bc6f04406045e4235e |
| docs/sandboxing.md | https://code.claude.com/docs/en/sandboxing.md | 2026-11-12T23:10:22Z | 200 | 111229 | f679b137a6c4d0dbe672150820af58197c058e769b6b25fb17383dbf14ab8d04 |
| docs/third-party-integrations.md | https://code.claude.com/docs/en/third-party-integrations.md | 2026-11-12T23:10:22Z | 200 | 12067 | 89c448905df6e70ef085ec34ad86c76488a5d0d8089943ca33867a3c36bb90a2 |
| docs/gateways.md | https://code.claude.com/docs/en/gateways.md | 2026-11-12T23:10:22Z | 200 | 9570 | 58e6836caa2cffc150bb3ef63fbd304565059b6a0efe60fb72b0f746db5d4c14 |
| docs/errors.md | https://code.claude.com/docs/en/errors.md | 2026-11-12T23:11:48Z | 200 | 627606 | 53e9c35f6594460ad25b771c6a1634d8a11edfdcc74da61647253ba41cbf745a |
| docs/claude-directory.md | https://code.claude.com/docs/en/claude-directory.md | 2026-11-12T23:11:49Z | 200 | 32620 | de0b1dc3607da8bd6f3e8e5a269c1326a2cb9ad5f1cb29e5411b21bcfa9e04d3 |
| docs/headless.md | https://code.claude.com/docs/en/headless.md | 2026-11-12T23:11:49Z | 200 | 37835 | 62359e4f8585a4044b217fb56416cbc3d27fdf9bc0c9963410b04fb0a416c886 |
| docs/fast-mode.md | https://code.claude.com/docs/en/fast-mode.md | 2026-11-12T23:11:50Z | 200 | 24728 | 8180a00603df26fa881dcbecf8b0d5aea2778a0ede37c64e45ed60aa0fa9fa25 |

## Bun sources (raw.githubusercontent.com at tag commit 38afee6c8, plus live bun.com docs and GitHub API records)

| file | URL | retrieved (UTC) | HTTP | bytes | sha256 |
| - | - | - | - | - | - |
| bun-source/packages__bun-usockets__src__crypto__root_certs.cpp | https://raw.githubusercontent.com/oven-sh/bun/38afee6c84fcf48c0edb2466aa4b27d287a59c30/packages/bun-usockets/src/crypto/root_certs.cpp | 2026-11-12T23:14:58Z | 200 | 15075 | 5a9dc4c694a8a71b95eb9243cf8711035c0559a4b7003fcda4776330c4fb471c |
| bun-source/packages__bun-usockets__src__crypto__root_certs_platform.h | https://raw.githubusercontent.com/oven-sh/bun/38afee6c84fcf48c0edb2466aa4b27d287a59c30/packages/bun-usockets/src/crypto/root_certs_platform.h | 2026-11-12T23:14:58Z | 200 | 536 | 6caf1f8aad8b3298e7157559ab128dd4fa96e741d808f35990169ba447bd4048 |
| bun-source/src__runtime__socket__system_certs.rs | https://raw.githubusercontent.com/oven-sh/bun/38afee6c84fcf48c0edb2466aa4b27d287a59c30/src/runtime/socket/system_certs.rs | 2026-11-12T23:14:58Z | 200 | 10146 | 95ff3e974b93ed8a9d77629343af9f933d4a0a327c1d2b86a7f2f35d0e798a95 |
| bun-source/src__js__node__tls.ts | https://raw.githubusercontent.com/oven-sh/bun/38afee6c84fcf48c0edb2466aa4b27d287a59c30/src/js/node/tls.ts | 2026-11-12T23:14:59Z | 200 | 69496 | 4dd6017064eb98a0be07b4406bbbfd6aa46a6c7073db081d7b90eaa24e5a262b |
| bun-source/docs__runtime__environment-variables.mdx | https://raw.githubusercontent.com/oven-sh/bun/38afee6c84fcf48c0edb2466aa4b27d287a59c30/docs/runtime/environment-variables.mdx | 2026-11-12T23:14:59Z | 200 | 14048 | dc1aaef5bbdcd644064fe0d6abbab881b2e6df534d942f26c42ee226a6cab234 |
| bun-source/docs__runtime__networking__fetch.mdx | https://raw.githubusercontent.com/oven-sh/bun/38afee6c84fcf48c0edb2466aa4b27d287a59c30/docs/runtime/networking/fetch.mdx | 2026-11-12T23:14:59Z | 200 | 22720 | 769aa878070c2175677e3b2ed41f8251154b32437d7bd824503f38e9add7e9e1 |
| bun-source/test__js__node__tls__test-node-extra-ca-certs.test.ts | https://raw.githubusercontent.com/oven-sh/bun/38afee6c84fcf48c0edb2466aa4b27d287a59c30/test/js/node/tls/test-node-extra-ca-certs.test.ts | 2026-11-12T23:15:00Z | 200 | 12369 | 2f2f7246c6d8f213b5db6afcc0a4379a4c718fc3a74a9124e970a554210906a3 |
| bun-source/docs__runtime__http__tls.mdx | https://raw.githubusercontent.com/oven-sh/bun/38afee6c84fcf48c0edb2466aa4b27d287a59c30/docs/runtime/http/tls.mdx | 2026-11-12T23:33:12Z | 200 | 4053 | 3c132e3d019cfa19b398357f26b06c1ce06208732f711bae0087f35c9573d8fc |
| bun-source/docs__guides__http__tls.mdx | https://raw.githubusercontent.com/oven-sh/bun/38afee6c84fcf48c0edb2466aa4b27d287a59c30/docs/guides/http/tls.mdx | 2026-11-12T23:33:13Z | 200 | 1065 | a2643c6d7c1f7227587cb1ffa6c6199f428c88d1bef4700fea42eed17f6115d2 |
| bun-source/live__bun.com__docs__runtime__environment-variables.md | https://bun.com/docs/runtime/environment-variables.md | 2026-11-12T23:33:32Z | 200 | 14571 | 7f1240059364fbd66200fa3e312d3efddcb8000f4969590dd1e8462504fac824 |
| bun-source/github-ref-bun-v1.4.4.json | https://api.github.com/repos/oven-sh/bun/git/ref/tags/bun-v1.4.4 | 2026-11-12T23:33:53Z | 200 | 382 | 42952838d8763e0df8059f06ac80054e7d9827b926db22b86edad2aaa45c607a |
| bun-source/github-commit-0ae49f8b6.json | https://api.github.com/repos/oven-sh/bun/commits/0ae49f8b6 | 2026-11-12T23:33:53Z | 422 | 157 | 09f2b44ec06d3b172879e8fa825cf91e14db7d117e5af60d90bf853a087a66a1 |
| bun-source/github-compare-bun-v1.4.4...0ae49f8b6.json | https://api.github.com/repos/oven-sh/bun/compare/bun-v1.4.4...0ae49f8b6 | 2026-11-12T23:33:54Z | 404 | 139 | 626dd32fc1fe4c61c78b21508d943983c358d5640304ffe568cbcb0916e4711a |

## Search logs (each output line: offset:match; the '## command:' line above each output is the exact command; cmds/*.cmds hold the commands, cmds/runner.sh ran them)

| file | what | sha256 |
| - | - | - |
| e_download.txt | download, size and sha256 check, deletion record | fd1149ca360e8ba4cbef71ad3756b05e46cd28ba3b84bb1bdac03dc424d71a25 |
| e_elf_sections.txt | ELF section headers | cef6a202a3e1b166e8998b5a64bc8c3df49ca7c17c7982564fb0c9c3828739d3 |
| e_bun_runtime.txt | Bun version string, bunfs paths, trailer | 5aa7c663f44bff517cf8ba87cc05739bd7c4060339f40a4d43ffb171504c913c |
| e_q1_ca.txt | CA store selection, NODE_EXTRA_CA_CERTS, deny list, chain filter, fetch tls.ca, Bun builtins | 52a59db0a931c86263e1bf9adffca0084e5c940bc703dee6ba8a896ae44e1aeb |
| e_q2_creds.txt | .credentials.json path/mode, apiKeyHelper TTL, env token sources | 812e7d51caf75652200b90a4372a3ebca9917a86bb8ba10214c2571cf2cbd1bb |
| e_q3_signin.txt | OAuth config, PKCE authorize URL, token exchange, stored fields, device-code contexts | 815ee30a203ea6d3a1b5c3e595dee66b8f7f9ba655b8edfc39a60c54dfdc3633 |
| e_q4_refresh.txt | expiry check, refresh request/response, revoke, lock, 401 path | e8ee7a4465feccdd1874a7430895b3ff05288c141f5c4928a96d3865d6ea0c2b |
| e_q5_wire.txt | X-Api-Key vs Authorization Bearer, oauth beta | 27aa8e6d3aa6b0717fd0de7b939b653ea636ee8ce9b847134980c184be0742d0 |
| e_q6_hosts.txt | hosts and endpoints, telemetry, feature flags, opt-out variables | 83fd4135a3850633e602f2cf63d906ec8defe9040c40d5891942877ec76457b4 |
| e_q7_pinning.txt | pinning searches (gateway-only TOFU pin) | dd8eb0a457c3a987633b7d532ba5f58dd950d88b849ea74775be59064d9cb906 |
| e_q8_transport.txt | HTTP/2 flag, SSE, WebSocket, gzip decision | 1cfd06cd5a3c76f2ec653d67a2ceb55aca353ab2064485d34beb34e818d46c7c |
| e_extra_sandbox_mask.txt | sandbox masked-credential proxy, undocumented host-auth variables | ed5d60d4407efab1bb940f7c2ae254579e71c7367acf6bd48fe9cf05428e72e8 |
| e_docs_searches.txt | searches over the saved docs (pinning, SSL_CERT_*, NODE_EXTRA_CA_CERTS, statsig/sentry) | bdc95851609ec59c107d56de21b515918e5c1c4595954ac9a6f3321e05408520 |
| kiln-pin.json |  | b98f01ae3d9632c51091c885ff874a36ef3ac8866215ae93041f9eae990bd10c |
| kiln-pin.json.txt |  | 01f54d0bc8463e317bc72a729329a755a457afb12fadf0a95509c9d99b08563c |
| cmds/runner.sh |  | 5eaccbb8db362efaa64169ed8c8742e8f50d447caebf7845f17459db0698615f |
| cmds/q1_ca.cmds |  | a211feb3a65052fe52fc8d65e3ad5751d4b72c50233882c1f0bf8913481658c0 |
| cmds/q2_creds.cmds |  | 600ad5b7d4dabe48988866a9bc02658cee6e3ed47d08e0a9e88c920fe902dd4e |
| cmds/q3_signin.cmds |  | cdd21340a8d3c1cc1cf22c938a1dbcf8f2a271b13b009ab86ceb4c3345a11f50 |
| cmds/q4_refresh.cmds |  | 0da4c5faf44bd890bf10aac347982059a006a5746fda161d34a2d87b11738989 |
| cmds/q5_wire.cmds |  | eb3c541d069eee6ca33127ed483d9bac47b68aa1a38a80dc9ed2046617099e4a |
| cmds/q6_hosts.cmds |  | 5bf00c6f2b7975c80dce834b44e13c03196943834020109e5dc7c74c16db7af1 |
| cmds/q7_pinning.cmds |  | 79fbab3416ae20788986ed24cee2e0f7374b0af601d64627058d38d1ad3132dc |
| cmds/q8_transport.cmds |  | 78049c7f268b514759f4f266165dd47fd7313688cc84e975aab8810f1a9c9a68 |
| cmds/extra_sandbox_mask.cmds |  | 0f8b7612fa2ff349004c4059256f3cc55d4646b1b5f78361bd0c6e8776a0fea9 |
| docs/fetch-log.tsv |  | 3b8cb2fd22a2403ff475bfff17fed7b157ef186b045821e1d7cbb6b1775e4003 |
| bun-source/fetch-log.tsv |  | 545413496ee3667f89863fb7681322774dd3a85b0b2b50d0e1ca4495cf3d135d |

## Deviations and caveats

- A 64-byte note holding only the scratch path was written to /tmp/s33cc_scratch_path.txt at 23:09Z and deleted within the minute (outside the owned path; no content beyond the path).
- Besides the binary's scratch dir, two more mktemp dirs held llms.txt and GitHub API responses briefly, and a third held one bun.com page; all were deleted (e_download.txt). Every kept copy is in this directory.
- Docs describe the latest Claude Code (several entries name v2.1.318/2.1.319), so docs claims are checked against the 2.1.311 binary where possible; binary evidence is minified JS, names are per-chunk.
