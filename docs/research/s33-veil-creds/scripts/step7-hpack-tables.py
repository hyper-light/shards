#!/usr/bin/env python3
"""Step 33, step 7 (D242): HPACK's static table and Huffman code, as Rust, from RFC 7541's own text.

Prints crates/net/src/hpack.rs's STATIC (Appendix A, Table 1) and HUFFMAN (Appendix B: each symbol's
code, aligned to the least significant bit, and its length in bits), so that no entry is typed by
hand. hpack.rs's tests read the same text and hold the tables to it.
Usage: python3 -I step7-hpack-tables.py RFC7541.TXT
"""
import re
import sys

text = open(sys.argv[1], encoding="ascii").read()
appendix_a = text[text.index("Appendix A.  Static Table Definition") : text.index("Appendix B.  Huffman Code")]
appendix_b = text[text.index("Appendix B.  Huffman Code") : text.index("Appendix C.  Examples")]

static = re.findall(r"^\s+\| (\d+)\s+\| (\S+)\s+\| (.*?)\s*\|$", appendix_a, re.M)
assert [int(i) for i, _, _ in static] == list(range(1, 62)), "Table 1 holds indices 1 to 61"
print("pub(crate) const STATIC: [(&[u8], &[u8]); 61] = [")
for _, name, value in static:
    print(f'    (b"{name}", b"{value}"),')
print("];")

codes = re.findall(r"^\s+(?:'.'\s*)?\(\s*(\d+)\)\s+\|[01|]+\s+([0-9a-f]+)\s+\[\s*(\d+)\]$", appendix_b, re.M)
eos = re.search(r"^\s+EOS \(256\)\s+\|[01|]+\s+([0-9a-f]+)\s+\[\s*(\d+)\]$", appendix_b, re.M)
assert [int(s) for s, _, _ in codes] == list(range(256)) and eos, "Appendix B holds symbols 0 to 255 and EOS"
print("pub(crate) const HUFFMAN: [(u32, u8); 257] = [")
for _, code, bits in codes + [("256", eos.group(1), eos.group(2))]:
    print(f"    (0x{code}, {bits}),")
print("];")
