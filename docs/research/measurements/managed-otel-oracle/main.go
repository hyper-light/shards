package main

import (
	"bytes"
	"encoding/base64"
	"encoding/json"
	"flag"
	"io"
	"math"
	"os"
	"sort"
	"strconv"

	"go.opentelemetry.io/collector/pdata/pcommon"
	"go.opentelemetry.io/collector/pdata/pmetric"
)

type refusal string

func (r refusal) Error() string { return string(r) }

const (
	inputError       refusal = "input"
	inputBound       refusal = "input_bound"
	jsonError        refusal = "json_shape"
	oneofError       refusal = "oneof"
	duplicateError   refusal = "duplicate_attribute"
	decodeError      refusal = "official_decoder"
	typeError        refusal = "unsupported_metric_type"
	timeError        refusal = "timestamp"
	temporalityError refusal = "temporality"
)

type value struct {
	Kind  string `json:"kind"`
	Value any    `json:"value"`
}

type attribute struct {
	Key string `json:"key"`
	value
}

type point struct {
	Start string `json:"startTimeUnixNano"`
	Time  string `json:"timeUnixNano"`
	Flags string `json:"flags"`
	value
	Attributes []attribute `json:"attributes"`
}

type metric struct {
	Name        string  `json:"name"`
	Unit        string  `json:"unit"`
	Type        string  `json:"type"`
	Temporality string  `json:"temporality,omitempty"`
	Monotonic   *bool   `json:"monotonic,omitempty"`
	Points      []point `json:"points"`
}

type scope struct {
	Name       string      `json:"name"`
	Version    string      `json:"version"`
	Schema     string      `json:"schemaUrl"`
	Attributes []attribute `json:"attributes"`
	Metrics    []metric    `json:"metrics"`
}

type resource struct {
	Schema     string      `json:"schemaUrl"`
	Attributes []attribute `json:"attributes"`
	Scopes     []scope     `json:"scopes"`
}

type receipt struct {
	Decoder   string     `json:"decoder"`
	Resources []resource `json:"resources"`
}

func main() {
	if err := run(os.Args[1:], os.Stdin, os.Stdout); err != nil {
		_ = json.NewEncoder(os.Stderr).Encode(struct {
			Error string `json:"error"`
		}{err.Error()})
		os.Exit(1)
	}
}

func run(args []string, stdin io.Reader, stdout io.Writer) error {
	flags := flag.NewFlagSet("managed-otel-oracle", flag.ContinueOnError)
	flags.SetOutput(io.Discard)
	most := flags.Int64("max-bytes", 0, "required input byte bound")
	path := flags.String("input", "-", "input file or - for stdin")
	parsed := flags.Parse(args)
	if parsed == flag.ErrHelp {
		_, err := io.WriteString(stdout, "Usage: managed-otel-oracle -max-bytes N [-input FILE|-]\n")
		return err
	}
	if parsed != nil || flags.NArg() != 0 || *most <= 0 || *most == math.MaxInt64 {
		return inputBound
	}
	reader := stdin
	if *path != "-" {
		file, err := os.Open(*path)
		if err != nil {
			return inputError
		}
		defer file.Close()
		reader = file
	}
	data, err := io.ReadAll(io.LimitReader(reader, *most+1))
	if err != nil {
		return inputError
	}
	if int64(len(data)) > *most {
		return inputBound
	}
	r, err := decode(data)
	if err != nil {
		return err
	}
	if json.NewEncoder(stdout).Encode(r) != nil {
		return refusal("output")
	}
	return nil
}

func decode(data []byte) (receipt, error) {
	if err := shape(data); err != nil {
		return receipt{}, err
	}
	md, err := (&pmetric.JSONUnmarshaler{}).UnmarshalMetrics(data)
	if err != nil {
		return receipt{}, decodeError
	}
	r := receipt{Decoder: "go.opentelemetry.io/collector/pdata@v1.71.0", Resources: []resource{}}
	groups := md.ResourceMetrics()
	for i := 0; i < groups.Len(); i++ {
		group := groups.At(i)
		attrs, err := attributes(group.Resource().Attributes())
		if err != nil {
			return receipt{}, err
		}
		out := resource{Schema: group.SchemaUrl(), Attributes: attrs, Scopes: []scope{}}
		scopes := group.ScopeMetrics()
		for j := 0; j < scopes.Len(); j++ {
			input := scopes.At(j)
			attrs, err := attributes(input.Scope().Attributes())
			if err != nil {
				return receipt{}, err
			}
			s := scope{Name: input.Scope().Name(), Version: input.Scope().Version(), Schema: input.SchemaUrl(), Attributes: attrs, Metrics: []metric{}}
			metrics := input.Metrics()
			for k := 0; k < metrics.Len(); k++ {
				m, err := normalize(metrics.At(k))
				if err != nil {
					return receipt{}, err
				}
				s.Metrics = append(s.Metrics, m)
			}
			out.Scopes = append(out.Scopes, s)
		}
		r.Resources = append(r.Resources, out)
	}
	return r, nil
}

