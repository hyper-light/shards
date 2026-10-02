// Writes crates/cmdline/tests/docker-time.json: what the Docker client sends for `logs
// --since` and `--until` (GetTimestamp: client/timestamp.go, docker/cli's vendored moby
// client/internal/timestamp/timestamp.go) at a fixed instant in two zones, and what
// dockerd makes of the timestamps it receives (ParseUnixTimestamp: daemon/timestamp.go,
// moby's daemon/internal/timestamp/timestamp.go). `generate` copies both in unchanged.
package main

import (
	"encoding/json"
	"fmt"
	"os"
	"time"

	client "github.com/docker/cli/shards-time/client"
	daemon "github.com/docker/cli/shards-time/daemon"
)

type since struct {
	Value  string `json:"value"`
	Offset int    `json:"offset"`
	Result string `json:"result,omitempty"`
	Error  string `json:"error,omitempty"`
}

// time.ParseDuration of a value, as pflag reads a duration flag, and what the duration
// prints as (Duration.String, which pflag shows for one).
type duration struct {
	Value  string `json:"value"`
	Ns     int64  `json:"ns"`
	Shown  string `json:"shown,omitempty"`
	Error  string `json:"error,omitempty"`
}

type unix struct {
	Value string `json:"value"`
	Ns    *int64 `json:"ns,omitempty"`
	Error string `json:"error,omitempty"`
}

var values = []string{
	// Durations back from now.
	"10m", "1h30m", "1.5h", ".5s", "5.s", "-10m", "+10m", "0", "0s", "-0", "1us",
	"1µs", "1μs", "3ns", "300ms", "1h1h", "", "h", "1", "1x", "1.2.3s",
	"9223372036854775807ns", "9223372036854775808ns", "2562047h47m16.854775807s",
	"2562047h47m16.854775808s", "-2562047h47m16.854775808s", "1.0000000000000000000001s",
	"0.1h", "1.123456789123h", ".s", "-.s", "1e9s", "1 s",
	// Times, each layout.
	"2013-01-02T13:23:37Z", "2013-01-02T13:23:37", "2013-01-02T13:23:37.123456789Z",
	"2013-01-02T13:23:37.1Z", "2013-01-02T13:23:37,5Z", "2013-01-02T13:23:37.123456789",
	"2013-01-02T13:23:37.1234567891234Z", "2013-01-02T13", "2013-01-02T13:23",
	"2013-01-02T13Z", "2013-01-02T13:23Z", "2013-01-02T13:23:37+01:00",
	"2013-01-02T13:23:37-05:30", "2013-01-02T13:23+01:00", "2013-01-02T13+01:00",
	"2013-01-02", "2013-01-02Z", "2013-01-02+01:00", "2013-01-02-05:00",
	"2013-01-02T13:23:37z", "2013-01-02t13:23:37Z", "2013-01-02T13:23:37.5",
	// Out of range, malformed, and extra text.
	"2013-13-02", "2013-00-02", "2013-02-30", "2013-01-02T24:00:00Z", "2013-01-02T13:60:00Z",
	"2013-01-02T13:23:60Z", "2013-01-02T13:23:37+25:00", "2013-01-02T13:23:37+01:61",
	"2013-01-02T13:23:37*01:00", "2013-01-02T13:23:37+0100", "2013-01-02T1:23:37Z",
	"2013-1-02", "13-01-02", "2013/01/02", "2013-01-02T13:23:37Zextra", "2013-01-02x",
	"2013-01-02T13:23:37 ", "-x", "x", "Z", "T", "2013-01-02T", "2013-01-02T13:",
	"2013-01-02T13:23:37.Z", "2013-01-02T13:23:37.", "２０１３-01-02",
	"2013-01-02T13:23:37é", "2013-01-02T13:23:37\"x", "2013-01-02T13:23:37\\x",
	"2024-02-29", "2023-02-29", "0000-01-01", "9999-12-31T23:59:59Z",
	"2013-01-02T13:23:37.999999999999Z", "2013-01-02T13:23:37.123-01:00",
	// Unix timestamps.
	"1600000000", "1600000000.5", "1600000000.123456789", "1.+5", "1.2.3", "+5", "-5",
	"1e9", "99999999999999999999", "1.99999999999999999999", "1.-5",
}

var timestamps = []string{
	"", "0", "5", "-5", "+5", "1600000000.5", "1600000000.000000001",
	"1.123456789012345678901", "1.12345678901234567890", "1.+5", "1.-5", "1.5x", "x", "1.",
	"99999999999999999999", "1.0", "1.000000000", "1.00000000000", ".5", "1.é",
}

func main() {
	// A fixed instant, as each zone's clock shows it.
	now := time.Date(2026, 9, 29, 11, 5, 37, 123456789, time.UTC)
	zones := []*time.Location{time.FixedZone("EST", -5*3600), time.FixedZone("IST", 5*3600+1800)}
	var sinces []since
	for _, zone := range zones {
		reference := now.In(zone)
		_, offset := reference.Zone()
		for _, v := range values {
			ts, err := client.GetTimestamp(v, reference)
			s := since{Value: v, Offset: offset, Result: ts}
			if err != nil {
				s.Result, s.Error = "", err.Error()
			}
			sinces = append(sinces, s)
		}
	}
	var unixes []unix
	for _, v := range timestamps {
		t, err := daemon.ParseUnixTimestamp(v)
		u := unix{Value: v}
		if err != nil {
			u.Error = err.Error()
		} else if !t.IsZero() {
			ns := t.UnixNano()
			u.Ns = &ns
		}
		unixes = append(unixes, u)
	}
	var durations []duration
	for _, v := range values {
		d, err := time.ParseDuration(v)
		r := duration{Value: v, Ns: int64(d), Shown: d.String()}
		if err != nil {
			r.Ns, r.Shown, r.Error = 0, "", err.Error()
		}
		durations = append(durations, r)
	}
	for _, ns := range []int64{0, 1, 999, 1000, 1500, 999999, 1000000, 1234567, 999999999, 1000000000,
		1500000000, 59999999999, 60000000000, 90000000000, 3599999999999, 3600000000000,
		3661001000000, 86400000000000, -1, -1500000000, -3661001000000, 9223372036854775807,
		-9223372036854775808} {
		durations = append(durations, duration{Value: "", Ns: ns, Shown: time.Duration(ns).String()})
	}
	out, err := json.MarshalIndent(map[string]any{
		"now_ns": now.UnixNano(), "since": sinces, "unix": unixes, "durations": durations,
	}, "", "  ")
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
	fmt.Println(string(out))
}
