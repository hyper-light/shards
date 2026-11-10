use std::io::{self, Write};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CatalogFormat {
    Mcp,
    CodexDynamic,
}

/// `descriptors` is the canonical server's immutable peer_tool_catalog view.
/// Native registration reuses its actual three MCP schemas; it grants nothing.
pub(crate) fn catalog(
    out: &mut impl Write,
    format: CatalogFormat,
    descriptors: impl Iterator<Item = &'static str>,
) -> io::Result<()> {
    out.write_all(match format {
        CatalogFormat::Mcp => b"{\"tools\":[",
        CatalogFormat::CodexDynamic => b"{\"type\":\"namespace\",\"name\":\"shards\",\"description\":\"Shards peer operations\",\"tools\":[",
    })?;
    let mut separator: &[u8] = b"";
    for descriptor in descriptors {
        out.write_all(separator)?;
        match format {
            CatalogFormat::Mcp => out.write_all(descriptor.as_bytes())?,
            CatalogFormat::CodexDynamic => {
                // The inspected canonical peer descriptors contain exactly the
                // native function's name, description and inputSchema fields.
                let members = descriptor.strip_prefix('{').ok_or(io::ErrorKind::InvalidData)?;
                out.write_all(b"{\"type\":\"function\",")?;
                out.write_all(members.as_bytes())?;
            }
        }
        separator = b",";
    }
    out.write_all(b"]}")
}
