//! A member's MCP connections to its run's controller (D254): a stdio bridge in the member, a
//! socket beside it, and each connection made to that socket carried to the run's host by
//! shards-init's relay, unprompted, one lane of the agent-control link per connection.
//!
//! - **Its ask.** A run asks for a member's listener by naming the member ([`MEMBER_KEY`]),
//!   beside the run-wide token its connections are carried by ([`TOKEN_KEY`], as `engine=`
//!   carries the engine's, D181). A run that names none has neither bridge nor socket (default
//!   deny), and a token alone gives no member a listener.
//! - **In the member.** Its bridge at [`BRIDGE`], shards-init's own binary read only, which a
//!   harness's MCP config names as a stdio server's command, and the socket the bridge reaches
//!   at [`SOCKET`], each seen by that member's processes alone.
//! - **Its connection.** Init's relay dials [`PORT`](super::MCP_PORT), says the token in its
//!   HELLO, then the member's declared index ([`preamble`]), then carries the bytes each way.
//!   The member never holds the token, so no member's connection is told as another's.
//! - **To its host.** The VM process hands each to its run's host as it comes
//!   (`shards_ipc::kind::LANE_ACCEPTED`), numbered from [`ACCEPTED_FIRST`] up, apart from every
//!   lane its host opens (`LANE_OPEN`), whose numbers lie below it.

use alloc::vec::Vec;

use super::TOKEN;
use super::members::{Error, decimal, push_decimal};

/// `mcp=<token>`: the run-wide token its members' MCP connections are carried by, its
/// [`TOKEN`] bytes in lowercase hex.
pub const TOKEN_KEY: &[u8] = b"mcp=";
/// `member-mcp=<index>`: the member, by its declared index, gets a bridge and a socket. One for a
/// member at most.
pub const MEMBER_KEY: &[u8] = b"member-mcp=";
/// The bridge, in each member that has one.
pub const BRIDGE: &str = "/run/shards/mcp";
/// The bridge's name, by which shards-init knows it is run as one: its command's last part.
pub const BRIDGE_NAME: &str = "mcp";
/// The socket the bridge reaches, beside it.
pub const SOCKET: &str = "/run/shards/mcp.sock";
/// The bytes after a connection's HELLO: the member's index, a big-endian u32.
pub const PREAMBLE: usize = 4;
/// The first number the VM process gives a connection it hands its host: the lanes its host
/// opens are numbered below it.
pub const ACCEPTED_FIRST: u64 = 1 << 63;

/// The setup entry giving member `member` a bridge and a socket ([`MEMBER_KEY`]).
pub fn entry(member: u32) -> Vec<u8> {
    let mut entry = Vec::new();
    entry.extend_from_slice(MEMBER_KEY);
    push_decimal(&mut entry, member);
    entry
}

/// The members `setup` gives a bridge and a socket, in its order: every entry checked, and a
/// member named twice refusing the whole.
pub fn members(setup: &[Vec<u8>]) -> Result<Vec<u32>, Error> {
    let mut found = Vec::new();
    for value in setup.iter().filter_map(|e| e.strip_prefix(MEMBER_KEY)) {
        let member = decimal(value)?;
        if found.contains(&member) {
            return Err(Error::SecondMcp);
        }
        found.push(member);
    }
    Ok(found)
}

/// The setup entry carrying the run's token ([`TOKEN_KEY`]).
pub fn token_entry(token: &[u8; TOKEN]) -> Vec<u8> {
    // A nibble's lowercase digit.
    let digit = |n: u8| match n & 0xf {
        n @ 0..=9 => b'0' + n,
        n => b'a' + (n - 10),
    };
    let mut entry = Vec::with_capacity(TOKEN_KEY.len() + 2 * TOKEN);
    entry.extend_from_slice(TOKEN_KEY);
    for &b in token {
        entry.extend_from_slice(&[digit(b >> 4), digit(b)]);
    }
    entry
}

/// The run's token, where `setup` carries one: refused where it is not [`TOKEN`] bytes in hex,
/// or carried twice.
pub fn token(setup: &[Vec<u8>]) -> Result<Option<[u8; TOKEN]>, Error> {
    let mut found = None;
    for value in setup.iter().filter_map(|e| e.strip_prefix(TOKEN_KEY)) {
        let (pairs, []) = value.as_chunks::<2>() else {
            return Err(Error::McpToken);
        };
        let mut token = [0u8; TOKEN];
        if pairs.len() != TOKEN {
            return Err(Error::McpToken);
        }
        let digit = |d: u8| match d {
            b'0'..=b'9' => Ok(d - b'0'),
            b'a'..=b'f' => Ok(d - b'a' + 10),
            _ => Err(Error::McpToken),
        };
        for (byte, &[hi, lo]) in token.iter_mut().zip(pairs) {
            *byte = (digit(hi)? << 4) | digit(lo)?;
        }
        if found.replace(token).is_some() {
            return Err(Error::McpToken);
        }
    }
    Ok(found)
}

/// The bytes after a connection's HELLO naming member `member` ([`PREAMBLE`]).
pub fn preamble(member: u32) -> [u8; PREAMBLE] {
    member.to_be_bytes()
}

/// The member a connection's [`preamble`] names.
pub fn read_preamble(bytes: [u8; PREAMBLE]) -> u32 {
    u32::from_be_bytes(bytes)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// A member's entry names it by its declared index, once; its token reads back as written,
    /// whole, once, lowercase hex alone.
    #[test]
    fn a_runs_mcp_entries_read_back_as_written() {
        let setup = [entry(2), b"readonly".to_vec(), entry(0)];
        assert_eq!(members(&setup), Ok(alloc::vec![2, 0]));
        assert_eq!(members(&[]), Ok(Vec::new()));
        assert_eq!(members(&[entry(1), entry(1)]), Err(Error::SecondMcp));
        assert!(members(&[b"member-mcp=01".to_vec()]).is_err());
        let token = [0xa5u8; TOKEN];
        let written = token_entry(&token);
        assert_eq!(written.len(), TOKEN_KEY.len() + 2 * TOKEN);
        assert_eq!(super::token(core::slice::from_ref(&written)), Ok(Some(token)));
        assert_eq!(super::token(&[]), Ok(None));
        assert_eq!(super::token(&[written.clone(), written]), Err(Error::McpToken));
        for bad in [&b"mcp=a5"[..], b"mcp=", &[TOKEN_KEY, &[b'A'; 2 * TOKEN]].concat()] {
            assert_eq!(super::token(&[bad.to_vec()]), Err(Error::McpToken), "{bad:?}");
        }
        assert_eq!(read_preamble(preamble(7)), 7);
        assert_eq!(preamble(0x0102_0304), [1, 2, 3, 4]);
        const { assert!(ACCEPTED_FIRST > u32::MAX as u64) };
    }
}
