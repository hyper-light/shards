# Rootless networking for shards microVMs and the in-VM Docker/Compose engine

Research note, 2026-09-28. Every factual claim carries a tag; the tags resolve in §5.

- `[Name YY §x]`: peer-reviewed paper, read in full text.
- `[RFC n §x]`, `[VIRTIO-1.3 §x]`: specifications.
- `[man: …]`, `[Apple: …]`, `[docker-docs: …]`: official documentation.
- `[repo:path:line]`: source code at the commit pinned in §5.
- "project-reported" marks numbers from project documentation, which are not peer-reviewed.
- `[Brooker21]` is an arXiv preprint and is not peer-reviewed.

## 1. Scope

This note answers six questions:

1. **Rootless host networking.** On macOS: vmnet modes and the entitlement/root rules, the userspace stacks (libslirp, passt/pasta, gvisor-tap-vsock), and libkrun TSI. On Linux: TAP and CAP_NET_ADMIN, pasta/slirp4netns, and vhost-net.
2. **Published evidence on costs.** Peer-reviewed data on network-setup latency and data-path overhead: namespaces, veth, TAP, userspace TCP stacks.
3. **Unprivileged in-VM networking.** What an unprivileged engine can do inside its own user and network namespaces, and what a root PID 1 must set up once.
4. **Docker/Compose semantics.** The network behavior that must be copied exactly.
5. **Design options and their costs.** Host→VM port publishing, egress NAT, VM↔VM traffic, container↔container traffic, virtio-net vs vsock, snapshot clones, and the ≤5 ms start budget.
6. **Isolation policy model and enforcement** (added to scope). Policy per microVM and per container, where each rule type must be enforced, the pitfalls of DNS-name rules, and a critique of go-microvm.

## 2. Findings

### 2.1 Host side without root (Q1)

**macOS: vmnet cannot be used rootless.**

- **Modes.** vmnet offers three modes [SDK vmnet.h:46-60,79-101]:
  - host mode: the VM talks to the host and to other host-mode interfaces;
  - shared mode: NAT to the Internet, plus the host and other shared-mode interfaces;
  - bridged mode.
- **macOS 26 network objects.** `vmnet_network_configuration_create` defaults to NAT44, NAT66, DHCP, a DNS proxy and router advertisements, on a /24 under 192.168/16 [vmnet.h:1008-1040]. Other options:
  - a per-interface isolation key [vmnet.h:385-396];
  - TSO, checksum and virtio-header offload keys [vmnet.h:376-432];
  - host-mode networks keyed by identifier, which provide "No DHCP service" [vmnet.h:332-345].
- **Entitlement rule.** Apple: "A sandboxed user space process must have the com.apple.vm.networking entitlement in order to use the vmnet API" [Apple: vmnet]. The entitlement lets an app manage interfaces "without escalating privileges to the root user". It "is restricted to developers of virtualization software" [Apple: com.apple.vm.networking].
- **Root otherwise.** QEMU's built-in vmnet "requires running the entire QEMU process as root" [socket_vmnet README] (project docs).
- **macOS 26 VZ attachment.** `VZVmnetNetworkDeviceAttachment` accepts only a network created by the same process, and "the vmnet framework requires an entitlement to create or configure a network" [SDK VZVmnetNetworkDeviceAttachment.h:30-36]. VZ bridged mode needs the same entitlement [VZBridgedNetworkDeviceAttachment.h:22].
- **Hard limits.** The guest must use the private IPv4 address that DHCP assigned; "the system drops packets sent from a different IPv4 address". There is a "maximum of 32 interfaces with a limit of 4 per guest". Each read or write call carries at most 200 packets / 256 KB [Apple: vmnet].
- **Apple's own model for third-party stacks.** `VZFileHandleNetworkDeviceAttachment` carries raw L2 frames over a connected datagram socket, with MTU 1500–65535 [VZFileHandleNetworkDeviceAttachment.h:13-49].
- **Privileged ports on macOS.** xnu requires `PRIV_NETINET_RESERVEDPORT` for ports below 1024 only when the bind address is not the wildcard [xnu bsd/netinet/in_pcb.c:989-1003; bsd/netinet6/in6_pcb.c:373-379].
  - An unprivileged VMM can bind `0.0.0.0:80`.
  - It cannot bind `127.0.0.1:80`.

**Userspace stacks.**

- **libslirp.** A "user-mode networking library used by virtual machines, containers or various tools". It needs glib2, and "there are no automated tests available" [libslirp README].
- **passt/pasta.**
  - It translates between L2 frames and host L4 sockets, and "doesn't require any capabilities or privileges" [passt: about].
  - Its TCP layer "has no stateful data buffering and operates by reflecting one peer's observed parameters (congestion window size, acknowledged data, etc.)" [man: passt(1) DESCRIPTION].
  - It does no dynamic memory allocation. Its seccomp profiles allow 34 syscalls (passt) and 43 (pasta). vhost-user mode gives "maximum one copy on every data path" (project-reported) [passt: about].
  - **Linux only.** Darwin and FreeBSD are listed as not yet supported [passt: about, Portability].
  - **Unsafe default.** By default the guest's gateway address is mapped to host loopback (`--map-host-loopback`); `--no-map-gw` turns this off [man: passt(1)].
- **gvisor-tap-vsock.**
  - A pure-Go replacement for libslirp and VPNKit, with DHCP, DNS and dynamic port forwarding [gvisor-tap-vsock README L4-9,134-160].
  - The host process "uses regular syscalls to connect to external endpoints". ICMP is not forwarded [README L184,200-202].
  - Throughput is 1.6–2.3 Gbit/s at MTU 4000 on macOS (project-reported) [README L188].
  - Its TCP forwarder refuses 169.254/16 unless `Ec2MetadataAccess` is set; otherwise it calls `net.Dial` for any destination [v0.8.9 pkg/services/forwarder/tcp.go:18-34].

**libkrun TSI (Transparent Socket Impersonation).**

- **Mechanism.** The VM gets no NIC. The guest's AF_INET, AF_INET6 and AF_UNIX socket calls are forwarded over vsock and carried out by the VMM.
  - It needs a custom guest kernel.
  - It supports only STREAM and DGRAM sockets, no raw sockets, and no DGRAM listen [libkrun README:66-83].
  - The operations are PROXY_CREATE, CONNECT, LISTEN and ACCEPT over vsock port 620 [vsock/mod.rs:86-94].
  - It relies on a non-standard `VIRTIO_VSOCK_F_DGRAM` bit [mod.rs:112]. VIRTIO 1.3 defines only STREAM, SEQPACKET and NO_IMPLIED_STREAM [VIRTIO-1.3 §5.10.3].
