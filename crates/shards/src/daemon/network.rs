//! A run's networks, as dockerd takes what `--network` asked (moby docker-v29.3.1): the
//! checks it makes as it creates the container (daemon/create.go validateNetworkingConfig,
//! daemon/container_operations.go validateEndpointSettings), and what fails as it starts
//! it. Of dockerd's predefined networks, shards gives a run `bridge`, Docker's default
//! bridge through the VM's network process (D31), `none`, and `host`, the host's network as
//! a microVM's processes can have it (D124).
//!
//! Where dockerd's answer depends on Go's map order (several endpoints with errors), the
//! endpoints are taken in the order the run named them. Where it contradicts itself
//! (`none` and then `bridge` runs, `bridge` and then `none` does not), both orders are
//! refused as it refuses the second.

use std::net::IpAddr;

use shards_cmdline::network::{Addr, is_user_defined, parse_addr};
use shards_ipc::{Endpoint, Run};
use shards_net::bridge::Bridge;

/// What a run's guest is attached to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Net {
    /// Docker's default bridge, on the subnet the daemon elected (shards_net::bridge).
    Bridge,
    /// A loopback alone.
    None,
    /// A user-defined network (D46), which the run's address and peers are of.
    User,
    /// The host's network (D124): the guest on the default bridge's link, its flows
    /// reaching what the host reaches, the host's own loopback among them.
    Host,
}

/// A user network's IPv4 subnets, by its name or ID; none for one there is not.
pub type UserSubnets<'a> = &'a dyn Fn(&str) -> Option<Vec<(IpAddr, u8)>>;

/// What starting the run will find of its networks: one to attach it to, another
/// container's to join (D119), or what dockerd says as the start fails, the container left
/// created.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Start {
    Attach(Net),
    /// `--network container:NAME`: the network of the container NAME names, as given; which
    /// container that is, and whether it runs, is seen as the run starts, as dockerd sees it
    /// (getNetworkedContainer).
    Join(String),
    Fails(String),
}

/// The driver option whose value sets the endpoint's interface sysctls.
use crate::setup::ENDPOINT_SYSCTLS as SYSCTLS;

/// moby's create route (daemon/server/router/container, handleSysctlBC, API 1.48 and on):
/// a sysctl of an interface (`net.X.Y.ethN.Z`) given a container on a network of its
/// own (not `host`, not `container:`) is refused before anything else is checked; it is
/// an endpoint's (`driver-opt=com.docker.network.endpoint.sysctls=…`).
pub fn interface_sysctl(run: &Run) -> Result<(), String> {
    if run.network == "host" || run.network.starts_with("container:") {
        return Ok(());
    }
    for kv in &run.sysctls {
        let key = kv.split_once('=').map_or(kv.as_str(), |(k, _)| k);
        let parts: Vec<&str> = key.splitn(5, '.').collect();
        if let [net, _, _, iface, _] = parts.as_slice()
            && *net == "net"
            && iface.starts_with("eth")
        {
            return Err(format!(
                "interface specific sysctl setting \"{key}\" must be supplied using driver option '{SYSCTLS}'"
            ));
        }
    }
    Ok(())
}

/// The subnets of a network, of both versions: a user network's, or those of dockerd's
/// predefined networks (the default bridge's IPv4 subnet for `bridge`; none has IPv6),
/// or `None` for a network it does not have.
fn subnets(network: &str, bridge: Option<Bridge>, user: UserSubnets<'_>) -> Option<Vec<(IpAddr, u8)>> {
    if is_user_defined(network) {
        return user(network);
    }
    match network {
        "bridge" => Some(
            bridge
                .iter()
                .map(Bridge::subnet)
                .map(|(net, bits)| (IpAddr::V4(net), bits))
                .collect(),
        ),
        "none" | "host" => Some(Vec::new()),
        _ => None,
    }
}

