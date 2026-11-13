//! The run's CA in its members' trust (D242): the certificate of the CA its network process
//! made for its credential grants, and the hosts those grants name, as setup entries the run
//! host adds once the network process has answered its grants. init lays the certificate
//! into the trust of each member whose link reaches one of the hosts, and of no other.

use alloc::vec::Vec;

/// `run-ca=<PEM>`: the CA's certificate, in PEM (RFC 7468); once.
pub const CA_KEY: &[u8] = b"run-ca=";
/// `run-ca-host=<host>`: a host one of the run's credential grants names, lowered; an entry
/// each.
pub const HOST_KEY: &[u8] = b"run-ca-host=";

/// Why the run's CA entries are refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// A second certificate.
    SecondCa,
    /// A certificate and no host, or hosts and no certificate: the run host sends both or
    /// neither.
    Unpaired,
    /// An empty certificate, or one holding a NUL.
    Certificate,
    /// An empty host, or one holding a NUL.
    Host,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Error::SecondCa => "a second certificate for the run's CA",
            Error::Unpaired => "the run's CA without the hosts it is for, or hosts without it",
            Error::Certificate => "an empty certificate, or one holding a NUL",
            Error::Host => "an empty host, or one holding a NUL",
        })
    }
}

/// The run's CA as its setup gives it: its certificate, and each host its grants name, once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trust<'a> {
    pub pem: &'a [u8],
    pub hosts: Vec<&'a [u8]>,
}

fn plain(bytes: &[u8]) -> bool {
    !bytes.is_empty() && !bytes.contains(&0)
}

/// The entry giving the run's CA its certificate `pem`.
pub fn ca(pem: &[u8]) -> Result<Vec<u8>, Error> {
    if !plain(pem) {
        return Err(Error::Certificate);
    }
    Ok([CA_KEY, pem].concat())
}

/// The entry naming `host`, one the run's credential grants name.
pub fn host(host: &[u8]) -> Result<Vec<u8>, Error> {
    if !plain(host) {
        return Err(Error::Host);
    }
    Ok([HOST_KEY, host].concat())
}

/// The run's CA, where `setup` gives one, each of its entries checked: none where it gives
/// neither a certificate nor a host.
pub fn of(setup: &[Vec<u8>]) -> Result<Option<Trust<'_>>, Error> {
    let mut pem = None;
    let mut hosts: Vec<&[u8]> = Vec::new();
    for e in setup {
        if let Some(v) = e.strip_prefix(CA_KEY) {
            if !plain(v) {
                return Err(Error::Certificate);
            }
            if pem.replace(v).is_some() {
                return Err(Error::SecondCa);
            }
        } else if let Some(h) = e.strip_prefix(HOST_KEY) {
            if !plain(h) {
                return Err(Error::Host);
            }
            if !hosts.contains(&h) {
                hosts.push(h);
            }
        }
    }
    match (pem, hosts.is_empty()) {
        (None, true) => Ok(None),
        (Some(pem), false) => Ok(Some(Trust { pem, hosts })),
        _ => Err(Error::Unpaired),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    const PEM: &[u8] = b"-----BEGIN CERTIFICATE-----\nAAEC\n-----END CERTIFICATE-----\n";

    /// The entries read back as written, a host named twice kept once; a run with neither
    /// has none.
    #[test]
    fn the_runs_ca_reads_back_as_written() {
        let setup = vec![
            b"caps=0".to_vec(),
            host(b"api.anthropic.com").unwrap(),
            ca(PEM).unwrap(),
            host(b"chatgpt.com").unwrap(),
            host(b"api.anthropic.com").unwrap(),
        ];
        assert_eq!(
            of(&setup),
            Ok(Some(Trust {
                pem: PEM,
                hosts: vec![b"api.anthropic.com".as_slice(), b"chatgpt.com"],
            }))
        );
        assert_eq!(of(&[b"caps=0".to_vec()]), Ok(None));
    }

    /// Refused: a second certificate, one without a host or a host without one, and an
    /// empty or NUL-holding certificate or host, as read and as made.
    #[test]
    fn a_malformed_runs_ca_is_refused() {
        let (c, h) = (ca(PEM).unwrap(), host(b"chatgpt.com").unwrap());
        assert_eq!(of(&[c.clone(), c.clone(), h.clone()]), Err(Error::SecondCa));
        assert_eq!(of(core::slice::from_ref(&c)), Err(Error::Unpaired));
        assert_eq!(of(core::slice::from_ref(&h)), Err(Error::Unpaired));
        assert_eq!(of(&[CA_KEY.to_vec(), h.clone()]), Err(Error::Certificate));
        assert_eq!(of(&[c.clone(), [HOST_KEY, b"a\0b"].concat()]), Err(Error::Host));
        assert_eq!(of(&[c, HOST_KEY.to_vec()]), Err(Error::Host));
        assert_eq!(ca(b""), Err(Error::Certificate));
        assert_eq!(host(b"x\0"), Err(Error::Host));
    }
}