- **Behavior.**
  - A guest `listen()` binds a host socket at the address the guest asked for; the port is remapped only if it appears in the port map [tsi_stream/unix.rs:87-130].
  - A guest `connect()` becomes a host `connect()` with no policy hook [unix.rs:270-312].
- **Security model.** libkrun's README says to treat the VMM and guest as one network context, and to apply on the VMM whatever restrictions you want to apply on the guest [README:109-111].

**Linux.**

- **TAP needs privilege or one-time setup.**
  - Creating a TUN/TAP device needs `ns_capable(net->user_ns, CAP_NET_ADMIN)` [linux drivers/net/tun.c:2827-2828].
  - An existing persistent device whose owner is a given uid can be attached by that uid without the capability [tun.c:515-523]. The kernel doc: "CAP_NET_ADMIN is required for creating network devices or for connecting to network devices which aren't owned by the user" [Documentation/networking/tuntap.rst:56-61].
  - So a rootless VMM gets a TAP only in two ways: a one-time root setup, or its own unprivileged user+net namespace, which is what pasta does.
- **vhost-net is not a root escape.** The kernel sets no mode on `/dev/net/tun` or `/dev/vhost-net` [tun.c:3550-3555; vhost/net.c:1885-1889]. systemd's udev rules make tun 0666, and vhost-net `GROUP="kvm", MODE="{{DEV_KVM_MODE}}"` with a default of 0666 [systemd 50-udev-default.rules.in; meson_options.txt]. But vhost-net's backend must be a TAP or a packet socket [vhost/net.c:1510-1536], so TAP remains the gating step.
- **RootlessKit drivers and throughput** (project-reported, GitHub Actions, iperf3 child→parent, MTU 1500 / 65520) [rootlesskit docs/network.md]:

  | Driver | MTU 1500 | MTU 65520 |
  |---|---|---|
  | slirp4netns | 1.69 Gbps | 8.11 Gbps |
  | pasta | 0.24 Gbps | 31.9 Gbps |
  | gvisor-tap-vsock | 2.46 Gbps | 8.75 Gbps |
  | lxc-user-nic (SUID) | 49.1 Gbps | 50.7 Gbps |
  | rootful veth | 49.3 Gbps | 50.8 Gbps |

  Ubuntu's `kernel.apparmor_restrict_unprivileged_userns` can break pasta [same doc].
- **Privileged ports and ping.** `ip_unprivileged_port_start` defaults to 1024 and is per network namespace [Documentation/networking/ip-sysctl.rst:1649-1656]. `ping_group_range` defaults to "1 0", which means "nobody (not even root)" may create ping sockets [ip-sysctl.rst:1707-1712].
- **Firecracker's reference design is not rootless.**
  - It uses TUN/TAP [Agache20 §3]; TAP is its only backend [firecracker docs/network-setup.md:9-10].
  - Setup uses `sudo ip tuntap add` plus nft NAT [network-setup.md:56-98].
  - The jailer joins a network namespace and then drops privileges [docs/jailer.md:145-154].

### 2.2 Peer-reviewed evidence on setup latency and overheads (Q2)

- **SOCK** [Oakes18 §2.2, Fig. 3-4; §4.1]. Measured on Linux 4.13.
  - Network namespaces scale poorly because a single global lock is held during creation and during RCU-delayed cleanup.
  - Container churn peaks at about 200 containers/s with network namespaces, over 400/s with optimizations, and 900/s with no network namespace at all.
  - SOCK therefore drops network namespaces; requests arrive over Unix sockets instead.
- **Agile Cold Starts** [Mohan19 §4 Table 1; Abstract]. Measured on Linux 4.4.
  - Creating 1/10/50/100 concurrent namespaces (netns + veth + IP) takes 0.28/1.27/6.28/14.41 s; cleanup takes 0.20/0.71/3.24/7.77 s.
  - A pre-created pool of "pause containers" cuts startup by about 80% at 100 concurrent containers.
- **Particle** [Thomas20 §1 Fig. 1; §2.2 Tables 2-3; §2.3]. Measured on Linux 5.0.
  - Network setup is 66–84% of startup.
  - 100 namespaces take 10.02 s and 1600 take 119.79 s.
  - Moving veth devices between namespaces is 91.7% of data-plane setup (47.71% + 43.95%), because `dev_change_net_namespace` runs notifier, unregister and flush work.
  - Particle's design (shared namespaces, batching, IP pooling, veth consolidation) cuts network setup 32× versus Docker Swarm [Thomas20 Fig. 1, §3.1].
- **Firecracker** [Agache20].
  - Adding a statically configured network interface adds about 20 ms to boot [§5.1].
  - VMM memory overhead is about 3 MB [§5.2, Fig. 7].
  - virtio-net over TAP reaches 15.61 Gb/s RX on a single stream at MTU 1500, versus 44.14 Gb/s on host loopback [§5.3, Table 1].
  - "The virtio-based approach … will not yield the near-bare-metal performance offered by PCI pass-through" [§5.3].
  - Firecracker's own docs report 25 Gbps with 6 streams and an average of 0.06 ms added latency (project-reported) [firecracker docs/network-performance.md].
- **Host stack overheads** [Cai21 §1, §3.1 Fig. 3, §3.7 Fig. 10].
  - About 42 Gbps per core with all offloads.
  - Data copy is over 50% of cycles.
  - TSO/GRO and jumbo frames cut the per-byte overhead.
  - For 4 KB RPCs, TCP/IP processing and scheduling costs rise.
- **Slim** [Zhuo19 §2.2.2 Table 2; §1; §6.2.1].
  - Same-host overlay traffic costs 23% of throughput and adds 34% RTT, because each packet traverses the stack an extra time.
  - Virtualizing at connection level brings throughput to within 3% of host mode, but makes connection setup 106% longer.
- **mTCP** [Jeong14 Abstract]. Batched packet I/O, per-core design and shared-memory events give 25× Linux throughput on short transactions.
- **TAS** [Kaufmann19 Abstract, §1]. Splitting a streamlined fast path from a slow path gives up to 90% more throughput and 57% lower tail latency than IX.
- **gVisor netstack** [Young19 §3.4]. On 1 GB downloads it reached 34% of runc's throughput (KVM platform) and 54% (ptrace); small downloads were comparable.
- **Caveat.** All namespace costs above come from Linux 4.4–5.0. None is valid for Linux 7.x without re-measurement (E3).

### 2.3 In-VM rootless container networking (Q3)