/// What run `run` will find as it starts, or why its container is not created. `bridge`
/// is the default bridge the daemon elected, none if every subnet it may take is in use
/// on the host; `exists` says whether a container is named so (for `container:NAME`).
pub fn check(
    run: &Run,
    bridge: Option<Bridge>,
    exists: impl Fn(&str) -> bool,
    user: UserSubnets<'_>,
) -> Result<Start, String> {
    // dockerd takes the default network mode as its bridge, and the endpoint named for it
    // as the bridge's.
    let mode = match run.network.as_str() {
        "" | "default" => "bridge",
        other => other,
    };
    let container = mode
        .split_once(':')
        .filter(|(k, _)| *k == "container")
        .map(|(_, v)| v);
    // A run joining another's microVM (D119) takes that run's names and network: what it
    // would set of them is refused as it is made, in the order moby checks them
    // (daemon/internal/runconfig/hostconfig.go validateNetContainerMode), and never as it
    // starts again, when its names are its provider's.
    let creating = run.again.is_none();
    if creating && (mode == "container" || container == Some("")) {
        return Err("--network container: name the run whose microVM to join, as container:NAME".into());
    }
    if creating && container.is_some() {
        let theirs = |flag: &str, what: &str| {
            format!("{flag}: a run joining another's microVM takes that run's {what}")
        };
        if run.hostname.is_some() {
            return Err(theirs("--hostname", "hostname"));
        }
        if !run.dns.is_empty() {
            return Err(theirs("--dns", "resolvers"));
        }
        if !run.add_hosts.is_empty() {
            return Err(theirs("--add-host", "hosts file"));
        }
        if !run.publish.is_empty() || run.publish_all {
            return Err(theirs("-p", "ports: publish them on that run"));
        }
        if !run.expose.is_empty() {
            return Err(theirs("--expose", "ports: expose them on that run"));
        }
    }
    let mut endpoints: Vec<Endpoint> = Vec::with_capacity(run.endpoints.len());
    for e in &run.endpoints {
        let mut e = e.clone();
        if e.network == "default" && run.network == "default" {
            e.network = "bridge".into();
        }
        if !endpoints.iter().any(|have| have.network == e.network) {
            endpoints.push(e);
        }
    }
    let invalid: Vec<String> = endpoints
        .iter()
        .filter_map(|e| {
            endpoint_settings(e, bridge, user).err().map(|why| {
                // Several, each on a line of its own under the network's name.
                let sep = if why.starts_with("* ") { "\n" } else { " " };
                format!("--network {}:{sep}{why}", e.network)
            })
        })
        .collect();
    if !invalid.is_empty() {
        return Err(join(&invalid));
    }
    if let Some(name) = container
        && exists(name)
    {
        return Ok(Start::Join(name.to_string()));
    }
    // As it starts: the mode's network, then the rest.
    let fails = |why: String| Ok(Start::Fails(why));
    if mode == "host" {
        // libnetwork's, of a sandbox that is the host's (PM M177): no other network. Where
        // the other is a bridge, dockerd tries its interface in the host's namespace, and
        // fails on what that host's routes are; shards says what it says of `none`.
        if endpoints.iter().any(|e| e.network != mode) {
            return fails("--network host: a run on its host's network is on no other".into());
        }
        // runc's validator (libcontainer/configs/validate, sysctl): no net.* sysctl in the
        // host's network namespace; the first given, where runc's map order would choose.
        let net_sysctl = run
            .sysctls
            .iter()
            .map(|kv| kv.split_once('=').map_or(kv.as_str(), |(k, _)| k))
            .find(|k| k.starts_with("net."));
        if let Some(key) = net_sysctl {
            return fails(format!(
                "--sysctl {key}: a run on its host's network has no network of its own to set it in"
            ));
        }
        return Ok(Start::Attach(Net::Host));
    }
    let net = match mode {
        // dockerd, which makes its bridge as it starts, does not start without one.
        "bridge" if bridge.is_none() => return Err(shards_net::bridge::NO_SUBNET.into()),
        "bridge" => Net::Bridge,
        "none" => Net::None,
        user_net if user(user_net).is_some() => Net::User,
        _ => {
            return match container {
                Some(name) => fails(format!("--network container:{name}: No such container: {name}")),
                None => fails(format!("--network {mode}: no network is named {mode}")),
            };
        }
    };
    // One network device a microVM: one network a microVM, yet (D46's open item).
    if net == Net::User
        && endpoints
            .iter()
            .any(|e| e.network != mode && user(&e.network).is_some())
    {
        return Err(
            "a microVM on more than one network is not supported by shards yet: each has one network device"
                .into(),
        );
    }
    if let Some(e) = endpoints.iter().find(|e| e.network != mode) {
        let why = match e.network.as_str() {
            "none" => "--network none: a run with no network is on no other".to_string(),
            _ if net == Net::None => "--network none: a run with no network is on no other".to_string(),
            "host" => {
                "--network host: a run is on its host's network as it is made, with --network host alone"
                    .to_string()
            }
            other => format!("--network {other}: no network is named {other}"),
        };
        return fails(why);
    }
    // As libnetwork makes the endpoint (network.go createEndpoint), on `none` too: each of
    // its link-local addresses one, else the first that is not fails the start (PM M175).
    if let Some(bad) = endpoints
        .iter()
        .find(|e| e.network == mode)
        .into_iter()
        .flat_map(|e| e.link_local.iter().filter_map(|s| parse_addr(s).ok()))
        .find(|a| !link_local_unicast(a))
    {
        return fails(format!(
            "--link-local-ip {}: not a link-local address (169.254.0.0/16, fe80::/10)",
            bad.unmap().ip
        ));
    }
    Ok(Start::Attach(net))
}

