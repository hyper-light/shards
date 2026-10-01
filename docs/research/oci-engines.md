# Other engines: could Docker, Compose or Kubernetes run shards microVMs?

Research note, 2026-10-01, for AGENTFILE_ARCH.md Q15. The user's condition: other engines
run shards microVMs only if every guarantee the spec defines holds fully; otherwise shards'
own runtime alone runs them. Primary sources only, cited inline; versions as given.

## How the engines run runtimes other than runc

- **Docker.** `daemon.json` `runtimes` names a containerd shim (`runtimeType:
  io.containerd.X.v2`) or a runc-compatible binary, chosen with `docker run --runtime`
  [docs.docker.com/engine/daemon/alternative-runtimes/]; Compose has `runtime:`
  [compose-spec 05-services.md]. containerd maps `io.containerd.X.v2` to
  `containerd-shim-X-v2`, whose Create gets the bundle (`config.json`, `rootfs/`), the
  snapshotter's mounts, stdio and options [containerd docs/runtime-v2.md].
- **Docker owns the network, and sets it up after the runtime starts.** moby asks for a
  new network namespace [moby 0fed273: daemon/oci_linux.go:242-270], creates the task, then
  `initializeCreatedTask` joins it to libnetwork's veth [daemon/start_linux.go:17-41,
  called at daemon/start.go:248 between `NewTask` and `Start`]. Kata rescans the network
  asynchronously because "Docker 26+ configures networking after the Start response"
  [kata 2ebcc67: src/runtime/pkg/containerd-shim-v2/start.go].
- **`-p` never reaches the runtime.** Port publishing is DNAT and docker-proxy in the
  host's namespace [moby: daemon/libnetwork/drivers/bridge/port_mapping_linux.go:26;
  internal/iptabler/port.go:84]; it is not in `config.json`.
- **Kata Containers** runs one VM per pod sandbox, its VMM in the namespace Docker or CNI
  made, the veth redirected to a tap by TC or macvtap [kata: docs/design/architecture/
  networking.md]. Its limits include no host networking, no checkpoint/restore, no
  Podman [docs/Limitations.md]; rootless only for QEMU, off by default
  [src/runtime/config/configuration-qemu.toml.in].
- **firecracker-containerd** is driven by its own `firecracker-ctr`, needs devmapper, runs
  runc inside the guest, and uses CNI with tc-redirect-tap under `sudo`
  [firecracker-containerd docs/architecture.md, networking.md, getting-started.md].
- **gVisor** runs its own stack but on the device "inside the network namespace setup by
  Docker or Kubernetes" [gvisor.dev/docs/user_guide/networking/].
- **Docker's own microVMs, Docker Sandboxes,** are no Docker runtime: a separate `sbx`
  CLI, a Docker daemon inside each sandbox, policy enforced in a host proxy
  [docs.docker.com/ai/sandboxes/architecture/].
- **Kubernetes.** RuntimeClass maps a handler to a containerd runtime
  [kubernetes.io/docs/concepts/containers/runtime-class/]; containerd's CRI plugin makes
  the pod's namespace and runs CNI itself [containerd df742f8:
  internal/cri/server/sandbox_run.go:196-217, 456]; NetworkPolicy is the CNI plugin's,
  per pod, allow-all by default [kubernetes.io/docs/concepts/services-networking/
  network-policies/].
- **Images.** containerd 2.1 added an EROFS snapshotter, one blob per layer [containerd
  docs/snapshotters/erofs.md; PR 10705]; moby has no EROFS support.
- **macOS.** Docker Desktop runs its engine in its own Linux VM
  [docs.docker.com/desktop/features/vmm/]; a shards runtime there would be a nested KVM
  guest, which needs macOS 15 and hardware that allows it
  [Virtualization.framework VZGenericPlatformConfiguration.h:36-57;
  Hypervisor.framework hv_vm_config.h:67-90], and Docker documents no `/dev/kvm` in it.
- **Builds.** BuildKit runs RUN through `worker.oci.binary` or `worker.containerd.runtime`
  [buildkit docs/buildkitd.toml.md]; a `# syntax=` frontend's RUN steps run under whatever
  worker the builder has.

## What a shim integration would keep

| Guarantee | Docker, Compose | Kubernetes | Why |
|---|---|---|---|
| VM-process confinement (D30) | partial on Linux, none on macOS | partial | shards-vm's seccomp and Landlock survive, but its parent becomes a shim containerd starts, usually as root; the network process would feed the engine's veth, outside its design; App Sandbox does not exist inside Docker Desktop's Linux VM |
| Deny-by-default networking (`EXPOSE`, `NETWORK`, `CONNECT`, D31) | no | no | the engine makes the namespace and veth, `-p` is host DNAT the shim never sees, networks are dockerd's or CNI's; a shim sees interfaces, not network names |
| Many agents per VM, scoped volumes, skills, MCP | mostly | mostly | inside the VM all shards' own; the engine sees one container or pod, so its exec, logs and stats address no agent |
| Warm pools, < 5 ms starts (D26) | unmeasured, the path not shards' | same | dockerd → containerd → shim → Create → network → Start; a restored VM's network comes only after Create |
| EROFS on pmem (D15) | partial | partial | the engine hands overlay mounts; the shim would ignore them for shards' own store |
| Rootless | no, by default | no | dockerd and containerd run as root; rootless Docker has its own network limits |

## Conclusion

The networking, the macOS confinement and rootless operation fail by the engines' design,
not for want of work: no shim can veto what the engine does on the host. By Q15's
condition, shards microVMs run through shards' runtime alone (architecture.md D32).
Interoperability stays where it costs no guarantee: registries (push, pull, tags,
inspect), the `# syntax=` BuildKit frontend for Agentfiles (Q16), and Compose files read by
shards.