**What rootless Docker does.**

- The daemon and containers run inside a user namespace created by RootlessKit. No SETUID binaries are used except `newuidmap` and `newgidmap` [docker-docs: rootless/_index "How it works"].
- Known limitations [docker-docs: rootless/troubleshoot "Known limitations"]:
  - overlay networks are not supported;
  - `IPAddress` is namespaced and not reachable from the host;
  - `-p` does not propagate source IPs by default;
  - privileged ports need a sysctl or a capability.
- Source-IP propagation requires `userland-proxy: false` [same doc, "Networking errors"].

**What an unprivileged process can do in its own namespaces.**

- **Ownership rule.** A non-user namespace is owned by the user namespace of its creator. "Privileged operations on resources governed by the nonuser namespace require … capabilities in the user namespace that owns" it [man: user_namespaces(7)].
- **Capabilities at creation.** A `CLONE_NEWUSER` child "starts out with a complete set of capabilities in the new user namespace" [same man page].
- **What stays out of reach.** Anything not owned by a namespace, such as loading kernel modules, needs privilege in the initial user namespace [same man page].
- **Consequences in the kernel.**
  - rtnetlink checks `net->user_ns` for link, address and route changes [linux net/core/rtnetlink.c:7003].
  - Creating a link in, or moving it into, another namespace also needs the capability in the target namespace's owning user namespace [rtnetlink.c:3970-3979,4152].
  - nftables uses `netlink_net_capable`, which is scoped to the namespace [net/netfilter/nfnetlink.c:659; net/netlink/af_netlink.c:899-902].
  - So an engine that owns a user namespace can create bridges, veths and nftables rules in network namespaces it owns, with no root.
  - The bridge, veth, nf_tables and br_netfilter code must be built into shards' guest kernel, because modules cannot be loaded.
- **What a network namespace isolates.** Devices, IP stacks, routes, firewall rules, ports and `/proc/sys/net` [man: network_namespaces(7)]. veth pairs "immediately" deliver to the peer and can be created straight into other namespaces [man: veth(4)].
- **vsock bypass.** In the 7.2 tree, vsock has per-namespace modes. PID 1 can write `child_ns_mode=local` once (it locks). After that, new namespaces "can connect only to VMs or other sockets within their own namespace" [Documentation/admin-guide/sysctl/net.rst:510-560; net/vmw_vsock/af_vsock.c:95-135]. Docker's default seccomp profile already denies `socket(AF_VSOCK=40)` [moby/profiles seccomp/default.json; linux include/linux/socket.h:246].
- **eBPF enforcement needs help from root.**
  - The cgroup socket-address, socket and sockopt program types need CAP_NET_ADMIN [kernel/bpf/syscall.c:2840-2861,3043].
  - That capability is checked against `init_user_ns` unless a BPF token is present [kernel/bpf/token.c:17-25].
  - Tokens come from a bpffs instance that has `delegate_*` options, and cannot be created in `init_user_ns` [token.c:140-157].
  - So an unprivileged engine can use cgroup-BPF enforcement only if PID 1 delegates it.

**Pre-provisioning by PID 1.** Everything that needs initial-namespace privilege can be done once by a root PID 1 before the engine starts. Examples: writing `uid_map`, handing a NIC to the engine's network namespace, setting vsock `child_ns_mode`, and setting up bpffs delegation. After that the engine needs no root. Firecracker's jailer follows the same "privileged setup, then drop" pattern [docs/jailer.md:145-154].

### 2.4 Docker/Compose semantics to replicate (Q4)

**Default bridge vs user-defined networks.**

- A bridge network by default allows access from the host and from containers on the same network [docker-docs: drivers/bridge intro]. It blocks access from other networks and from outside, masquerades egress, and publishes ports on host addresses.
- **DNS.** Only user-defined networks get name and alias resolution; the default bridge needs the legacy `--link` [docker-docs: bridge "Differences"].
- **Limits and options.**
  - Networks with 1000 or more containers become unstable [docker-docs: bridge "Connection limit"].
  - `enable_icc`, `enable_ip_masquerade`, `host_binding_ipv4` and `mtu` are per-network options [docker-docs: bridge "Options"].

**Isolation as implemented (nftables backend).** For each bridge there are forward-in and forward-out chains [moby drivers/bridge/internal/nftabler/network.go:60-215]:

- established and related connections are accepted;
- ICC is accept or drop;
- an internal network drops anything whose input or output interface is not its own bridge;
- non-internal networks accept outgoing traffic, drop unpublished ports ("UNPUBLISHED PORT DROP") and masquerade.

**Embedded DNS.**

- Address 127.0.0.11, with no IPv6 equivalent. External names are forwarded to the host's resolvers. Custom networks use it; the default bridge gets a copy of `/etc/resolv.conf` [docker-docs: network/_index "DNS services"].
- **Mechanism** [moby: resolver.go:148-170; resolver_unix.go:36-41,95-132; sandbox_dns_unix.go:27]:
  - the resolver runs in the daemon;
  - it listens on UDP and TCP on 127.0.0.11 at a random port inside the container's namespace;
  - DNAT/SNAT rules in that namespace map port 53 to it.
- **Answers and forwarding** [resolver.go:57,462-470,515-527,483-509; sandbox.go:552-563]:
  - internal answers have TTL 600;
  - at most 3 upstream servers are tried;
  - single-label names fail when `ndots` is set;
  - external queries are dialed from inside the container's namespace;
  - for internal networks, nothing is forwarded via the host's loopback resolvers.
- **Name scope.** A name resolves only on the container's own networks: `name`, `name.network`, and aliases first [sandbox.go:447-517].

**Internal networks.**

- "Containers on an internal network may communicate between each other, but not with any other network". The gateway IP (host services) remains reachable [docker-cli: network_create.md:187-193].
- Gateway mode `isolated`, which is valid only with `--internal`, puts no address on the bridge [docker-docs: port-publishing "Gateway modes"].
- Compose: `internal: true` creates "an externally isolated network" [compose-spec 06-networks "internal"].

**Publishing ports.**

- Forms: `-p [ip:]host:container[/udp]`. With no host IP, the port is published on `0.0.0.0` and `[::]`. Unpublished ports are blocked for IPv4 and IPv6 [docker-docs: port-publishing].
- **Userland proxy.** `--userland-proxy` defaults to true and handles loopback traffic [docker-cli: dockerd.md:115].
- **Without the proxy (hairpin mode)** [moby: bridge_linux.go:199,1185-1190; setup_ipv4_linux.go:78-90; nftabler.go:190-198]:
  - the bridge ports get hairpin mode;
  - `route_localnet` is enabled on the bridge;
  - the NAT output chain skips `127.0.0.0/8` only when the proxy runs.