/// Go's `net.IP.IsLinkLocalUnicast` of an address as net.IP holds it, its zone gone and an
/// IPv4-mapped one as IPv4: 169.254.0.0/16 and fe80::/10.
pub fn link_local_unicast(a: &Addr) -> bool {
    match a.unmap().ip {
        IpAddr::V4(v4) => v4.is_link_local(),
        IpAddr::V6(v6) => v6.is_unicast_link_local(),
    }
}

/// validateEndpointSettings: Ok, or the errors joined under "invalid endpoint settings:".
fn endpoint_settings(e: &Endpoint, bridge: Option<Bridge>, user: UserSubnets<'_>) -> Result<(), String> {
    let addr = |s: &str| (!s.is_empty()).then(|| parse_addr(s)).transpose();
    let ipv4 = addr(&e.ipv4)?;
    let ipv6 = addr(&e.ipv6)?;
    let link_local = e
        .link_local
        .iter()
        .map(|s| parse_addr(s))
        .collect::<Result<Vec<_>, _>>()?;
    let mut errs: Vec<String> = Vec::new();
    if !is_user_defined(&e.network) {
        if ipv4.is_some() || ipv6.is_some() {
            errs.push("--ip: an address of its own is a run's on a network you made".into());
        }
        if !e.aliases.is_empty() {
            errs.push(
                "--network-alias: names are a run's on a network you made, where its peers ask for them"
                    .into(),
            );
        }
    }
    // normalizeEndpointIPAMConfig.
    if let Some(a) = &ipv4
        && (!a.is4() && !a.is4in6() || a.is_unspecified())
    {
        errs.push(format!("--ip {a}: not an IPv4 address a run can have"));
    }
    if let Some(a) = &ipv6
        && (a.is4() || a.is4in6() || a.is_unspecified() || !a.zone.is_empty())
    {
        errs.push(format!("--ip6 {a}: not an IPv6 address a run can have"));
    }
    for a in &link_local {
        if a.is_unspecified() {
            errs.push(format!("--link-local-ip {a}: not an address a run can have"));
        }
    }
    // validateIPAMConfigIsInRange, for the networks dockerd has.
    if let Some(nets) = subnets(&e.network, bridge, user) {
        let within = |a: &Addr| {
            nets.iter().any(|(net, bits)| match (a.ip, net) {
                (IpAddr::V4(ip), IpAddr::V4(net)) => {
                    let mask = u32::MAX.checked_shl(32 - u32::from(*bits)).unwrap_or(0);
                    u32::from(ip) & mask == u32::from(*net) & mask
                }
                (IpAddr::V6(ip), IpAddr::V6(net)) => {
                    let mask = u128::MAX.checked_shl(128 - u32::from(*bits)).unwrap_or(0);
                    u128::from(ip) & mask == u128::from(*net) & mask
                }
                _ => false,
            })
        };
        for a in [ipv4.as_ref().map(Addr::unmap), ipv6.as_ref().map(Addr::unmap)]
            .into_iter()
            .flatten()
        {
            if !within(&a) {
                errs.push(format!("{a}: in none of the network's subnets"));
            }
        }
    }
    if let Some((_, sysctls)) = e.driver_opts.iter().find(|(k, _)| k == SYSCTLS) {
        for sysctl in sysctls.split(',') {
            let parts: Vec<&str> = sysctl.splitn(5, '.').collect();
            let ok = parts.len() == 5
                && matches!(parts.get(1), Some(&("ipv4" | "ipv6" | "mpls")))
                && matches!(parts.get(3), Some(&("IFNAME" | "ifname")));
            if !ok {
                errs.push(format!(
                    "endpoint sysctl {sysctl}: name an interface's as net.X.Y.IFNAME.Z=V, X one of ipv4, ipv6 or mpls"
                ));
            }
        }
    }
    if errs.is_empty() { Ok(()) } else { Err(join(&errs)) }
}

