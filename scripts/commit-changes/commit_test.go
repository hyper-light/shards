package dockerfile

// What moby's dockerd makes of `docker commit --change` and `docker import --change`, for
// shards to match (crates/dockerfile/src/commit.rs, testdata/commit-changes.json).
// `generate` copies this file into moby's daemon/builder/dockerfile and runs it there, on
// Linux, as dockerd runs: BuildFromConfig applies each case's changes to its base config,
// and the test records the config it returns, as the API writes it, or its error.

import (
	"context"
	"encoding/json"
	"os"
	"testing"
	"time"

	"github.com/moby/moby/api/types/container"
	"github.com/moby/moby/api/types/network"
)

func shardsBases() map[string]*container.Config {
	timeout := 10
	return map[string]*container.Config{
		"empty": {},
		"busybox": {
			Env:   []string{"PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"},
			Cmd:   []string{"sh"},
			Image: "sha256:6fd955f66c231c1a946653170d096a1f2c5b8c4e9ab1ecba5a0f2fd3e8c8d3e5",
		},
		"full": {
			Hostname:     "abc123",
			User:         "root",
			AttachStdout: true,
			ExposedPorts: network.PortSet{network.MustParsePort("80/tcp"): {}, network.MustParsePort("53/udp"): {}},
			OpenStdin:    true,
			Env:          []string{"PATH=/usr/bin:/bin", "FOO=bar", "HOME=/root", "BARE", "PORTS=8080 9090/udp", "SIG=SIGQUIT"},
			Cmd:          []string{"/bin/sh", "-c", "echo hi"},
			Healthcheck: &container.HealthConfig{
				Test:     []string{"CMD-SHELL", "true"},
				Interval: 30 * time.Second,
				Retries:  2,
			},
			ArgsEscaped: true,
			Image:       "base:latest",
			Volumes:     map[string]struct{}{"/data": {}},
			WorkingDir:  "/app",
			Entrypoint:  []string{"/entry"},
			OnBuild:     []string{"RUN echo built"},
			Labels:      map[string]string{"a": "b", "maintainer": "me"},
			StopSignal:  "SIGTERM",
			StopTimeout: &timeout,
			Shell:       []string{"/bin/bash", "-eo", "pipefail", "-c"},
		},
	}
}

type shardsCase struct {
	name    string
	base    string
	os      string
	changes []string
}