- **Gateway modes.** `nat`, `nat-unprotected`, `routed` and `isolated` [docker-docs: port-publishing].

**`none` and `host`.**

- `none`: only loopback exists, with no IPv6 loopback address [docker-docs: drivers/none].
- `host`: the container shares the host namespace, and `-p` is ignored with a warning [docker-docs: drivers/host].

**IPv6.**

- Enabled per network with `--ipv6`; a ULA subnet is used when none is given. The default bridge needs `ipv6` plus `fixed-cidr-v6` in `daemon.json`. `ip6tables` is on by default [docker-docs: engine/daemon/ipv6; bridge "IPv6"].

**Compose.**

- Services with no `networks` join the implicit `default` network [compose-spec 06-networks "default"; 05-services "implicit default network"].
- Network attributes include `aliases` (network-scoped; shared aliases resolve nondeterministically), `ipv4_address`/`ipv6_address` (need a matching `ipam`), `mac_address`, `gw_priority`, `priority` and `interface_name` [05-services §networks].
- `network_mode: none | host | service:{name} | container:{name}` cannot be combined with `networks` [05-services §network_mode].
- `external: true` means the network must exist; any attribute other than `name` is invalid [06-networks §external].
- `ipam` accepts `subnet`, `ip_range`, `gateway` and `aux_addresses` [06-networks §ipam].
- `ports` short and long syntax; with no `host_ip` the port binds `0.0.0.0` [05-services §ports].

### 2.5 Transports, snapshot clones and the start budget (Q5 facts)

**Transports.**

- vsock is "a zero-configuration socket communications device … without using the Ethernet or IP protocols" [VIRTIO-1.3 §5.10].
- On `TRANSPORT_RESET`, the driver must shut down existing connections, while listen sockets keep working [VIRTIO-1.3 §5.10.6.7]. Firecracker sends this event at snapshot time, so vsock connections do not survive a restore [firecracker docs/snapshotting/snapshot-support.md:56-61,674-686].
- virtio-net offers `GUEST_ANNOUNCE`, which lets the device ask the driver to send gratuitous packets after a migration [VIRTIO-1.3 §5.1.3, §5.1.6.5.4].
- Firecracker advertises checksum, TSO and UFO in both directions, plus `MRG_RXBUF` [firecracker src/vmm/src/devices/virtio/net/device.rs:295-319]. It also embeds a minimal userspace TCP/HTTP stack ("dumbo") for its metadata service (MMDS) [src/vmm/src/dumbo/mod.rs:4-5].

**Clones.**

- Clones restore with the same TAP names and the same guest IPs. Firecracker's fix is one network namespace plus NAT per clone [docs/snapshotting/network-for-clones.md:12-20]. Its snapshot-load API accepts `network_overrides` [src/vmm/src/vmm_config/snapshot.rs:118,155].
- Cloning "is not compatible with … TCP and TLS", and reconnecting adds to restore latency [Brooker21 §1] (preprint).
- **TCP sequence-number key.** Linux derives initial TCP sequence numbers and port offsets from one `net_secret` key, generated once [net/core/secure_seq.c:22-28]. The VM generation-ID reseed does not re-key it [drivers/virt/vmgenid.c:35], so all clones share the key. RFC 6528 requires that F() "MUST NOT be computable from the outside" [RFC 6528 §3].

**Start-budget hazards.**

- **DHCP.** A client "SHOULD wait a random time between one and ten seconds" at startup [RFC 2131 §4.4.1].
- **ARP conflict detection.** PROBE_WAIT is 1 s and ANNOUNCE_WAIT is 2 s [RFC 5227 §1.1].
- **IPv6 duplicate-address detection.** An address stays tentative for RetransTimer (1000 ms) after its probes [RFC 4862 §5.4; RFC 4861 §10]. It can be disabled with `accept_dad=0`, or shortened with optimistic DAD [ip-sysctl.rst:3055-3063,3145-3155; RFC 4429].
- **Kernel `ip=` autoconfiguration.** It sleeps 10 ms unconditionally (`CONF_POST_OPEN`) and waits for carrier [net/ipv4/ipconfig.c:86,103,1514].
- **Precedent.** go-microvm configures its guest with netlink calls and a static address, not DHCP [go-microvm guest/netcfg/netcfg.go].

### 2.6 Isolation policy model and enforcement (scope extension)

**Reference semantics.**

- **Docker.**
  - `--network none` leaves only loopback [docker-docs: drivers/none]. Compose `network_mode: none` "turns off all container networking" [compose-spec §network_mode].
  - `--internal` is not total isolation, because the gateway and host stay reachable (§2.4).
- **Kubernetes NetworkPolicy** [k8s-docs: network-policies, sections "two sorts of pod isolation", "Network traffic filtering", "What you can't do", "impact on existing connections"]:
  - pods are unrestricted by default;
  - a pod is isolated for a direction once any policy selects it for that direction;
  - allowed traffic is the additive union of policies, and order does not matter;
  - a connection needs both the source's egress allow and the destination's ingress allow;
  - reply traffic is implicit;
  - enforcement is guaranteed only for TCP, UDP and SCTP (ICMP is undefined);
  - `ipBlock` supports `except`; `endPort` supports port ranges;
  - peers are selected only by pod/namespace labels or `ipBlock`, so there are no FQDN rules ("Behavior of `to` and `from` selectors");
  - there are no deny rules and no logging; node traffic is always allowed;
  - whether a policy change affects existing connections is implementation-defined.
- **ClusterNetworkPolicy** (v1alpha2, which replaces ANP and BANP) [SIG-NP: api-overview]:
  - tiers are evaluated Admin, then NetworkPolicy, then Baseline;
  - actions are Accept, Deny ("No further … rules will be processed") and Pass;
  - within a tier, a lower priority value wins.
- **NPEP-133 (FQDN egress)** [NPEP-133]:
  - Accept-only, because a deny rule "may accidentally allow traffic to an IP belonging to a denied domain";
  - new connections stop at TTL expiry while existing ones persist;
  - it works only when the pod uses the cluster DNS;
  - a CNAME chain is honored only within a single response;
  - it cannot tell apart domains that share an IP.

**Enforcement points and their costs.**