func normalize(input pmetric.Metric) (metric, error) {
	m := metric{Name: input.Name(), Unit: input.Unit(), Type: input.Type().String(), Points: []point{}}
	var points pmetric.NumberDataPointSlice
	switch input.Type() {
	case pmetric.MetricTypeGauge:
		points = input.Gauge().DataPoints()
	case pmetric.MetricTypeSum:
		sum := input.Sum()
		t := sum.AggregationTemporality()
		if t != pmetric.AggregationTemporalityDelta && t != pmetric.AggregationTemporalityCumulative {
			return metric{}, temporalityError
		}
		m.Temporality = t.String()
		monotonic := sum.IsMonotonic()
		m.Monotonic = &monotonic
		points = sum.DataPoints()
	default:
		return metric{}, typeError
	}
	for i := 0; i < points.Len(); i++ {
		p := points.At(i)
		if p.Timestamp() == 0 || p.StartTimestamp() > p.Timestamp() {
			return metric{}, timeError
		}
		v := value{}
		switch p.ValueType() {
		case pmetric.NumberDataPointValueTypeInt:
			v = value{"Int", strconv.FormatInt(p.IntValue(), 10)}
		case pmetric.NumberDataPointValueTypeDouble:
			v = value{"Double", strconv.FormatFloat(p.DoubleValue(), 'g', -1, 64)}
		default:
			return metric{}, oneofError
		}
		attrs, err := attributes(p.Attributes())
		if err != nil {
			return metric{}, err
		}
		m.Points = append(m.Points, point{Start: strconv.FormatUint(uint64(p.StartTimestamp()), 10), Time: strconv.FormatUint(uint64(p.Timestamp()), 10), Flags: strconv.FormatUint(uint64(p.Flags()), 10), value: v, Attributes: attrs})
	}
	return m, nil
}

func attributes(input pcommon.Map) ([]attribute, error) {
	out := make([]attribute, 0, input.Len())
	seen := make(map[string]struct{}, input.Len())
	var failed error
	input.Range(func(key string, v pcommon.Value) bool {
		if _, exists := seen[key]; exists {
			failed = duplicateError
			return false
		}
		seen[key] = struct{}{}
		out = append(out, attribute{key, normalizeValue(v)})
		return true
	})
	sort.Slice(out, func(i, j int) bool { return out[i].Key < out[j].Key })
	return out, failed
}

func normalizeValue(v pcommon.Value) value {
	out := value{Kind: v.Type().String()}
	switch v.Type() {
	case pcommon.ValueTypeStr:
		out.Value = v.Str()
	case pcommon.ValueTypeInt:
		out.Value = strconv.FormatInt(v.Int(), 10)
	case pcommon.ValueTypeDouble:
		out.Value = strconv.FormatFloat(v.Double(), 'g', -1, 64)
	case pcommon.ValueTypeBool:
		out.Value = v.Bool()
	case pcommon.ValueTypeBytes:
		out.Value = base64.StdEncoding.EncodeToString(v.Bytes().AsRaw())
	case pcommon.ValueTypeMap:
		// Nested maps are receipt data, not identity selection.
		attrs := []attribute{}
		v.Map().Range(func(key string, item pcommon.Value) bool {
			attrs = append(attrs, attribute{key, normalizeValue(item)})
			return true
		})
		sort.Slice(attrs, func(i, j int) bool { return attrs[i].Key < attrs[j].Key })
		out.Value = attrs
	case pcommon.ValueTypeSlice:
		s := v.Slice()
		items := make([]value, 0, s.Len())
		for i := 0; i < s.Len(); i++ {
			items = append(items, normalizeValue(s.At(i)))
		}
		out.Value = items
	}
	return out
}

// This checks JSON structure only. Official pdata supplies every decoded value.
func shape(data []byte) error {
	trimmed := bytes.TrimSpace(data)
	if len(trimmed) == 0 || trimmed[0] != '{' {
		return jsonError
	}
	decoder := json.NewDecoder(bytes.NewReader(data))
	decoder.UseNumber()
	if err := shapeValue(decoder); err != nil {
		return err
	}
	if _, err := decoder.Token(); err != io.EOF {
		return jsonError
	}
	return nil
}

func shapeValue(d *json.Decoder) error {
	token, err := d.Token()
	if err != nil {
		return jsonError
	}
	delimiter, container := token.(json.Delim)
	if !container {
		return nil
	}
	keys := map[string]bool{}
	groups := map[string]bool{}
	for d.More() {
		if delimiter == '{' {
			token, err := d.Token()
			if err != nil {
				return jsonError
			}
			key, ok := token.(string)
			if !ok || keys[key] {
				return jsonError
			}
			keys[key] = true
			group := ""
			switch key {
			case "asInt", "asDouble":
				group = "number"
			case "gauge", "sum", "histogram", "exponentialHistogram", "summary":
				group = "metric"
			case "stringValue", "boolValue", "intValue", "doubleValue", "arrayValue", "kvlistValue", "bytesValue":
				group = "attribute"
			}
			if group != "" && groups[group] {
				return oneofError
			}
			groups[group] = true
		}
		if err := shapeValue(d); err != nil {
			return err
		}
	}
	if _, err := d.Token(); err != nil {
		return jsonError
	}
	return nil
}