/// moby's multierror.Join: one error as itself, trimmed; several as a list, each line
/// after a bullet's first indented.
fn join(errs: &[String]) -> String {
    match errs {
        [one] => one.trim().to_string(),
        _ => {
            let items: Vec<String> = errs.iter().map(|e| e.replace('\n', "\n\t")).collect();
            format!("* {}", items.join("\n* "))
        }
    }
}

#[cfg(test)]
mod tests {
    /// [`super::check`] where no user network is there.
    fn check(run: &Run, bridge: Option<Bridge>, exists: impl Fn(&str) -> bool) -> Result<Start, String> {
        super::check(run, bridge, exists, &|_| None)
    }

    use super::*;

    fn run(networks: &[&str]) -> Run {
        let given: Vec<_> = networks
            .iter()
            .map(|n| shards_cmdline::network::attachment(n).unwrap())
            .collect();
        Run {
            network: shards_cmdline::network::mode(&given).to_string(),
            endpoints: shards_cmdline::network::endpoints(
                &given,
                &shards_cmdline::network::TopLevel::default(),
            )
            .unwrap()
            .into_iter()
            .map(|a| Endpoint {
                network: a.target,
                aliases: a.aliases,
                ipv4: a.ipv4.map(|a| a.to_string()).unwrap_or_default(),
                ipv6: a.ipv6.map(|a| a.to_string()).unwrap_or_default(),
                link_local: a.link_local.iter().map(ToString::to_string).collect(),
                mac: a.mac,
                driver_opts: a.driver_opts,
                gw_priority: a.gw_priority,
            })
            .collect(),
            ..Run::default()
        }
    }