- **In the VMM stack.** The VMM terminates guest flows, so it can decide at the host-socket boundary with the full 5-tuple, after reassembly. The guest cannot bypass this point if the VMM is the guest's only network path (compare libkrun's security model [README:109-111]).
- **In the guest (nftables).** Rules live per network namespace [man: network_namespaces(7)]. Sets are hash tables or red-black trees [nftables wiki: Sets]. Sets can carry element timeouts, and `dynamic` sets can be updated from the packet path [man: nft(8)].
- **Rule-count and update costs.**
  - Linear iptables/nftables rule lists degrade as rules are added. With 50 rules, nft forwarding on a single core is about 5× slower than bpf-iptables [Miano19 §6.2.2, Fig. 6a].
  - Inserting one rule through the userspace tools takes 15–28 ms for iptables and 31–75 ms for nft [Miano19 §6.4.2, Table 1].
- **cgroup BPF.** The hooks exist (`cgroup/connect4/6`, `sendmsg4/6`, `bind4/6`, `cgroup_skb/ingress|egress`) [Documentation/bpf/libbpf/program_types.rst:20-75]. No peer-reviewed overhead figure was found (E5).

**Pitfalls of DNS-name egress rules (ground truth).**

1. **TTL.**
   - The TTL bounds how long a record may be cached; zero means "only for the transaction in progress" [RFC 1035 §3.2.1].
   - The value is unsigned 31-bit; a value with the MSB set is treated as 0 [RFC 2181 §8].
   - The guest caches for the TTL it receives, so the enforcer must not let its allow entry expire before the TTL the guest saw.
2. **CNAME chains.** An alias has exactly one canonical name [RFC 1034 §3.6.2; RFC 2181 §10.1]. Every record in the chain bounds validity, CNAMEs included.
3. **DNS over TCP.** Required: "All general-purpose DNS implementations MUST support both UDP and TCP" [RFC 7766 §5].
4. **Encrypted DNS.**
   - DNS over TLS uses TCP 853 [RFC 7858 §3.1]; DNS over QUIC uses UDP 853 [RFC 9250 §4.1.1].
   - DNS over HTTPS "MUST be used with the https URI scheme" [RFC 8484 §5], so it cannot be told apart from HTTPS. It can be blocked only by IP allowlisting.
5. **SVCB/HTTPS address hints.** `ipv4hint`/`ipv6hint` addresses "MAY" be used to connect [RFC 9460 §7.3]. They must be added to or stripped from the allowlist.
6. **Shared IPs.** SNI exists because servers host many names on one address [RFC 6066 §3]. ECH encrypts the SNI, and "many TLS servers host multiple domains on the same IP" [RFC 9849 §1]. Domain fronting hides the real host in the encrypted Host header [Fifield15 Abstract]. So name allowlists do not stop reaching other tenants behind a CDN.
7. **DNS rebinding.** Defense: resolvers must "prevent external names from resolving to internal addresses", and outbound port 53 must be blocked [Jackson07 §1, §5.1]. The ranges to screen are listed in [RFC 6890 §2.2.2-2.2.3], [RFC 1918], [RFC 3927], [RFC 4193] and [RFC 6598].
8. **Fragments.** Filtering before reassembly is open to tiny and overlapping fragment attacks [RFC 1858 §3-4; RFC 3128].

**go-microvm as a reference implementation (critique).**

Design:

- Frames pass through a Go relay in front of the gVisor netstack [NETWORKING.md:98-122; relay.go:126-237].
- DNS responses are snooped to create per-IP rules that expire [interceptor.go:124-213].
- Default deny applies when a policy is set.
- The hosted provider is wired in automatically, so deny-by-default cannot silently degrade [microvm.go:372-384].

What it does well:

- Only A records reachable through the response's own CNAME chain are accepted [dns.go:64-126].
- Only responses from the gateway are snooped [interceptor.go:125].
- Queries with more than one question are rejected [dns.go:34-36].
- Denied names get a synthesized NXDOMAIN and never leave the VM [interceptor.go:94-116].
- TTLs are clamped to between 60 s and 5 min [interceptor.go:14-26,150-156].
- In deny mode, non-IPv4 frames other than ARP are dropped [relay.go:187-200].

Defects:

- **(a) Stale docs.** The documentation still says IPv6 passes unfiltered [NETWORKING.md:277-279; SECURITY.md:385-386], contradicting the code above.
- **(b) DNS over TCP is broken.** Only UDP DNS is intercepted [relay.go:161-184], and the implicit rule allows only UDP/53 to the gateway [hosted/provider.go:183-186]. Truncated responses that retry over TCP fail (RFC 7766 §5).
- **(c) Rebinding is open.** Any IPv4 address in an allowed answer becomes an allow rule [interceptor.go:170-211]; there is no check against RFC 6890 ranges. The netstack blocks only 169.254/16 and `net.Dial`s everything else [gvisor-tap-vsock tcp.go:24-34], so allowed names can be rebound to RFC 1918 hosts on the host's LAN or to the host's own non-loopback addresses. This matters most for wildcards such as `*.github.com` in the Standard profile [profiles.go:12-29].
- **(d) TTLs are not rewritten.** The TTL the guest sees is left unchanged, so a guest cache can outlive the 5-minute rule, and connections fail. CNAME TTLs are also left out of the minimum [dns.go:110-124].
- **(e) Rules are too broad.** Each rule allows a whole IP for the entire VM, not a (name, flow) pair, which is the shared-IP pitfall above.
- **(f) Linear scans and unbounded state.** Every new flow scans the dynamic rules linearly, and the rule list is capped at 10,000 with a silent drop once full [dynamic.go:15,44-93]. A guest can fill the cap by querying many allowed wildcard names. The conntrack map has no size bound [conntrack.go:33].
- **(g) Filtering before reassembly.** Ports are parsed without checking the fragment offset [firewall/packet.go:63-75], so the RFC 1858 attacks above apply.
- **(h) AAAA answers are ignored** [interceptor.go:171-174].

## 3. Implications for shards (ranked)

**R1. Default datapath: one userspace L2→L4 translator (passt-style), in-repo, on both macOS and Linux.** No TAP, no host network namespaces, no vmnet.

> Superseded in part (2026-09-30): the translator runs in a network process of its own per VM, not in the VMM, as rootless-security.md R4.16 and R6 require; architecture.md D31.

- **Why:**
  - It is the only option that is rootless and needs no entitlement on macOS [Apple: vmnet; socket_vmnet README].
  - It creates no host-namespace churn [Oakes18 §2.2; Thomas20 §2.3].
  - Each VM gets a private L2, so clones can keep the same IP and MAC.
  - Guest TCP sequence numbers never reach the wire, which removes the shared-key issue [secure_seq.c:22-28; RFC 6528 §3].
  - Adopting passt's no-buffering design keeps memory small [man: passt(1)].
  - The VMM becomes a single enforcement point (R7).
