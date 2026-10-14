// Captures what Docker's log drivers send (scripts/logdrivers/generate): UDP datagrams
// on :5514, TCP and TLS streams on :5515 and :5516, a unix datagram socket and a unix
// stream socket in DIR, and, where JOURNAL is given, the journal's socket there. Each
// datagram, and each connection's bytes, goes to a file of its own in DIR/out, named for
// its transport and the order it came in. The TLS listener's certificate, for 127.0.0.1
// and localhost, is made as it starts, and its certificate is DIR/ca.pem, which the
// syslog driver's syslog-tls-ca-cert names.
//
//	capture DIR [JOURNAL]
package main

import (
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/tls"
	"crypto/x509"
	"crypto/x509/pkix"
	"encoding/pem"
	"fmt"
	"io"
	"log"
	"math/big"
	"net"
	"os"
	"path/filepath"
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
	if err := os.WriteFile(filepath.Join(dir, "ready"), nil, 0o644); err != nil {
		log.Fatal(err)
	}
	select {}
}