    /// Each refused or taken when moby 29.3.1 does it (measured under Docker Desktop,
    /// 2026-10-02), in shards' words.
    #[test]
    fn run_networks_are_checked() {
        let none = |_: &str| false;
        // The host bridge where these were measured.
        let bridge: Option<Bridge> = "172.17.0.0/16".parse().ok();
        let check = |networks: &[&str]| check(&run(networks), bridge, none);
        let attach = |net| Ok(Start::Attach(net));
        let fails = |why: &str| Ok(Start::Fails(why.to_string()));
        let invalid = |net: &str, why: &str| {
            let sep = if why.starts_with("* ") { "\n" } else { " " };
            Err(format!("--network {net}:{sep}{why}"))
        };
        assert_eq!(check(&[]), attach(Net::Bridge));
        assert_eq!(check(&["bridge"]), attach(Net::Bridge));
        assert_eq!(check(&["default"]), attach(Net::Bridge));
        assert_eq!(check(&["none"]), attach(Net::None));
        assert_eq!(check(&["default", "bridge"]), attach(Net::Bridge));
        assert_eq!(check(&["name=bridge,gw-priority=1"]), attach(Net::Bridge));
        assert_eq!(check(&["foo"]), fails("--network foo: no network is named foo"));
        assert_eq!(
            check(&["bridge", "default"]),
            fails("--network default: no network is named default")
        );
        assert_eq!(
            check(&["bridge", "none"]),
            fails("--network none: a run with no network is on no other")
        );
        assert_eq!(
            check(&["bridge", "host"]),
            fails("--network host: a run is on its host's network as it is made, with --network host alone")
        );
        assert_eq!(
            check(&["container:nope"]),
            fails("--network container:nope: No such container: nope")
        );
        assert_eq!(
            check(&["container:nope", "none"]),
            fails("--network container:nope: No such container: nope")
        );
        assert_eq!(
            check(&["name=bridge,ip=172.17.0.9"]),
            invalid(
                "bridge",
                "--ip: an address of its own is a run's on a network you made"
            )
        );
        assert_eq!(
            check(&["name=bridge,ip=::ffff:172.17.0.9"]),
            invalid(
                "bridge",
                "--ip: an address of its own is a run's on a network you made"
            )
        );
        assert_eq!(
            check(&["name=bridge,ip=10.1.1.1"]),
            invalid(
                "bridge",
                "* --ip: an address of its own is a run's on a network you made\n* 10.1.1.1: in none of the network's subnets"
            )
        );
        assert_eq!(
            check(&["name=bridge,ip=0.0.0.0"]),
            invalid(
                "bridge",
                "* --ip: an address of its own is a run's on a network you made\n* --ip 0.0.0.0: not an IPv4 address a run can have\n* 0.0.0.0: in none of the network's subnets"
            )
        );
        assert_eq!(
            check(&["name=bridge,ip6=fd00::5"]),
            invalid(
                "bridge",
                "* --ip: an address of its own is a run's on a network you made\n* fd00::5: in none of the network's subnets"
            )
        );
        assert_eq!(
            check(&["name=bridge,ip6=::ffff:1.2.3.4"]),
            invalid(
                "bridge",
                "* --ip: an address of its own is a run's on a network you made\n* --ip6 ::ffff:1.2.3.4: not an IPv6 address a run can have\n* 1.2.3.4: in none of the network's subnets"
            )
        );
        assert_eq!(
            check(&["name=bridge,ip6=fe80::1%eth0"]),
            invalid(
                "bridge",
                "* --ip: an address of its own is a run's on a network you made\n* --ip6 fe80::1%eth0: not an IPv6 address a run can have\n* fe80::1%eth0: in none of the network's subnets"
            )
        );
        assert_eq!(
            check(&["name=bridge,link-local-ip=0.0.0.0"]),
            invalid("bridge", "--link-local-ip 0.0.0.0: not an address a run can have")
        );
        assert_eq!(
            check(&["name=bridge,driver-opt=com.docker.network.endpoint.sysctls=net.foo"]),
            invalid(
                "bridge",
                "endpoint sysctl net.foo: name an interface's as net.X.Y.IFNAME.Z=V, X one of ipv4, ipv6 or mpls"
            )
        );
        for (net, ip) in [
            ("none", "ip=10.1.1.1"),
            ("none", "ip6=fd00::5"),
            ("host", "ip=1.2.3.4"),
        ] {
            let a = ip.split_once('=').unwrap().1;
            assert_eq!(
                check(&[&format!("name={net},{ip}")]),
                invalid(
                    net,
                    &format!(
                        "* --ip: an address of its own is a run's on a network you made\n* {a}: in none of the network's subnets"
                    )
                )
            );
        }
        assert_eq!(
            check(&["name=default,ip=1.2.3.4"]),
            invalid(
                "bridge",
                "* --ip: an address of its own is a run's on a network you made\n* 1.2.3.4: in none of the network's subnets"
            )
        );
        assert_eq!(
            check(&["name=container:nope,ip=1.2.3.4"]),
            invalid(
                "container:nope",
                "--ip: an address of its own is a run's on a network you made"
            )
        );
        let mut named = run(&["container:nope"]);
        named.hostname = Some("h".into());
        assert_eq!(
            check_with(&named),
            Err("--hostname: a run joining another's microVM takes that run's hostname".into())
        );
    }