- **Cost and risk:**
  - Throughput is below kernel TAP: [Agache20 §5.3] vs [rootlesskit docs/network.md].
  - It must be written in-repo, because passt does not run on Darwin [passt: about].
- **Mitigations:**
  - Advertise TSO/GSO and a large MTU (up to 65520): RootlessKit's pasta goes from 0.24 to 31.9 Gbps with MTU alone [rootlesskit docs/network.md; Cai21 §3.1].
  - Batch I/O and events [Jeong14].
  - Split a fast path from a slow path [Kaufmann19].
- **Constraints:** no start-time cost; memory must be measured (E2). GPU: see R9.

**R2. Linux "fast mode", opt-in.**

- **Design:**
  - A per-user shards network namespace inside an unprivileged user namespace.
  - It holds a bridge plus one TAP per VM (allowed by tun.c:2827) and vhost-net, whose device is 0666 by default.
  - VM↔VM traffic and bulk flows run at kernel speed.
  - Policy is enforced by nftables sets in that namespace, which is still outside the guest.
- **Risks:**
  - Distributions can restrict unprivileged user namespaces [rootlesskit docs/network.md].
  - The code path is more complex.
  - Clones need per-VM NAT [firecracker network-for-clones.md].

**R3. Zero network work on the restore path.**

- Configure networking before the snapshot, with netlink calls: static addresses with `IFA_F_NODAD` [linux include/uapi/linux/if_addr.h:48] or `accept_dad=0`.
- Do not use DHCP, the kernel `ip=` option, or duplicate-address detection [RFC 2131 §4.4.1; RFC 5227 §1.1; RFC 4862 §5.4; ipconfig.c:86,1514; Agache20 §5.1].
- Keep the guest IP, MAC and gateway MAC constant across clones. VM identity lives only in the VMM, as per-VM NAT.
- Build the control plane (Engine API, exec, logs) on vsock and design it to tolerate `TRANSPORT_RESET` [VIRTIO-1.3 §5.10.6.7].
- `GUEST_ANNOUNCE` is only needed if VMs share an L2.

**R4. The in-VM engine owns the network; PID 1 pre-provisions once, at snapshot build time.** PID 1:

- maps the engine's uid through a subuid range;
- creates a network namespace owned by the engine's user namespace and moves `eth0` into it;
- writes vsock `child_ns_mode=local`;
- pre-creates the bridge and a pool of container sandboxes (network namespace + veth + address + DNS socket);
- then exits the privileged phase.

The guest kernel is built with bridge, veth, nf_tables and br_netfilter compiled in (`=y`), because the engine cannot load modules.

The engine then does everything as root of its own user namespace [man: user_namespaces(7); rtnetlink.c:7003; nfnetlink.c:659].

- **Benefit:** the pooling, batching and no-move strategies measured in [Mohan19 §5; Thomas20 §3.1; Oakes18 §4.1]. Creating veths straight into their target namespace [man: veth(4)] should avoid the costly `dev_change_net_namespace` move [Thomas20 §2.3]; this is a hypothesis, tested in E3.
- **Cost:** kernel memory for each pooled sandbox (E4). Re-addressing when a Compose IPAM subnet is non-default.
- Seccomp denies AF_VSOCK in containers [moby/profiles default.json].

**R5. Port publishing: host listener in the VMM, then a direct route to the container.** Traffic takes the "routed" path through guest `eth0`, with in-guest forwarding enabled [docker-docs: port-publishing "Gateway modes"].

- The engine keeps Docker's "UNPUBLISHED PORT DROP" rule [nftabler/network.go].
- No per-port DNAT rule is needed.
- Behavior across the stack:
  - Source IP becomes the VMM gateway, matching rootless Docker [docker-docs: rootless/troubleshoot].
  - Privileged ports follow the host OS: on Linux, `ip_unprivileged_port_start` [ip-sysctl.rst:1649-1656]; on macOS, only wildcard binds below 1024 [in_pcb.c:989-1003].
  - Surface these limits with Docker-rootless-style errors.
  - Mapping host loopback into the guest (`host.docker.internal`) is opt-in. passt's default mapping is the anti-pattern [man: passt(1)].

**R6. Embedded DNS with Docker's semantics but without DNAT.**

- Bind 127.0.0.11:53 (UDP and TCP) directly in each container namespace; the engine is root of that namespace's user namespace.
- Replicate the following [moby: resolver.go; sandbox.go:447-563]:
  - TTL 600;
  - the `ndots` behavior;
  - at most 3 upstream servers;
  - forwarding from the container's namespace;
  - no forwarding on internal networks.
- Point upstream at the VMM resolver, which is the policy point.

**R7. Policy model: three tiers (the ClusterNetworkPolicy shape) plus Compose extensions** [SIG-NP: api-overview; k8s-docs: network-policies; NPEP-133].

| Rule | Enforced at | Holds after container escape | Holds after guest-kernel compromise |
|---|---|---|---|
| VM no-network | No virtio-net device; vsock limited to control ports | Yes | Yes |
| VM egress CIDR/port/proto, allow and deny with priority | VMM, at host-socket open (post-reassembly) | Yes | Yes |
| VM egress by DNS name (Accept-only) | VMM resolver + (VM, name, IP, TTL) bindings | Yes | Yes, within the shared-IP limits |
| VM ingress, published ports, VM↔VM (egress ∧ ingress) | VMM listener or switch | Yes | Yes |
| Container none / internal / ICC / Docker isolation | Engine-namespace nftables (bridge forward) | Yes, if rules are outside the container namespace | No |
| Container egress/ingress allowlists, including names | Engine nftables sets with timeouts, filled by the per-container resolver | Yes | No |

- **Semantics.**
  - The effective policy is the intersection of the VM ceiling and the container policy. Container policies are additive and allow-only.
  - Name rules are Accept-only.
  - Resolved IPs in RFC 6890 ranges are denied unless explicitly allowed.
  - The TTL shown to the guest is capped at the rule lifetime. TCP DNS is intercepted.
  - DNS over TLS and DNS over QUIC are denied. SVCB/HTTPS hints are stripped.
  - Established flows survive TTL expiry.
- **What must be documented honestly.** A guest-kernel compromise collapses every per-container distinction to the VM ceiling. Agents that need hard isolation must run in separate microVMs.
- **Costs to design around.** Avoid linear rules and running `nft` at start [Miano19 §6.2.2, §6.4.2]. Use sets and netlink batches.

**R8. VM↔VM traffic.**