func shardsCases() []shardsCase {
	c := func(name, base string, changes ...string) shardsCase {
		return shardsCase{name: name, base: base, os: "linux", changes: changes}
	}
	w := func(name, base, osName string, changes ...string) shardsCase {
		return shardsCase{name: name, base: base, os: osName, changes: changes}
	}
	return []shardsCase{
		// No changes, or nothing in them.
		c("none-empty", "empty"),
		c("none-full", "full"),
		c("blank", "busybox", ""),
		c("spaces", "busybox", "   "),
		c("comment-only", "busybox", "# a comment"),
		c("blank-lines", "busybox", "", "", "ENV A=1", ""),

		// CMD.
		c("cmd-exec", "busybox", `CMD ["echo", "hi"]`),
		c("cmd-shell", "busybox", `CMD echo hi`),
		c("cmd-shell-custom", "full", `CMD echo hi`),
		c("cmd-empty-json", "busybox", `CMD []`),
		c("cmd-bare", "busybox", `CMD`),
		c("cmd-no-expansion", "full", `CMD echo $HOME`),
		c("cmd-lowercase", "busybox", `cmd ["x"]`),
		c("cmd-mixed-case", "busybox", `Cmd ["x"]`),
		c("cmd-bad-json", "busybox", `CMD ["a",`),
		c("cmd-json-spaces", "busybox", `CMD [ "a" , "b c" ]`),
		c("cmd-args-escaped", "full", `CMD ["x"]`),
		c("cmd-flag", "busybox", `CMD --foo=bar x`),
		c("cmd-windows", "busybox", `CMD echo hi`),
		w("cmd-windows-os", "busybox", "windows", `CMD echo hi`),
		w("cmd-windows-os-exec", "full", "windows", `CMD ["x"]`),
		c("cmd-twice", "busybox", `CMD ["a"]`, `CMD ["b"]`),
		c("cmd-continuation", "busybox", `CMD echo \`, `  hi`),
		c("cmd-quotes", "busybox", `CMD echo "a b" 'c'`),
		c("cmd-unicode", "busybox", `CMD ["écho", "日本"]`),

		// ENTRYPOINT.
		c("entrypoint-exec", "busybox", `ENTRYPOINT ["/e"]`),
		c("entrypoint-shell", "busybox", `ENTRYPOINT /e -x`),
		c("entrypoint-shell-custom", "full", `ENTRYPOINT /e -x`),
		c("entrypoint-empty-json", "full", `ENTRYPOINT []`),
		c("entrypoint-bare", "full", `ENTRYPOINT`),
		c("cmd-then-entrypoint", "busybox", `CMD ["c"]`, `ENTRYPOINT ["/e"]`),
		c("entrypoint-then-cmd", "busybox", `ENTRYPOINT ["/e"]`, `CMD ["c"]`),
		c("cmd-empty-then-entrypoint", "busybox", `CMD []`, `ENTRYPOINT ["/e"]`),
		c("cmd-shell-then-entrypoint", "busybox", `CMD echo`, `ENTRYPOINT ["/e"]`),
		c("entrypoint-args-escaped", "full", `ENTRYPOINT ["/e"]`),
		w("entrypoint-windows-os", "full", "windows", `ENTRYPOINT /e`),
		c("entrypoint-flag", "busybox", `ENTRYPOINT --x /e`),

		// HEALTHCHECK.
		c("health-none", "busybox", `HEALTHCHECK NONE`),
		c("health-none-lower", "busybox", `healthcheck none`),
		c("health-shell", "busybox", `HEALTHCHECK CMD curl -f http://localhost/`),
		c("health-exec", "busybox", `HEALTHCHECK CMD ["curl", "-f", "http://localhost/"]`),
		c("health-lower-cmd", "busybox", `HEALTHCHECK cmd true`),
		c("health-options", "busybox", `HEALTHCHECK --interval=5s --timeout=3s --start-period=1m --start-interval=2s --retries=3 CMD true`),
		c("health-zero-interval", "busybox", `HEALTHCHECK --interval=0s CMD true`),
		c("health-fractional", "busybox", `HEALTHCHECK --interval=1.5s --timeout=1ms CMD true`),
		c("health-too-short", "busybox", `HEALTHCHECK --interval=1us CMD true`),
		c("health-bad-duration", "busybox", `HEALTHCHECK --timeout=soon CMD true`),
		c("health-negative-retries", "busybox", `HEALTHCHECK --retries=-1 CMD true`),
		c("health-bad-retries", "busybox", `HEALTHCHECK --retries=abc CMD true`),
		c("health-zero-retries", "full", `HEALTHCHECK --retries=0 CMD true`),
		c("health-none-args", "busybox", `HEALTHCHECK NONE x`),
		c("health-unknown-type", "busybox", `HEALTHCHECK FOO x`),
		c("health-missing-cmd", "busybox", `HEALTHCHECK CMD`),
		c("health-bare", "busybox", `HEALTHCHECK`),
		c("health-unknown-flag", "busybox", `HEALTHCHECK --bogus=1 CMD true`),
		c("health-flag-no-value", "busybox", `HEALTHCHECK --interval CMD true`),
		c("health-override", "full", `HEALTHCHECK CMD ["true"]`),
		c("health-none-override", "full", `HEALTHCHECK NONE`),
		c("health-no-expansion", "full", `HEALTHCHECK CMD echo $HOME`),

		// ENV.
		c("env-one", "busybox", `ENV A=1`),
		c("env-legacy", "busybox", `ENV A 1`),
		c("env-legacy-spaces", "busybox", `ENV A  one two  three`),
		c("env-many", "busybox", `ENV A=1 B=2 C=3`),
		c("env-path", "busybox", `ENV PATH=/opt/bin:$PATH`),
		c("env-earlier-change", "busybox", `ENV A=1`, `ENV B=$A`),
		c("env-same-instruction", "busybox", `ENV A=1 B=$A`),
		c("env-same-instruction-old", "full", `ENV FOO=new B=$FOO`),
		c("env-default", "busybox", `ENV B=${UNSET:-def}`),
		c("env-default-set", "full", `ENV B=${FOO:-def}`),
		c("env-default-empty", "busybox", `ENV E=`, `ENV B=${E:-def} C=${E-def}`),
		c("env-alternate", "full", `ENV B=${FOO:+set} C=${UNSET:+set}`),
		c("env-required", "busybox", `ENV B=${UNSET:?is required}`),
		c("env-required-plain", "busybox", `ENV B=${UNSET?}`),
		c("env-double-quoted", "busybox", `ENV B="quoted value $PATH"`),
		c("env-single-quoted", "full", `ENV B='single $FOO'`),
		c("env-escaped-dollar", "full", `ENV B=\$FOO`),
		c("env-escaped-space", "busybox", `ENV B=a\ b`),
		c("env-replace", "full", `ENV FOO=new`),
		c("env-replace-bare", "full", `ENV BARE=now`),
		c("env-missing-value", "busybox", `ENV A`),
		c("env-blank-name", "busybox", `ENV =x`),
		c("env-bare", "busybox", `ENV`),
		c("env-key-expansion", "full", `ENV $FOO=v`),
		c("env-unterminated-quote", "busybox", `ENV A="open`),
		c("env-unterminated-brace", "busybox", `ENV A=${B`),
		c("env-bad-substitution", "busybox", `ENV A=${B!x}`),
		c("env-unicode", "busybox", `ENV NAME=日本語 É=é`),
		c("env-newline-in-change", "busybox", "ENV A=1\nCMD [\"x\"]"),
		c("env-continuation", "busybox", `ENV A=1 \`, `B=2`),
		c("env-continuation-eof", "busybox", `ENV A=1 \`),
		c("env-prefix-trim", "full", `ENV P=${PATH#/usr} Q=${PATH##*:} R=${PATH%:*} S=${PATH%%:*}`),
		c("env-replace-pattern", "full", `ENV P=${PATH/bin/sbin} Q=${PATH//bin/sbin}`),
		c("env-self", "full", `ENV FOO=${FOO}-more`),
		c("env-null-base", "empty", `ENV A=1`),
		c("env-bare-reference", "full", `ENV B=x${BARE}y`),
		c("env-escape-directive", "busybox", "# escape=`", "ENV A=C:\\dir B=x`$y"),
		c("env-bad-escape-directive", "busybox", "# escape=x", "ENV A=1"),
		c("env-flag", "busybox", `ENV --foo A=1`),
		c("env-dollar-at-end", "busybox", `ENV A=cost$`),
		c("env-braced-number", "busybox", `ENV A=${1}`),
		c("env-lowercase", "busybox", `env a=b`),

		// EXPOSE.
		c("expose-one", "busybox", `EXPOSE 80`),
		c("expose-many", "busybox", `EXPOSE 80/udp 443/tcp 22`),
		c("expose-range", "busybox", `EXPOSE 8000-8003`),
		c("expose-range-udp", "busybox", `EXPOSE 5000-5001/udp`),
		c("expose-upper-proto", "busybox", `EXPOSE 80/TCP 81/Udp`),
		c("expose-sctp", "busybox", `EXPOSE 9/sctp`),
		c("expose-bad-proto", "busybox", `EXPOSE 80/http`),
		c("expose-not-a-port", "busybox", `EXPOSE abc`),
		c("expose-too-big", "busybox", `EXPOSE 70000`),
		c("expose-backwards", "busybox", `EXPOSE 90-80`),
		c("expose-same-range", "busybox", `EXPOSE 80-80`),
		c("expose-bad-end", "busybox", `EXPOSE 80-x`),
		c("expose-split-words", "full", `EXPOSE $PORTS`),
		c("expose-default", "busybox", `EXPOSE ${P:-8080}`),
		c("expose-unset", "busybox", `EXPOSE $UNSET`),
		c("expose-host-ip", "busybox", `EXPOSE 127.0.0.1:8080:80`),
		c("expose-host-port", "busybox", `EXPOSE 8080:80`),
		c("expose-host-range", "busybox", `EXPOSE 8080-8081:80-81`),
		c("expose-host-range-mismatch", "busybox", `EXPOSE 8080-8082:80-81`),
		c("expose-host-range-dynamic", "busybox", `EXPOSE 8080-8090:80`),
		c("expose-bad-host-port", "busybox", `EXPOSE x:80`),
		c("expose-ipv6", "busybox", `EXPOSE [::1]:80:80`),
		c("expose-ipv6-bad-bracket", "busybox", `EXPOSE [::1:80:80`),
		c("expose-bad-ip", "busybox", `EXPOSE bad.ip:80:80`),
		c("expose-empty-ip", "busybox", `EXPOSE ::80`),
		c("expose-many-colons", "busybox", `EXPOSE 1:2:3:80`),
		c("expose-only-proto", "busybox", `EXPOSE /tcp`),
		c("expose-empty-proto", "busybox", `EXPOSE 80/`),
		c("expose-full-range", "busybox", `EXPOSE 0-65535`),
		c("expose-max", "busybox", `EXPOSE 65535`),
		c("expose-zero", "busybox", `EXPOSE 0`),
		c("expose-leading-zero", "busybox", `EXPOSE 080`),
		c("expose-plus", "busybox", `EXPOSE +80`),
		c("expose-quoted", "busybox", `EXPOSE "80" '443'`),
		c("expose-add", "full", `EXPOSE 80 81`),
		c("expose-bare", "busybox", `EXPOSE`),
		c("expose-flag", "busybox", `EXPOSE --x 80`),
		c("expose-nested-default", "busybox", `EXPOSE 80${P:-/udp}`),
		c("expose-from-env", "busybox", `ENV A=1`, `EXPOSE ${A}000`),
		c("expose-bad-expansion", "busybox", `EXPOSE ${A`),
		c("expose-ipv4-zone", "busybox", `EXPOSE 1.2.3:80:80`),

		// LABEL.
		c("label-one", "busybox", `LABEL a=b`),
		c("label-quoted", "busybox", `LABEL "a b"="c d"`),
		c("label-many", "busybox", `LABEL a=b c=d`),
		c("label-legacy", "busybox", `LABEL a b c`),
		c("label-expansion", "full", `LABEL a=$HOME`),
		c("label-single-quoted", "full", `LABEL a='$HOME'`),
		c("label-empty-value", "busybox", `LABEL a=`),
		c("label-bare", "busybox", `LABEL`),
		c("label-override", "full", `LABEL a=new`),
		c("label-blank-name", "busybox", `LABEL =x`),
		c("label-duplicate", "busybox", `LABEL a=b a=c`),
		c("label-key-expansion", "full", `LABEL $FOO=1`),
		c("label-dotted", "busybox", `LABEL org.opencontainers.image.title="My App"`),
		c("label-escaped-quote", "busybox", `LABEL a="say \"hi\""`),
		c("label-from-env", "busybox", `ENV V=1.2`, `LABEL version=$V`),
		c("label-flag", "busybox", `LABEL --x a=b`),

		// ONBUILD.
		c("onbuild-run", "busybox", `ONBUILD RUN echo hi`),
		c("onbuild-add", "busybox", `ONBUILD ADD . /app`),
		c("onbuild-spaces", "busybox", `onbuild   run   x`),
		c("onbuild-append", "full", `ONBUILD RUN two`),
		c("onbuild-onbuild", "busybox", `ONBUILD ONBUILD RUN x`),
		c("onbuild-from", "busybox", `ONBUILD FROM x`),
		c("onbuild-maintainer", "busybox", `ONBUILD MAINTAINER x`),
		c("onbuild-bare", "busybox", `ONBUILD`),
		c("onbuild-no-expansion", "full", `ONBUILD RUN echo $HOME`),
		c("onbuild-unknown", "busybox", `ONBUILD FOO bar`),
		c("onbuild-json", "busybox", `ONBUILD CMD ["a", "b"]`),
		c("onbuild-two", "busybox", `ONBUILD RUN a`, `ONBUILD RUN b`),
		c("onbuild-heredoc", "busybox", "ONBUILD RUN <<EOF\necho hi\nEOF"),
		c("onbuild-bad-run-flag", "busybox", `ONBUILD RUN --bogus x`),

		// STOPSIGNAL.
		c("stopsignal-name", "busybox", `STOPSIGNAL SIGKILL`),
		c("stopsignal-number", "busybox", `STOPSIGNAL 9`),
		c("stopsignal-short", "busybox", `STOPSIGNAL kill`),
		c("stopsignal-rt", "busybox", `STOPSIGNAL SIGRTMIN+3`),
		c("stopsignal-rtmax", "busybox", `STOPSIGNAL rtmax-1`),
		c("stopsignal-rtmin-bare", "busybox", `STOPSIGNAL RTMIN`),
		c("stopsignal-rtmax-bare", "busybox", `STOPSIGNAL SIGRTMAX`),
		c("stopsignal-rtmin-past", "busybox", `STOPSIGNAL SIGRTMIN+16`),
		c("stopsignal-rtmin-zero", "busybox", `STOPSIGNAL SIGRTMIN+0`),
		c("stopsignal-rtmax-past", "busybox", `STOPSIGNAL SIGRTMAX-15`),
		c("stopsignal-lower-sig", "busybox", `STOPSIGNAL sigterm`),
		c("stopsignal-zero", "busybox", `STOPSIGNAL 0`),
		c("stopsignal-unknown", "busybox", `STOPSIGNAL SIGFOO`),
		c("stopsignal-expansion", "full", `STOPSIGNAL $SIG`),
		c("stopsignal-unset", "busybox", `STOPSIGNAL $SIG`),
		w("stopsignal-windows", "busybox", "windows", `STOPSIGNAL SIGKILL`),
		w("stopsignal-windows-bad", "busybox", "windows", `STOPSIGNAL ${A`),
		w("stopsignal-no-os", "busybox", "", `STOPSIGNAL SIGKILL`),
		c("stopsignal-bare", "busybox", `STOPSIGNAL`),
		c("stopsignal-negative", "busybox", `STOPSIGNAL -5`),
		c("stopsignal-plus", "busybox", `STOPSIGNAL +15`),
		c("stopsignal-darwin-only", "busybox", `STOPSIGNAL SIGINFO`),
		c("stopsignal-two", "busybox", `STOPSIGNAL A B`),
		c("stopsignal-huge", "busybox", `STOPSIGNAL 99999999999999999999`),
		c("stopsignal-sig-only", "busybox", `STOPSIGNAL SIG`),
		c("stopsignal-flag-like", "busybox", `STOPSIGNAL --x`),

		// USER.
		c("user-name", "busybox", `USER nobody`),
		c("user-ids", "busybox", `USER 1000:1000`),
		c("user-expansion", "full", `ENV U=app`, `USER $U`),
		c("user-default", "busybox", `USER ${U:-root}`),
		c("user-bare", "busybox", `USER`),
		c("user-two", "busybox", `USER a b`),
		c("user-override", "full", `USER app`),
		c("user-flag", "busybox", `USER --x a`),

		// VOLUME.
		c("volume-one", "busybox", `VOLUME /data`),
		c("volume-json", "busybox", `VOLUME ["/a", "/b"]`),
		c("volume-list", "busybox", `VOLUME /a /b`),
		c("volume-expansion", "full", `VOLUME $HOME ${HOME}2`),
		c("volume-empty-json", "busybox", `VOLUME []`),
		c("volume-empty-string", "busybox", `VOLUME [""]`),
		c("volume-space-string", "busybox", `VOLUME ["  "]`),
		c("volume-unset", "busybox", `VOLUME ${UNSET}`),
		c("volume-add", "full", `VOLUME /more`),
		c("volume-bare", "busybox", `VOLUME`),
		c("volume-json-unicode", "busybox", `VOLUME ["/日本"]`),
		c("volume-relative", "busybox", `VOLUME data`),

		// WORKDIR.
		c("workdir-abs", "busybox", `WORKDIR /app`),
		c("workdir-rel", "full", `WORKDIR sub`),
		c("workdir-rel-empty", "busybox", `WORKDIR sub`),
		c("workdir-clean", "busybox", `WORKDIR /a/../b/./c/`),
		c("workdir-expansion", "full", `WORKDIR $HOME/x`),
		c("workdir-chain", "busybox", `WORKDIR a`, `WORKDIR b`, `WORKDIR ../c`),
		c("workdir-bare", "busybox", `WORKDIR`),
		c("workdir-unset", "busybox", `WORKDIR ${UNSET}`),
		c("workdir-double-slash", "busybox", `WORKDIR //x//y`),
		c("workdir-up", "full", `WORKDIR ..`),
		c("workdir-up-root", "busybox", `WORKDIR ../../..`),
		w("workdir-windows-os", "full", "windows", `WORKDIR C:\x`),
		c("workdir-quoted", "busybox", `WORKDIR "/quoted dir"`),
		c("workdir-two", "busybox", `WORKDIR /a /b`),
		c("workdir-env", "busybox", `ENV A=1`, `WORKDIR /$A`),
		c("workdir-dot", "full", `WORKDIR .`),
		c("workdir-escape-directive", "busybox", "# escape=`", `WORKDIR C:\dir`),

		// Not change commands.
		c("invalid-from", "busybox", `FROM busybox`),
		c("invalid-from-lower", "busybox", `from busybox`),
		c("invalid-run", "busybox", `RUN echo hi`),
		c("invalid-copy", "busybox", `COPY a b`),
		c("invalid-add", "busybox", `ADD a b`),
		c("invalid-arg", "busybox", `ARG x=1`),
		c("invalid-shell", "busybox", `SHELL ["/bin/bash", "-c"]`),
		c("invalid-maintainer", "busybox", `MAINTAINER me`),
		c("invalid-unknown", "busybox", `FOO bar`),
		c("invalid-after-valid", "busybox", `ENV A=1`, `RUN x`),
		c("invalid-before-parse-error", "busybox", `RUN x`, `ENV A`),
		c("parse-error-before-invalid", "busybox", `ENV A`, `RUN x`),
		c("dispatch-error-then-invalid", "busybox", `STOPSIGNAL bogus`, `RUN x`),
		c("invalid-mixed-case", "busybox", `Run x`),

		// Several at once.
		c("all", "busybox",
			`ENV APP=/srv/app PORT=8080`,
			`WORKDIR $APP`,
			`EXPOSE $PORT 9000/udp`,
			`VOLUME ["/srv/data"]`,
			`USER app:app`,
			`LABEL org.example.port=$PORT`,
			`STOPSIGNAL SIGINT`,
			`HEALTHCHECK --interval=10s CMD wget -q localhost:$PORT`,
			`ONBUILD RUN make`,
			`ENTRYPOINT ["/srv/app/run"]`,
			`CMD ["--serve"]`),
		c("all-on-full", "full",
			`ENV FOO=baz`,
			`CMD run`,
			`ENTRYPOINT exec me`,
			`WORKDIR next`,
			`EXPOSE 443`),
		c("dispatch-error-midway", "busybox", `ENV A=1`, `WORKDIR ${UNSET}`, `USER x`),
		c("one-change-many-lines", "busybox", "ENV A=1\nUSER a\nWORKDIR /w"),
		c("comment-between", "busybox", `ENV A=1`, `# note`, `USER $A`),
	}
}

func TestShardsCommit(t *testing.T) {
	out := os.Getenv("SHARDS_COMMIT_OUT")
	if out == "" {
		t.Skip("SHARDS_COMMIT_OUT names the file to write")
	}
	bases := shardsBases()
	type record struct {
		Name    string          `json:"name"`
		OS      string          `json:"os"`
		Base    json.RawMessage `json:"base"`
		Changes []string        `json:"changes"`
		Config  json.RawMessage `json:"config,omitempty"`
		Error   *string         `json:"error,omitempty"`
	}
	var records []record
	seen := map[string]bool{}
	for _, tc := range shardsCases() {
		if seen[tc.name] {
			t.Fatalf("case %s twice", tc.name)
		}
		seen[tc.name] = true
		base, ok := bases[tc.base]
		if !ok {
			t.Fatalf("case %s: no base %s", tc.name, tc.base)
		}
		baseJSON, err := json.Marshal(base)
		if err != nil {
			t.Fatal(err)
		}
		// Each case gets its own copy, as each commit reads the container's.
		var cfg container.Config
		if err := json.Unmarshal(baseJSON, &cfg); err != nil {
			t.Fatal(err)
		}
		changes := tc.changes
		if changes == nil {
			changes = []string{}
		}
		r := record{Name: tc.name, OS: tc.os, Base: baseJSON, Changes: changes}
		got, err := BuildFromConfig(context.Background(), &cfg, tc.changes, tc.os)
		if err != nil {
			msg := err.Error()
			r.Error = &msg
		} else {
			b, err := json.Marshal(got)
			if err != nil {
				t.Fatal(err)
			}
			r.Config = b
		}
		records = append(records, r)
	}
	b, err := json.MarshalIndent(records, "", " ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(out, append(b, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}
