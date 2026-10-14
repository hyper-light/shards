// Captures what Docker's log drivers send (scripts/logdrivers/generate): UDP datagrams
// on :5514, TCP and TLS streams on :5515 and :5516, a unix datagram socket and a unix
// stream socket in DIR, a Splunk HTTP Event Collector's requests on :8088 and, over TLS,
// :8089, CloudWatch Logs' on :4566, where JOURNAL is given the journal's socket there, and
// where CREDS is given an ECS task's credentials endpoint at that address. Each
// datagram, and each connection's bytes, goes to a file of its own in DIR/out, named for
// its transport and the order it came in. The TLS listener's certificate, for 127.0.0.1
// and localhost, is made as it starts, and its certificate is DIR/ca.pem, which the
// syslog driver's syslog-tls-ca-cert names.
//
//	capture DIR [JOURNAL [CREDS]]
package main

import (
	"bytes"
	"compress/gzip"
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/tls"
	"crypto/x509"
	"crypto/x509/pkix"
	"encoding/json"
	"encoding/pem"
	"fmt"
	"io"
	"log"
	"math/big"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"time"
)

var seq atomic.Int64

func save(dir, kind string, b []byte) {
	name := filepath.Join(dir, "out", fmt.Sprintf("%06d-%s", seq.Add(1), kind))
	if err := os.WriteFile(name, b, 0o644); err != nil {
		log.Fatal(err)
	}
}

func packets(dir, kind string, pc net.PacketConn) {
	buf := make([]byte, 1<<16)
	for {
		n, _, err := pc.ReadFrom(buf)
		if err != nil {
			return
		}
		save(dir, kind, append([]byte(nil), buf[:n]...))
	}
}

func streams(dir, kind string, l net.Listener) {
	for {
		c, err := l.Accept()
		if err != nil {
			return
		}
		go func() {
			b, _ := io.ReadAll(c)
			save(dir, kind, b)
		}()
	}
}

// journal records each entry journald's socket is sent: its datagram's bytes, or, where
// it is passed a descriptor instead (go-systemd's file in /dev/shm, for an entry too long
// for a datagram), that file's.
func journal(dir string, c *net.UnixConn) {
	buf := make([]byte, 1<<20)
	oob := make([]byte, 256)
	for {
		n, oobn, _, _, err := c.ReadMsgUnix(buf, oob)
		if err != nil {
			return
		}
		entry, kind := append([]byte(nil), buf[:n]...), "journald"
		msgs, err := syscall.ParseSocketControlMessage(oob[:oobn])
		if err != nil {
			log.Fatal(err)
		}
		for _, m := range msgs {
			fds, err := syscall.ParseUnixRights(&m)
			if err != nil {
				log.Fatal(err)
			}
			for _, fd := range fds {
				f := os.NewFile(uintptr(fd), "entry")
				info, err := f.Stat()
				if err != nil {
					log.Fatal(err)
				}
				// Its offset is its sender's, past what it wrote: read it from the start.
				entry = make([]byte, info.Size())
				if _, err := f.ReadAt(entry, 0); err != nil && err != io.EOF {
					log.Fatal(err)
				}
				f.Close()
				kind = "journald-fd"
			}
		}
		save(dir, kind, entry)
	}
}

// hec records each request made of the Splunk HTTP Event Collector it stands in for: its
// method, its path, the fields a driver sets and its body, unzipped where it says it is
// gzip's; and answers as HEC does, with success.
func hec(dir, kind string, l net.Listener) {
	_ = http.Serve(l, http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		body, err := io.ReadAll(r.Body)
		if err != nil {
			log.Fatal(err)
		}
		if r.Header.Get("Content-Encoding") == "gzip" {
			zr, err := gzip.NewReader(bytes.NewReader(body))
			if err != nil {
				log.Fatal(err)
			}
			if body, err = io.ReadAll(zr); err != nil {
				log.Fatal(err)
			}
		}
		rec, err := json.Marshal(map[string]any{
			"method":        r.Method,
			"path":          r.URL.Path,
			"query":         r.URL.RawQuery,
			"authorization": r.Header.Get("Authorization"),
			"encoding":      r.Header.Get("Content-Encoding"),
			"type":          r.Header.Get("Content-Type"),
			"channel":       r.Header.Get("X-Splunk-Request-Channel"),
			"body":          string(body),
		})
		if err != nil {
			log.Fatal(err)
		}
		save(dir, kind, rec)
		w.Header().Set("Content-Type", "application/json; charset=UTF-8")
		_, _ = io.WriteString(w, `{"text":"Success","code":0}`)
	}))
}

