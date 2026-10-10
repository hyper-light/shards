// D113: decodes the spy's capture of a frontend's gateway traffic into a transcript: each
// HTTP/2 frame of both directions, the headers HPACK decodes, and each gRPC message
// decoded with BuildKit's own gateway types (protojson). Also writes each direction's
// raw bytes, for shards' tests.
package main

import (
	"bufio"
	"bytes"
	"encoding/binary"
	"encoding/hex"
	"fmt"
	"io"
	"os"
	"strings"

	pb "github.com/moby/buildkit/frontend/gateway/pb"
	"golang.org/x/net/http2"
	"golang.org/x/net/http2/hpack"
	"google.golang.org/protobuf/encoding/protojson"
	"google.golang.org/protobuf/proto"
)

type rpc struct {
	req, resp func() proto.Message
}

var methods = map[string]rpc{
	"Ping":               {func() proto.Message { return &pb.PingRequest{} }, func() proto.Message { return &pb.PongResponse{} }},
	"Inputs":             {func() proto.Message { return &pb.InputsRequest{} }, func() proto.Message { return &pb.InputsResponse{} }},
	"Solve":              {func() proto.Message { return &pb.SolveRequest{} }, func() proto.Message { return &pb.SolveResponse{} }},
	"ReadFile":           {func() proto.Message { return &pb.ReadFileRequest{} }, func() proto.Message { return &pb.ReadFileResponse{} }},
	"ReadDir":            {func() proto.Message { return &pb.ReadDirRequest{} }, func() proto.Message { return &pb.ReadDirResponse{} }},
	"StatFile":           {func() proto.Message { return &pb.StatFileRequest{} }, func() proto.Message { return &pb.StatFileResponse{} }},
	"Evaluate":           {func() proto.Message { return &pb.EvaluateRequest{} }, func() proto.Message { return &pb.EvaluateResponse{} }},
	"ResolveImageConfig": {func() proto.Message { return &pb.ResolveImageConfigRequest{} }, func() proto.Message { return &pb.ResolveImageConfigResponse{} }},
	"ResolveSourceMeta":  {func() proto.Message { return &pb.ResolveSourceMetaRequest{} }, func() proto.Message { return &pb.ResolveSourceMetaResponse{} }},
	"Return":             {func() proto.Message { return &pb.ReturnRequest{} }, func() proto.Message { return &pb.ReturnResponse{} }},
	"Warn":               {func() proto.Message { return &pb.WarnRequest{} }, func() proto.Message { return &pb.WarnResponse{} }},
}

func main() {
	f, err := os.Open(os.Args[1])
	if err != nil {
		panic(err)
	}
	var in, out bytes.Buffer
	sc := bufio.NewScanner(f)
	sc.Buffer(make([]byte, 64<<20), 64<<20)
	for sc.Scan() {
		line := sc.Text()
		i := strings.Index(line, "SHARDS-D113-SPY ")
		if i < 0 {
			continue
		}
		parts := strings.Fields(line[i:])
		if len(parts) != 4 || (parts[1] != "IN" && parts[1] != "OUT") {
			continue
		}
		b, err := hex.DecodeString(parts[3])
		if err != nil {
			panic(err)
		}
		if parts[1] == "IN" {
			in.Write(b)
		} else {
			out.Write(b)
		}
	}
	prefix := os.Args[2]
	os.WriteFile(prefix+".client.bin", out.Bytes(), 0o644)
	os.WriteFile(prefix+".server.bin", in.Bytes(), 0o644)
	client := out.Bytes()
	preface := []byte(http2.ClientPreface)
	if !bytes.HasPrefix(client, preface) {
		panic("no client preface")
	}
	paths := map[uint32]string{}
	fmt.Println("== client (frontend -> BuildKit)")
	walk("C", bytes.NewReader(client[len(preface):]), paths, true)
	fmt.Println("== server (BuildKit -> frontend)")
	walk("S", bytes.NewReader(in.Bytes()), paths, false)
}

func walk(side string, r io.Reader, paths map[uint32]string, client bool) {
	fr := http2.NewFramer(nil, r)
	fr.MaxHeaderListSize = 1 << 24
	fr.ReadMetaHeaders = hpack.NewDecoder(4096, nil)
	data := map[uint32]*bytes.Buffer{}
	for {
		frame, err := fr.ReadFrame()
		if err != nil {
			fmt.Printf("%s end: %v\n", side, err)
			return
		}
		h := frame.Header()
		switch f := frame.(type) {
		case *http2.SettingsFrame:
			var s []string
			f.ForeachSetting(func(st http2.Setting) error { s = append(s, st.String()); return nil })
			fmt.Printf("%s SETTINGS flags=%v %v\n", side, h.Flags, s)
		case *http2.MetaHeadersFrame:
			var hs []string
			for _, hf := range f.Fields {
				hs = append(hs, hf.Name+"="+hf.Value)
				if hf.Name == ":path" {
					paths[h.StreamID] = hf.Value
				}
			}
			fmt.Printf("%s HEADERS stream=%d end_stream=%v %v\n", side, h.StreamID, f.StreamEnded(), hs)
		case *http2.DataFrame:
			fmt.Printf("%s DATA stream=%d len=%d end_stream=%v\n", side, h.StreamID, len(f.Data()), f.StreamEnded())
			b := data[h.StreamID]
			if b == nil {
				b = &bytes.Buffer{}
				data[h.StreamID] = b
			}
			b.Write(f.Data())
			for b.Len() >= 5 {
				raw := b.Bytes()
				n := binary.BigEndian.Uint32(raw[1:5])
				if uint32(len(raw)-5) < n {
					break
				}
				msg := raw[5 : 5+n]
				method := paths[h.StreamID]
				name := method[strings.LastIndex(method, "/")+1:]
				m, ok := methods[name]
				if !ok {
					fmt.Printf("%s   message %s (%d bytes, compressed=%d) undecoded\n", side, method, n, raw[0])
				} else {
					var v proto.Message
					if client {
						v = m.req()
					} else {
						v = m.resp()
					}
					if err := proto.Unmarshal(msg, v); err != nil {
						fmt.Printf("%s   message %s: %v\n", side, method, err)
					} else {
						j, _ := protojson.Marshal(v)
						fmt.Printf("%s   message %s (%d bytes) %s\n", side, name, n, j)
					}
				}
				b.Next(int(5 + n))
			}
		case *http2.WindowUpdateFrame:
			fmt.Printf("%s WINDOW_UPDATE stream=%d inc=%d\n", side, h.StreamID, f.Increment)
		case *http2.PingFrame:
			fmt.Printf("%s PING ack=%v %x\n", side, f.IsAck(), f.Data)
		case *http2.GoAwayFrame:
			fmt.Printf("%s GOAWAY last=%d code=%v\n", side, f.LastStreamID, f.ErrCode)
		case *http2.RSTStreamFrame:
			fmt.Printf("%s RST_STREAM stream=%d code=%v\n", side, h.StreamID, f.ErrCode)
		default:
			fmt.Printf("%s %v\n", side, h)
		}
	}
}
