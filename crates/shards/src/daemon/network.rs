//! A run's networks, as dockerd takes what `--network` asked (moby docker-v29.3.1): the
//! checks it makes as it creates the container (daemon/create.go validateNetworkingConfig,
//! daemon/container_operations.go validateEndpointSettings), and what fails as it starts
//! it. Of dockerd's predefined networks, shards gives a run `bridge`, Docker's default
//! bridge through the VM's network process (D31), and `none`.
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
}

/// A user network's IPv4 subnets, by its name or ID; none for one there is not.
pub type UserSubnets<'a> = &'a dyn Fn(&str) -> Option<Vec<(IpAddr, u8)>>;

/// What starting the run will find of its networks: one to attach it to, or what dockerd
/// says as the start fails, the container left created.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Start {
    Attach(Net),
    Fails(String),
}

/// The driver option whose value sets the endpoint's interface sysctls.
const SYSCTLS: &str = "com.docker.network.endpoint.sysctls";

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
    if container.is_some() && run.hostname.is_some() {
        return Err("conflicting options: hostname and the network mode".into());
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
            endpoint_settings(e, bridge, user)
                .err()
                .map(|why| format!("invalid config for network {}: {why}", e.network))
        })
        .collect();
    if !invalid.is_empty() {
        return Err(join(&invalid));
    }
    // What shards does not do yet, refused before a container is made for it.
    let unsupported = |what: &str| Err(format!("\"--network {what}\" is not supported by shards yet"));
    if mode == "host" {
        return unsupported("host");
    }
    if let Some(name) = container
        && exists(name)
    {
        return unsupported("container:NAME");
    }
    for e in &endpoints {
        if !e.mac.is_empty() {
            return unsupported("mac-address");
        }
        if !e.link_local.is_empty() {
            return unsupported("link-local-ip");
        }
        if e.driver_opts.iter().any(|(k, _)| k == SYSCTLS) {
            return unsupported("driver-opt");
        }
    }
    // As it starts: the mode's network, then the rest.
    let fails = |why: String| Ok(Start::Fails(why));
    let net = match mode {
        // dockerd, which makes its bridge as it starts, does not start without one.
        "bridge" if bridge.is_none() => return Err(shards_net::bridge::NO_SUBNET.into()),
        "bridge" => Net::Bridge,
        "none" => Net::None,
        user_net if user(user_net).is_some() => Net::User,
        _ => {
            return match container {
                Some(name) => fails(format!(
                    "joining network namespace of container: No such container: {name}"
                )),
                None => fails(format!(
                    "failed to set up container networking: network {mode} not found"
                )),
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
            "none" => "container cannot be connected to multiple networks with one of the networks in private (none) mode".to_string(),
            _ if net == Net::None => "container cannot be connected to multiple networks with one of the networks in private (none) mode".to_string(),
            "host" => "cannot connect container to host network - container must be created in host network mode".to_string(),
            other => format!("network {other} not found"),
        };
        return fails(format!("failed to set up container networking: {why}"));
    }
    Ok(Start::Attach(net))
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
            errs.push("user-specified IP address is supported on user-defined networks only".into());
        }
        if !e.aliases.is_empty() {
            errs.push("network-scoped aliases are only supported for user-defined networks".into());
        }
    }
    // normalizeEndpointIPAMConfig.
    if let Some(a) = &ipv4
        && (!a.is4() && !a.is4in6() || a.is_unspecified())
    {
        errs.push(format!("invalid IPv4 address: {a}"));
    }
    if let Some(a) = &ipv6
        && (a.is4() || a.is4in6() || a.is_unspecified() || !a.zone.is_empty())
    {
        errs.push(format!("invalid IPv6 address: {a}"));
    }
    for a in &link_local {
        if a.is_unspecified() {
            errs.push(format!("invalid link-local IP address: {a}"));
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
                errs.push(format!("no configured subnet contains IP address {a}"));
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
                    "unrecognised network interface sysctl '{sysctl}'; represent 'net.X.Y.ethN.Z=V' as 'net.X.Y.IFNAME.Z=V', 'X' must be 'ipv4', 'ipv6' or 'mpls'"
                ));
            }
        }
    }
    if errs.is_empty() {
        Ok(())
    } else {
        Err(format!("invalid endpoint settings:\n{}", join(&errs)))
    }
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

    /// Every answer here is dockerd 29.3.1's, measured under Docker Desktop (2026-10-02).
    #[test]
    fn networks_are_taken_as_dockerd_takes_them() {
        let none = |_: &str| false;
        // Docker Desktop's bridge, where these were measured.
        let docker: Option<Bridge> = "172.17.0.0/16".parse().ok();
        let check = |networks: &[&str]| check(&run(networks), docker, none);
        let attach = |net| Ok(Start::Attach(net));
        let fails = |why: &str| Ok(Start::Fails(why.to_string()));
        let invalid = |net: &str, why: &str| {
            Err(format!(
                "invalid config for network {net}: invalid endpoint settings:\n{why}"
            ))
        };
        assert_eq!(check(&[]), attach(Net::Bridge));
        assert_eq!(check(&["bridge"]), attach(Net::Bridge));
        assert_eq!(check(&["default"]), attach(Net::Bridge));
        assert_eq!(check(&["none"]), attach(Net::None));
        assert_eq!(check(&["default", "bridge"]), attach(Net::Bridge));
        assert_eq!(check(&["name=bridge,gw-priority=1"]), attach(Net::Bridge));
        assert_eq!(
            check(&["foo"]),
            fails("failed to set up container networking: network foo not found")
        );
        assert_eq!(
            check(&["bridge", "default"]),
            fails("failed to set up container networking: network default not found")
        );
        assert_eq!(
            check(&["bridge", "none"]),
            fails(
                "failed to set up container networking: container cannot be connected to multiple networks with one of the networks in private (none) mode"
            )
        );
        assert_eq!(
            check(&["bridge", "host"]),
            fails(
                "failed to set up container networking: cannot connect container to host network - container must be created in host network mode"
            )
        );
        assert_eq!(
            check(&["container:nope"]),
            fails("joining network namespace of container: No such container: nope")
        );
        assert_eq!(
            check(&["container:nope", "none"]),
            fails("joining network namespace of container: No such container: nope")
        );
        assert_eq!(
            check(&["name=bridge,ip=172.17.0.9"]),
            invalid(
                "bridge",
                "user-specified IP address is supported on user-defined networks only"
            )
        );
        assert_eq!(
            check(&["name=bridge,ip=::ffff:172.17.0.9"]),
            invalid(
                "bridge",
                "user-specified IP address is supported on user-defined networks only"
            )
        );
        assert_eq!(
            check(&["name=bridge,ip=10.1.1.1"]),
            invalid(
                "bridge",
                "* user-specified IP address is supported on user-defined networks only\n* no configured subnet contains IP address 10.1.1.1"
            )
        );
        assert_eq!(
            check(&["name=bridge,ip=0.0.0.0"]),
            invalid(
                "bridge",
                "* user-specified IP address is supported on user-defined networks only\n* invalid IPv4 address: 0.0.0.0\n* no configured subnet contains IP address 0.0.0.0"
            )
        );
        assert_eq!(
            check(&["name=bridge,ip6=fd00::5"]),
            invalid(
                "bridge",
                "* user-specified IP address is supported on user-defined networks only\n* no configured subnet contains IP address fd00::5"
            )
        );
        assert_eq!(
            check(&["name=bridge,ip6=::ffff:1.2.3.4"]),
            invalid(
                "bridge",
                "* user-specified IP address is supported on user-defined networks only\n* invalid IPv6 address: ::ffff:1.2.3.4\n* no configured subnet contains IP address 1.2.3.4"
            )
        );
        assert_eq!(
            check(&["name=bridge,ip6=fe80::1%eth0"]),
            invalid(
                "bridge",
                "* user-specified IP address is supported on user-defined networks only\n* invalid IPv6 address: fe80::1%eth0\n* no configured subnet contains IP address fe80::1%eth0"
            )
        );
        assert_eq!(
            check(&["name=bridge,link-local-ip=0.0.0.0"]),
            invalid("bridge", "invalid link-local IP address: 0.0.0.0")
        );
        assert_eq!(
            check(&["name=bridge,driver-opt=com.docker.network.endpoint.sysctls=net.foo"]),
            invalid(
                "bridge",
                "unrecognised network interface sysctl 'net.foo'; represent 'net.X.Y.ethN.Z=V' as 'net.X.Y.IFNAME.Z=V', 'X' must be 'ipv4', 'ipv6' or 'mpls'"
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
                        "* user-specified IP address is supported on user-defined networks only\n* no configured subnet contains IP address {a}"
                    )
                )
            );
        }
        assert_eq!(
            check(&["name=default,ip=1.2.3.4"]),
            invalid(
                "bridge",
                "* user-specified IP address is supported on user-defined networks only\n* no configured subnet contains IP address 1.2.3.4"
            )
        );
        assert_eq!(
            check(&["name=container:nope,ip=1.2.3.4"]),
            invalid(
                "container:nope",
                "user-specified IP address is supported on user-defined networks only"
            )
        );
        let mut named = run(&["container:nope"]);
        named.hostname = Some("h".into());
        assert_eq!(
            check_with(&named),
            Err("conflicting options: hostname and the network mode".into())
        );
    }

    fn check_with(run: &Run) -> Result<Start, String> {
        check(run, "172.17.0.0/16".parse().ok(), |_| false)
    }

    /// What shards does not do yet is refused before a container is made, where dockerd
    /// would have made one.
    #[test]
    fn what_shards_does_not_do_yet_is_refused_up_front() {
        let docker: Option<Bridge> = "172.17.0.0/16".parse().ok();
        let unsupported = |what: &str| Err(format!("\"--network {what}\" is not supported by shards yet"));
        assert_eq!(check(&run(&["host"]), docker, |_| false), unsupported("host"));
        assert_eq!(
            check(&run(&["container:web"]), docker, |n| n == "web"),
            unsupported("container:NAME")
        );
        assert_eq!(
            check(
                &run(&["name=bridge,mac-address=02:11:22:33:44:55"]),
                docker,
                |_| false
            ),
            unsupported("mac-address")
        );
        assert_eq!(
            check(&run(&["name=bridge,link-local-ip=169.254.1.1"]), docker, |_| {
                false
            }),
            unsupported("link-local-ip")
        );
        assert_eq!(
            check(
                &run(&[
                    "name=bridge,driver-opt=com.docker.network.endpoint.sysctls=net.ipv4.conf.IFNAME.log_martians=1"
                ]),
                docker,
                |_| false
            ),
            unsupported("driver-opt")
        );
        // dockerd runs `none` then `bridge`; it refuses `bridge` then `none`, and so are both.
        assert_eq!(
            check(&run(&["none", "bridge"]), docker, |_| false),
            Ok(Start::Fails("failed to set up container networking: container cannot be connected to multiple networks with one of the networks in private (none) mode".into()))
        );
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
            Err("invalid config for network bridge: invalid endpoint settings:\nuser-specified IP address is supported on user-defined networks only".into())
        );
        let outside = check(&run(&["name=bridge,ip=172.17.0.9"]), elected, |_| false);
        assert_eq!(
            outside,
            Err("invalid config for network bridge: invalid endpoint settings:\n* user-specified IP address is supported on user-defined networks only\n* no configured subnet contains IP address 172.17.0.9".into())
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