    /// A container network mode, checked as dockerd checks it as the container is made
    /// (validateNetContainerMode), in its order and words: each conflict alone, then two,
    /// of which the first in dockerd's order is said.
    #[test]
    fn a_joiners_network_mode_is_checked() {
        let joined = || run(&["container:web"]);
        let bridge: Option<Bridge> = "172.17.0.0/16".parse().ok();
        let check = |r: &Run| check(r, bridge, |n| n == "web");
        let said = |what: &str| Err(what.to_string());
        assert_eq!(
            check(&run(&["container:"])),
            Err("--network container: name the run whose microVM to join, as container:NAME".into())
        );
        let mut r = joined();
        r.dns = vec!["1.1.1.1".into()];
        assert_eq!(
            check(&r),
            said("--dns: a run joining another's microVM takes that run's resolvers")
        );
        let mut r = joined();
        r.add_hosts = vec!["a:1.2.3.4".into()];
        assert_eq!(
            check(&r),
            said("--add-host: a run joining another's microVM takes that run's hosts file")
        );
        let mut r = joined();
        r.publish_all = true;
        assert_eq!(
            check(&r),
            said("-p: a run joining another's microVM takes that run's ports: publish them on that run")
        );
        let mut r = joined();
        r.expose = vec!["80".into()];
        assert_eq!(
            check(&r),
            said("--expose: a run joining another's microVM takes that run's ports: expose them on that run")
        );
        // dns before exposing, as dockerd checks them.
        let mut r = joined();
        r.expose = vec!["80".into()];
        r.dns = vec!["1.1.1.1".into()];
        assert_eq!(
            check(&r),
            said("--dns: a run joining another's microVM takes that run's resolvers")
        );
        assert_eq!(check(&joined()), Ok(Start::Join("web".into())));
        // Started again, its names its provider's as its first start stored them: nothing
        // of what it was created by is checked again, as dockerd checks none.
        let mut again = joined();
        again.hostname = Some("web-host".into());
        again.again = Some("id".into());
        assert_eq!(check(&again), Ok(Start::Join("web".into())));
        // One that is not there fails as it starts, the container made, in dockerd's words.
        assert_eq!(
            check(&run(&["container:nope"])),
            Ok(Start::Fails(
                "--network container:nope: No such container: nope".into()
            ))
        );
    }

    fn check_with(run: &Run) -> Result<Start, String> {
        check(run, "172.17.0.0/16".parse().ok(), |_| false)
    }

    /// The host's network (D124, PM M177): taken alone; a `net.*` sysctl, which has no
    /// network namespace of its own there, and a second network, fail the start in runc's
    /// and libnetwork's words.
    #[test]
    fn the_hosts_network_is_taken_alone() {
        assert_eq!(check_with(&run(&["host"])), Ok(Start::Attach(Net::Host)));
        let mut tuned = run(&["host"]);
        tuned.sysctls = vec!["kernel.shmmax=1".into(), "net.core.somaxconn=1024".into()];
        assert_eq!(
            check_with(&tuned),
            Ok(Start::Fails(
                "--sysctl net.core.somaxconn: a run on its host's network has no network of its own to set it in".into()
            ))
        );
        assert_eq!(
            check_with(&run(&["host", "none"])),
            Ok(Start::Fails(
                "--network host: a run on its host's network is on no other".into()
            ))
        );
    }

    /// What shards does not do yet is refused before a container is made, where dockerd
    /// would have made one.
    #[test]
    fn what_shards_does_not_do_yet_is_refused_up_front() {
        let bridge: Option<Bridge> = "172.17.0.0/16".parse().ok();
        assert_eq!(
            check(&run(&["container:web"]), bridge, |n| n == "web"),
            Ok(Start::Join("web".into()))
        );
        // An endpoint's MAC is taken (D46, PM M173).
        assert_eq!(
            check(
                &run(&["name=bridge,mac-address=02:11:22:33:44:55"]),
                bridge,
                |_| false
            ),
            Ok(Start::Attach(Net::Bridge))
        );
        // Link-local addresses are taken; one that is not link-local fails the start after
        // the network is found, on `none` too, the first of several (PM M175).
        assert_eq!(
            check(&run(&["name=bridge,link-local-ip=169.254.1.1"]), bridge, |_| {
                false
            }),
            Ok(Start::Attach(Net::Bridge))
        );
        for (networks, bad) in [
            (&["name=bridge,link-local-ip=10.9.8.7"][..], "10.9.8.7"),
            (&["name=none,link-local-ip=fd00::7"], "fd00::7"),
            (
                &["name=bridge,link-local-ip=169.254.1.1,link-local-ip=::ffff:10.1.1.1"],
                "10.1.1.1",
            ),
        ] {
            assert_eq!(
                check(&run(networks), bridge, |_| false),
                Ok(Start::Fails(format!(
                    "--link-local-ip {bad}: not a link-local address (169.254.0.0/16, fe80::/10)"
                ))),
                "{networks:?}"
            );
        }
        assert_eq!(
            check(&run(&["name=nope,link-local-ip=10.9.8.7"]), bridge, |_| false),
            Ok(Start::Fails("--network nope: no network is named nope".into()))
        );
        // An endpoint's sysctls are taken (PM M171).
        assert_eq!(
            check(
                &run(&[
                    "name=bridge,driver-opt=com.bridge.network.endpoint.sysctls=net.ipv4.conf.IFNAME.log_martians=1"
                ]),
                bridge,
                |_| false
            ),
            Ok(Start::Attach(Net::Bridge))
        );
        // dockerd runs `none` then `bridge`; it refuses `bridge` then `none`, and so are both.
        assert_eq!(
            check(&run(&["none", "bridge"]), bridge, |_| false),
            Ok(Start::Fails(
                "--network none: a run with no network is on no other".into()
            ))
        );
    }

