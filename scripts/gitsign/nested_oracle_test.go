package main

// go-crypto's answers (as buildx v0.37.1 vendors it, v1.4.1) to signature packets that
// embed signatures in turn, for crates/gitsign/tests/adversarial.rs: what packet.Reader's
// Next makes of each. The packets are built here as the Rust test builds them (`level`,
// `nested`), the deep one recorded by its SHA-256 alone. `generate-nested` copies this
// file into buildx's cmd/buildx and runs it there.

import (
	"bytes"
	"crypto/sha256"
	"encoding/base64"
	"encoding/binary"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"os"
	"testing"

	"github.com/ProtonMail/go-crypto/openpgp/packet"
)

// nestedLevel is a v6 signature's body (Ed25519, SHA-256) of type sigType, with a creation
// time or none, embedding each of children.
func nestedLevel(sigType byte, withTime bool, children ...[]byte) []byte {
	var hashed []byte
	if withTime {
		hashed = append(hashed, 5, 2, 0, 0, 0, 1)
	}
	for _, c := range children {
		hashed = append(hashed, 255)
		hashed = binary.BigEndian.AppendUint32(hashed, uint32(1+len(c)))
		hashed = append(hashed, 32)
		hashed = append(hashed, c...)
	}
	out := []byte{6, sigType, 27, 8}
	out = binary.BigEndian.AppendUint32(out, uint32(len(hashed)))
	out = append(out, hashed...)
	out = binary.BigEndian.AppendUint32(out, 0)
	out = append(out, 0, 0, 16)
	out = append(out, make([]byte, 16)...)
	out = append(out, make([]byte, 64)...)
	return out
}

// nestedPacket frames a signature body as a new-format packet of a five-byte length.
func nestedPacket(body []byte) []byte {
	out := []byte{0xC2, 255}
	out = binary.BigEndian.AppendUint32(out, uint32(len(body)))
	return append(out, body...)
}

// nestedDeep is a packet whose signature embeds one, depth deep, as the Rust test's
// nested_signature builds it: built outside in, each level's prefix then each suffix.
func nestedDeep(depth int) []byte {
	const suffix = 4 + 2 + 1 + 16 + 64
	const prefixEmbedding = 1 + 3 + 4 + 6 + 5 + 1
	const prefixLeaf = 1 + 3 + 4 + 6
	sizes := make([]int, depth+1)
	sizes[depth] = prefixLeaf + suffix
	for k := depth - 1; k >= 0; k-- {
		sizes[k] = prefixEmbedding + sizes[k+1] + suffix
	}
	out := []byte{0xC2, 255}
	out = binary.BigEndian.AppendUint32(out, uint32(sizes[0]))
	for k := 0; k <= depth; k++ {
		sigType := byte(0x19)
		if k == 0 {
			sigType = 0
		}
		out = append(out, 6, sigType, 27, 8)
		if k == depth {
			out = binary.BigEndian.AppendUint32(out, 6)
			out = append(out, 5, 2, 0, 0, 0, 1)
		} else {
			child := sizes[k+1]
			out = binary.BigEndian.AppendUint32(out, uint32(6+5+1+child))
			out = append(out, 5, 2, 0, 0, 0, 1, 255)
			out = binary.BigEndian.AppendUint32(out, uint32(1+child))
			out = append(out, 32)
		}
	}
	for k := 0; k <= depth; k++ {
		out = binary.BigEndian.AppendUint32(out, 0)
		out = append(out, 0, 0, 16)
		out = append(out, make([]byte, 16)...)
		out = append(out, make([]byte, 64)...)
	}
	return out
}

type nestedCase struct {
	Name   string `json:"name"`
	Packet string `json:"packet,omitempty"`
	SHA256 string `json:"sha256,omitempty"`
	Depth  int    `json:"depth,omitempty"`
	Answer string `json:"answer"`
}

// nestedAnswer is what Next makes of a packet: its error, or the signature's type and
// its embedded signature's.
func nestedAnswer(data []byte) string {
	p, err := packet.NewReader(bytes.NewReader(data)).Next()
	if err != nil {
		return err.Error()
	}
	sig, ok := p.(*packet.Signature)
	if !ok {
		return fmt.Sprintf("%T", p)
	}
	if sig.EmbeddedSignature == nil {
		return fmt.Sprintf("ok %d none", sig.SigType)
	}
	return fmt.Sprintf("ok %d %d", sig.SigType, sig.EmbeddedSignature.SigType)
}

func TestShardsNested(t *testing.T) {
	out := os.Getenv("SHARDS_NESTED_OUT")
	if out == "" {
		t.Skip("SHARDS_NESTED_OUT names the file to write")
	}
	leaf := func(sigType byte) []byte { return nestedLevel(sigType, true) }
	short := nestedLevel(0x19, true)
	short = short[:len(short)-1]
	small := []struct {
		name string
		body []byte
	}{
		{"wrong type two down", nestedLevel(0, true, nestedLevel(0x19, true, leaf(0)))},
		{"no creation time one down", nestedLevel(0, true, nestedLevel(0x19, false, leaf(0x19)))},
		{"two embedded one down", nestedLevel(0, true, nestedLevel(0x19, true, leaf(0x19), leaf(0x19)))},
		{"two embedded at the top", nestedLevel(0, true, leaf(0x19), leaf(0x19))},
		{"sound, three deep", nestedLevel(0, true, nestedLevel(0x19, true, nestedLevel(0x19, true, leaf(0x19))))},
		{"embedded cut short", nestedLevel(0, true, short)},
		{"wrong type one down", nestedLevel(0, true, leaf(0x13))},
		{"embedded signature v4 in a v6", nestedLevel(0, true, []byte{4, 0x19, 27, 8, 0, 0})},
	}
	var cases []nestedCase
	for _, c := range small {
		p := nestedPacket(c.body)
		cases = append(cases, nestedCase{
			Name:   c.name,
			Packet: base64.StdEncoding.EncodeToString(p),
			Answer: nestedAnswer(p),
		})
	}
	for _, depth := range []int{1, 20000} {
		p := nestedDeep(depth)
		sum := sha256.Sum256(p)
		cases = append(cases, nestedCase{
			Name:   fmt.Sprintf("deep %d", depth),
			SHA256: hex.EncodeToString(sum[:]),
			Depth:  depth,
			Answer: nestedAnswer(p),
		})
	}
	data, err := json.MarshalIndent(cases, "", "  ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(out, append(data, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}