// cloudwatch records each request made of the CloudWatch Logs it stands in for, its
// JSON 1.1 API's (X-Amz-Target Logs_20140328.*): its method, path, query, every field and
// its body, whole, as its signature covers them; and answers as CloudWatch Logs does. Its
// log groups are `oracle`'s alone until a CreateLogGroup makes another; a stream in a
// group it has not is ResourceNotFoundException's.
func cloudwatch(dir string, l net.Listener) {
	var mu sync.Mutex
	groups := map[string]bool{"oracle": true}
	_ = http.Serve(l, http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		body, err := io.ReadAll(r.Body)
		if err != nil {
			log.Fatal(err)
		}
		fields := map[string]string{"host": r.Host}
		for k, v := range r.Header {
			fields[strings.ToLower(k)] = strings.Join(v, ",")
		}
		rec, err := json.Marshal(map[string]any{
			"method": r.Method,
			"path":   r.URL.EscapedPath(),
			"query":  r.URL.RawQuery,
			"fields": fields,
			"body":   string(body),
		})
		if err != nil {
			log.Fatal(err)
		}
		save(dir, "cwl", rec)
		var asked struct {
			LogGroupName string `json:"logGroupName"`
		}
		_ = json.Unmarshal(body, &asked)
		w.Header().Set("Content-Type", "application/x-amz-json-1.1")
		mu.Lock()
		defer mu.Unlock()
		switch r.Header.Get("X-Amz-Target") {
		case "Logs_20140328.CreateLogGroup":
			groups[asked.LogGroupName] = true
			_, _ = io.WriteString(w, "{}")
		case "Logs_20140328.CreateLogStream":
			if !groups[asked.LogGroupName] {
				w.WriteHeader(http.StatusBadRequest)
				_, _ = io.WriteString(w, `{"__type":"ResourceNotFoundException","message":"The specified log group does not exist."}`)
				return
			}
			_, _ = io.WriteString(w, "{}")
		case "Logs_20140328.PutLogEvents":
			_, _ = io.WriteString(w, `{"nextSequenceToken":"49039859608939181393924181739426935212924181385185"}`)
		default:
			w.WriteHeader(http.StatusBadRequest)
			_, _ = io.WriteString(w, `{"__type":"UnknownOperationException"}`)
		}
	}))
}

// credentials answers as an ECS task's credentials endpoint does, with AWS's documented
// example keys, which sign nothing anywhere but here.
func credentials(l net.Listener) {
	_ = http.Serve(l, http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		_, _ = io.WriteString(w, `{"AccessKeyId":"AKIDEXAMPLE","SecretAccessKey":"wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY","Token":"example-session-token","Expiration":"2099-01-01T00:00:00Z"}`)
	}))
}

func main() {
	dir := os.Args[1]
	if err := os.MkdirAll(filepath.Join(dir, "out"), 0o755); err != nil {
		log.Fatal(err)
	}
	udp, err := net.ListenPacket("udp", "127.0.0.1:5514")
	if err != nil {
		log.Fatal(err)
	}
	go packets(dir, "udp", udp)
	tcp, err := net.Listen("tcp", "127.0.0.1:5515")
	if err != nil {
		log.Fatal(err)
	}
	go streams(dir, "tcp", tcp)

	key, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		log.Fatal(err)
	}
	tmpl := x509.Certificate{
		SerialNumber:          big.NewInt(1),
		Subject:               pkix.Name{CommonName: "localhost"},
		NotBefore:             time.Now().Add(-time.Hour),
		NotAfter:              time.Now().Add(24 * time.Hour),
		KeyUsage:              x509.KeyUsageDigitalSignature | x509.KeyUsageCertSign,
		ExtKeyUsage:           []x509.ExtKeyUsage{x509.ExtKeyUsageServerAuth},
		BasicConstraintsValid: true,
		IsCA:                  true,
		DNSNames:              []string{"localhost"},
		IPAddresses:           []net.IP{net.ParseIP("127.0.0.1")},
	}
	der, err := x509.CreateCertificate(rand.Reader, &tmpl, &tmpl, &key.PublicKey, key)
	if err != nil {
		log.Fatal(err)
	}
	ca := pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: der})
	if err := os.WriteFile(filepath.Join(dir, "ca.pem"), ca, 0o644); err != nil {
		log.Fatal(err)
	}
	cert := tls.Certificate{Certificate: [][]byte{der}, PrivateKey: key}
	tl, err := tls.Listen("tcp", "127.0.0.1:5516", &tls.Config{Certificates: []tls.Certificate{cert}})
	if err != nil {
		log.Fatal(err)
	}
	go streams(dir, "tls", tl)
	hl, err := net.Listen("tcp", "127.0.0.1:8088")
	if err != nil {
		log.Fatal(err)
	}
	go hec(dir, "hec", hl)
	hsl, err := tls.Listen("tcp", "127.0.0.1:8089", &tls.Config{Certificates: []tls.Certificate{cert}})
	if err != nil {
		log.Fatal(err)
	}
	go hec(dir, "hecs", hsl)
	cw, err := net.Listen("tcp", "127.0.0.1:4566")
	if err != nil {
		log.Fatal(err)
	}
	go cloudwatch(dir, cw)

	gram, err := net.ListenPacket("unixgram", filepath.Join(dir, "gram.sock"))
	if err != nil {
		log.Fatal(err)
	}
	go packets(dir, "unixgram", gram)
	stream, err := net.Listen("unix", filepath.Join(dir, "stream.sock"))
	if err != nil {
		log.Fatal(err)
	}
	go streams(dir, "unix", stream)
	if len(os.Args) > 2 {
		j, err := net.ListenUnixgram("unixgram", &net.UnixAddr{Name: os.Args[2], Net: "unixgram"})
		if err != nil {
			log.Fatal(err)
		}
		go journal(dir, j)
	}
	if len(os.Args) > 3 {
		creds, err := net.Listen("tcp", os.Args[3])
		if err != nil {
			log.Fatal(err)
		}
		go credentials(creds)
	}
	if err := os.WriteFile(filepath.Join(dir, "ready"), nil, 0o644); err != nil {
		log.Fatal(err)
	}
	select {}
}