    /// An interface's sysctl given as a container's own is refused as moby's create route
    /// refuses it, on any network of the container's own, `none` too, for eth0 and any
    /// other; on `host`, or another's network, it is not; other sysctls pass (PM M171). An
    /// endpoint's sysctls are its network's, none on `none`.
    #[test]
    fn interface_sysctls_are_endpoints() {
        let with = |network: &str, sysctls: &[&str]| Run {
            network: network.into(),
            sysctls: sysctls.iter().map(|s| s.to_string()).collect(),
            ..Run::default()
        };
        let refused = |key: &str| {
            Err(format!(
                "interface specific sysctl setting \"{key}\" must be supplied using driver option 'com.docker.network.endpoint.sysctls'"
            ))
        };
        for network in ["default", "bridge", "none", "mine"] {
            for key in ["net.ipv4.conf.eth0.log_martians", "net.ipv6.conf.eth1.forwarding"] {
                assert_eq!(
                    interface_sysctl(&with(network, &[&format!("{key}=1")])),
                    refused(key),
                    "{network} {key}"
                );
            }
        }
        for (network, sysctl) in [
            ("host", "net.ipv4.conf.eth0.log_martians=1"),
            ("container:x", "net.ipv4.conf.eth0.log_martians=1"),
            ("bridge", "net.ipv4.conf.all.forwarding=1"),
            ("bridge", "net.ipv4.ip_forward=1"),
            ("bridge", "kernel.shmmax=1"),
        ] {
            assert_eq!(
                interface_sysctl(&with(network, &[sysctl])),
                Ok(()),
                "{network} {sysctl}"
            );
        }
        let opt = "com.docker.network.endpoint.sysctls=net.ipv4.conf.IFNAME.log_martians=1,net.ipv4.conf.ifname.accept_redirects=0";
        let given = run(&[&format!("name=bridge,\"driver-opt={opt}\"")]);
        // Lower case, as docker/cli hands driver options on (moby: "the CLI converts to
        // lower case").
        assert_eq!(
            crate::setup::endpoint_sysctls(&given),
            [
                "net.ipv4.conf.ifname.log_martians=1",
                "net.ipv4.conf.ifname.accept_redirects=0"
            ]
        );
        let none = run(&[&format!("name=none,\"driver-opt={opt}\"")]);
        assert!(crate::setup::endpoint_sysctls(&none).is_empty());
    }

    /// The bridge is the one the daemon elected: an address is checked against its subnet,
    /// and without one, which no subnet left to elect means, a run on it is refused as
    /// dockerd, which would not have started, cannot run one; a run on `none` still is.
    #[test]
    fn the_bridge_is_the_one_elected() {
        let elected: Option<Bridge> = "172.18.0.0/16".parse().ok();
        let within = check(&run(&["name=bridge,ip=172.18.0.9"]), elected, |_| false);
        assert_eq!(
            within,
            Err("--network bridge: --ip: an address of its own is a run's on a network you made".into())
        );
        let outside = check(&run(&["name=bridge,ip=172.17.0.9"]), elected, |_| false);
        assert_eq!(
            outside,
            Err("--network bridge:\n* --ip: an address of its own is a run's on a network you made\n* 172.17.0.9: in none of the network's subnets".into())
        );
        assert_eq!(
            check(&run(&[]), None, |_| false),
            Err(shards_net::bridge::NO_SUBNET.into())
        );
        assert_eq!(
            check(&run(&["none"]), None, |_| false),
            Ok(Start::Attach(Net::None))
        );
    }
}