- Default: through the host's loopback published ports, at L4 with no extra infrastructure.
- Opt-in: a per-user userspace switch that enforces source egress and destination ingress.
- On macOS, vmnet host/shared mode is off the table without the entitlement [Apple: com.apple.vm.networking].

**R9. GPUs and training networking.**

- A userspace NAT stack is unlikely to carry multi-node collective traffic. Rootless userspace stacks reach at most 31.9 Gbps even with a jumbo MTU [rootlesskit docs/network.md]. Even kernel TAP with virtio reaches only 15–25 Gbps [Agache20 §5.3; firecracker network-performance.md], and "will not yield … PCI pass-through" performance [Agache20 §5.3].
- NIC or GPU passthrough through VFIO needs a one-time root step (`echo … > /sys/bus/pci/…/unbind`, `chown /dev/vfio/N`) [Documentation/driver-api/vfio.rst:137-163].
- Offer this as an administrator-provisioned tier.

**R10. Do not build the default on TSI.**

- It needs a custom guest kernel and has no policy hook.
- Its interaction with in-guest network namespaces is UNVERIFIED; the patches are not in the local tree.
- Socket-level interception pays extra at connection setup [Zhuo19 §1].

**Conflicts between constraints:**

- Rootless vs GPU/RDMA (R9).
- Rootless vs privileged ports on Linux; on macOS only wildcard binds work.
- Rootless and "install nothing" vs kernel-speed networking: fast paths need SUID (lxc-user-nic) or unprivileged user namespaces [rootlesskit docs].
- The ≤5 ms start vs per-container namespace setup: feasible only with pools baked into snapshots.
- The ≤5 ms start vs IPv6: duplicate-address detection must be off.
- Minimal memory vs pool size.
- Per-container policy vs a compromised guest kernel.
- DNS-name egress vs CDNs, ECH and DNS over HTTPS: best-effort only.

## 4. Open questions needing our own measurement

- **E1. Network ready after restore.**
  - Clone the VM 1000 times on each platform: M5 Max/macOS 26.4, Linux arm64, Linux x86_64.
  - Timestamp in the VMM (`CLOCK_MONOTONIC`) from resume to the first successful guest TCP connect to a host echo server, and to the first answer from 127.0.0.11 inside a container.
  - Report p50 and p99 against a 0.5 ms network share of the 5 ms budget.
- **E2. Datapath performance.** Compare the in-VMM stack, gvisor-tap-vsock, libslirp, passt (Linux), and TAP with vhost-net.
  - Workloads: iperf3 with 1 and 8 streams at MTU 1500/9000/65520; netperf TCP_RR and TCP_CRR.
  - Measurements: CPU cycles per byte (perf / Instruments); VMM RSS at 0, 1k and 10k idle connections.
- **E3. Namespace costs on the guest kernel (7.x).** Concurrency 1–256.
  - Operations: `unshare(CLONE_NEWNET)`; veth created in the target namespace vs moved there; bridge attach; address add with and without NODAD; a batched nft set update.
  - Use ftrace, as in [Oakes18 §2.2]; compare with [Thomas20 Table 3].
- **E4. Memory per pooled sandbox.** Read slabinfo and meminfo deltas across N = 0/16/64/256 sandboxes to size the pool against the memory target.
- **E5. Enforcement overhead.** Compare VMM socket-level checks, a go-microvm-style frame relay, engine nft sets, and cgroup connect hooks.
  - Workload: TCP_CRR rate and p99 connect latency.
  - Rule and name counts: 0, 100 and 10k.
- **E6. DNS-policy conformance suite.**
  - Cases: TTL rewrite and MSB-set TTLs; CNAME chains; TC=1 → TCP; rebinding to each RFC 6890 range; SVCB hints; DoT/DoQ; DoH to an allowed CDN (document the residual risk); RFC 1858/3128 fragments; filling the dynamic tables.
- **E7. vsock isolation.** Check that a container namespace created after `child_ns_mode=local` cannot connect to CID 2 over virtio-vsock, and that `socket(AF_VSOCK)` fails under the default seccomp profile.
- **E8. macOS host behavior.** Bind semantics for 0.0.0.0:80 vs 127.0.0.1:80 unprivileged; whether unprivileged ICMP echo sockets are available for guest ping; if an entitlement is ever obtained, vmnet interface start latency and the 32-interface limit.
- **E9. TSI and in-guest namespaces.** In a libkrunfw guest, connect from a non-init network namespace to a bridge IP. Determine whether TSI hijacks the socket (which breaks Docker semantics).
- **E10. Clone TCP sequence numbers.** On the fast path (R2), capture the ISNs of two clones for the same 4-tuple. If they match, re-key before networking resumes, or confine clones to R1.

## 5. References

**Peer-reviewed papers and the preprint**

