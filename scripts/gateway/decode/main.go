// D113: decodes the spy's capture of a frontend's gateway traffic into a transcript: each
// HTTP/2 frame of both directions, the headers HPACK decodes, and each gRPC message
// decoded with BuildKit's own gateway types (protojson). Also writes each direction's
// raw bytes, for shards' tests, and times each call: from the chunk that ended its
// request to the one that ended its answer, and the process's start, first byte and end
// (CALL and PROCESS lines, nanoseconds, for D113's measurements).
package main

import (
	"bufio"
	"bytes"
	"encoding/binary"
	"encoding/hex"
	"fmt"
	"io"
	"os"
	"sort"
	"strconv"
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

// chunk is where a chunk of one direction ended, and when it was relayed.
type chunk struct {
	end int
	ts  int64
}

// when is the time of the chunk that holds byte `off` of a direction.
func when(chunks []chunk, off int) int64 {
	for _, c := range chunks {
		if off <= c.end {
			return c.ts
		}
	}
	return 0
}

// counter counts what a reader has given.
type counter struct {
	r io.Reader
	n int
}

func (c *counter) Read(p []byte) (int, error) {
	n, err := c.r.Read(p)
	c.n += n
	return n, err
}

var (
	inChunks, outChunks []chunk
	requestEnd          = map[uint32]int64{}
	responseEnd         = map[uint32]int64{}
)

func main() {
	f, err := os.Open(os.Args[1])
	if err != nil {
		panic(err)
	}
	var in, out bytes.Buffer
	var start, exit int64
	sc := bufio.NewScanner(f)
	sc.Buffer(make([]byte, 64<<20), 64<<20)
	for sc.Scan() {
		line := sc.Text()
		i := strings.Index(line, "SHARDS-D113-SPY ")
		if i < 0 {
			continue
		}
		parts := strings.Fields(line[i:])
		if len(parts) >= 3 && parts[1] == "START" {
			start, _ = strconv.ParseInt(parts[2], 10, 64)
		}
		if len(parts) >= 3 && parts[1] == "EXIT" {
			exit, _ = strconv.ParseInt(parts[2], 10, 64)
		}
		if len(parts) != 4 || (parts[1] != "IN" && parts[1] != "OUT") {
			continue
		}
		b, err := hex.DecodeString(parts[3])
		if err != nil {
			panic(err)
		}
		ts, _ := strconv.ParseInt(parts[2], 10, 64)
		if parts[1] == "IN" {
			in.Write(b)
			inChunks = append(inChunks, chunk{in.Len(), ts})
		} else {
			out.Write(b)
			outChunks = append(outChunks, chunk{out.Len(), ts})
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
	var first int64
	if len(outChunks) > 0 {
		first = outChunks[0].ts
	}
	fmt.Printf("PROCESS start=%d first_out=%d exit=%d\n", start, first, exit)
	ids := make([]int, 0, len(requestEnd))
	for id := range requestEnd {
		ids = append(ids, int(id))
	}
	sort.Ints(ids)
	for _, id := range ids {
		s := uint32(id)
		method := paths[s][strings.LastIndex(paths[s], "/")+1:]
		fmt.Printf("CALL stream=%d method=%s request_end=%d response_end=%d ns=%d\n", s, method, requestEnd[s], responseEnd[s], responseEnd[s]-requestEnd[s])
	}
}

func walk(side string, r io.Reader, paths map[uint32]string, client bool) {
	cr := &counter{r: r}
	// The client's offsets count the preface the walk starts after.
	base := 0
	chunks := inChunks
	if client {
		base = len(http2.ClientPreface)
		chunks = outChunks
	}
	fr := http2.NewFramer(nil, cr)
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
		// The framer reads a frame whole: the counter has its end.
		ts := when(chunks, base+cr.n)
		if h.Flags.Has(http2.FlagDataEndStream) && (h.Type == http2.FrameData || h.Type == http2.FrameHeaders) {
			if client {
				requestEnd[h.StreamID] = ts
			} else {
				responseEnd[h.StreamID] = ts
			}
		}
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