- [Agache20] A. Agache et al. "Firecracker: Lightweight Virtualization for Serverless Applications." NSDI 2020. https://www.usenix.org/system/files/nsdi20-paper-agache.pdf
- [Brooker21] M. Brooker, A. C. Catangiu, M. Danilov, A. Graf, C. MacCarthaigh, A. Sandu. "Restoring Uniqueness in MicroVM Snapshots." arXiv:2102.12892, 2021. Preprint, not peer-reviewed. https://arxiv.org/abs/2102.12892
- [Cai21] Q. Cai, S. Chaudhary, M. Vuppalapati, J. Hwang, R. Agarwal. "Understanding Host Network Stack Overheads." SIGCOMM 2021. doi:10.1145/3452296.3472888. https://www.cs.cornell.edu/~ragarwal/pubs/network-stack.pdf
- [Fifield15] D. Fifield, C. Lan, R. Hynes, P. Wegmann, V. Paxson. "Blocking-resistant communication through domain fronting." PoPETs 2015(2):46–64. doi:10.1515/popets-2015-0009. https://petsymposium.org/popets/2015/popets-2015-0009.pdf
- [Jackson07] C. Jackson, A. Barth, A. Bortz, W. Shao, D. Boneh. "Protecting Browsers from DNS Rebinding Attacks." CCS 2007. https://crypto.stanford.edu/dns/dns-rebinding.pdf
- [Jeong14] E. Jeong, S. Woo, M. Jamshed, H. Jeong, S. Ihm, D. Han, K. Park. "mTCP: a Highly Scalable User-level TCP Stack for Multicore Systems." NSDI 2014. https://www.usenix.org/system/files/conference/nsdi14/nsdi14-paper-jeong.pdf
- [Kaufmann19] A. Kaufmann, T. Stamler, S. Peter, N. K. Sharma, A. Krishnamurthy, T. Anderson. "TAS: TCP Acceleration as an OS Service." EuroSys 2019. doi:10.1145/3302424.3303985
- [Miano19] S. Miano, M. Bertrone, F. Risso, M. Vásquez Bernal, Y. Lu, J. Pi. "Securing Linux with a Faster and Scalable Iptables." ACM SIGCOMM CCR 49(3), 2019. doi:10.1145/3371927.3371929. https://ccronline.sigcomm.org/wp-content/uploads/2019/07/acmdl19-304.pdf
- [Mohan19] A. Mohan, H. Sane, K. Doshi, S. Edupuganti, N. Nayak, V. Sukhomlinov. "Agile Cold Starts for Scalable Serverless." HotCloud 2019. https://www.usenix.org/system/files/hotcloud19-paper-mohan.pdf
- [Oakes18] E. Oakes, L. Yang, D. Zhou, K. Houck, T. Harter, A. C. Arpaci-Dusseau, R. H. Arpaci-Dusseau. "SOCK: Rapid Task Provisioning with Serverless-Optimized Containers." USENIX ATC 2018. https://www.usenix.org/system/files/conference/atc18/atc18-oakes.pdf
- [Thomas20] S. Thomas, L. Ao, G. M. Voelker, G. Porter. "Particle: Ephemeral Endpoints for Serverless Networking." SoCC 2020. doi:10.1145/3419111.3421275. https://www.sysnet.ucsd.edu/~voelker/pubs/particle-socc20.pdf
- [Young19] E. G. Young, P. Zhu, T. Caraza-Harter, A. C. Arpaci-Dusseau, R. H. Arpaci-Dusseau. "The True Cost of Containing: A gVisor Case Study." HotCloud 2019. https://www.usenix.org/system/files/hotcloud19-paper-young.pdf
- [Zhuo19] D. Zhuo, K. Zhang, Y. Zhu, H. H. Liu, M. Rockett, A. Krishnamurthy, T. Anderson. "Slim: OS Kernel Support for a Low-Overhead Container Overlay Network." NSDI 2019. https://www.usenix.org/system/files/nsdi19-zhuo.pdf

**Specifications**

- [VIRTIO-1.3] OASIS VIRTIO v1.3 csd01, §5.1 (net) and §5.10 (vsock).
- [RFC 1034], [RFC 1035], [RFC 1858], [RFC 1918], [RFC 2131], [RFC 2181], [RFC 3128], [RFC 3927], [RFC 4193], [RFC 4429], [RFC 4861], [RFC 4862], [RFC 5227], [RFC 6066], [RFC 6528], [RFC 6598], [RFC 6890], [RFC 7766], [RFC 7858], [RFC 8484], [RFC 9250], [RFC 9460], [RFC 9849] (TLS ECH, March 2026). All at https://www.rfc-editor.org/rfc/rfcN.txt
- Compose Specification, commit 914ec15: 05-services.md, 06-networks.md.

**Official documentation**

- [Apple: vmnet] developer.apple.com/documentation/vmnet
- [Apple: com.apple.vm.networking] developer.apple.com/documentation/bundleresources/entitlements/com.apple.vm.networking
- macOS SDK 26.4 headers: vmnet.h; Virtualization `VZVmnetNetworkDeviceAttachment.h`, `VZBridgedNetworkDeviceAttachment.h`, `VZFileHandleNetworkDeviceAttachment.h`.
- [man: user_namespaces(7)], [man: network_namespaces(7)], [man: veth(4)] (man7.org); [man: passt(1)] and [passt: about] (passt.top); [man: nft(8)] (netfilter.org); [nftables wiki: Sets].
- [docker-docs] docker/docs at f22c0e6: engine/network/{_index, port-publishing, packet-filtering-firewalls, drivers/{bridge,host,none}}.md, engine/daemon/ipv6.md, engine/security/rootless/{_index,troubleshoot}.md. [docker-cli] docker/cli at 7fc2dff: docs/reference/{dockerd.md, commandline/network_create.md}.
- [k8s-docs] kubernetes/website at 0457c0d: concepts/services-networking/network-policies.md. [SIG-NP] network-policy-api.sigs.k8s.io/api-overview/. [NPEP-133] …/npeps/npep-133-fqdn-egress-selector/.
- Linux 7.2-rc4 Documentation: networking/{tuntap, ip-sysctl}.rst, admin-guide/sysctl/net.rst, bpf/libbpf/program_types.rst, driver-api/vfio.rst.
- Project docs (project-reported): rootlesskit at e31dab4 (docs/network.md, docs/port.md); slirp4netns at 5731894 (README); libslirp at 62b2986 (README); gvisor-tap-vsock at 071cfd6 (README); socket_vmnet at a061a81 (README); Firecracker docs (network-setup, network-performance, jailer, snapshotting/*).

**Source code (pinned)**

- Linux 7.2-rc4 at 1590cf0: drivers/net/tun.c; drivers/vhost/net.c; net/core/{rtnetlink.c, secure_seq.c}; net/netfilter/nfnetlink.c; net/netlink/af_netlink.c; net/ipv4/ipconfig.c; net/vmw_vsock/af_vsock.c; kernel/bpf/{token.c, syscall.c}; drivers/virt/vmgenid.c; include/linux/socket.h; include/uapi/linux/if_addr.h. Local path /Users/adalundhe/Projects/linux.
- libkrun at 1f5dd02: README.md; src/devices/src/virtio/vsock/{mod.rs, tsi_stream/unix.rs}.
- Firecracker at edb6061: src/vmm/src/devices/virtio/net/device.rs; src/vmm/src/dumbo/mod.rs; src/vmm/src/vmm_config/snapshot.rs.
- go-microvm at 7e148d8: net/{egress,firewall,hosted}/*.go; microvm.go; guest/netcfg/netcfg.go; docs/{NETWORKING,SECURITY}.md.
- moby at c3c2a9e: daemon/libnetwork/{resolver.go, resolver_unix.go, sandbox.go, sandbox_dns_unix.go, drivers/bridge/{bridge_linux.go, setup_ipv4_linux.go, internal/nftabler/{nftabler.go, network.go}}}. moby/profiles at 85e237f: seccomp/default.json.
- gvisor-tap-vsock v0.8.9: pkg/services/forwarder/tcp.go; pkg/virtualnetwork/services.go.
- xnu-12377.121.6: bsd/netinet/in_pcb.c; bsd/netinet6/in6_pcb.c.
- systemd at be1e78e: rules.d/50-udev-default.rules.in; meson_options.txt.
