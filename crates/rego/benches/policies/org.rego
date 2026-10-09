package docker

# An organization's build policy, as buildx's documentation shapes one: images from
# the registries it trusts, pinned, signed by Docker's GitHub builder where they are
# Docker's own; Git sources from its forges, at tags or commits; HTTP sources from the
# hosts it trusts, with a checksum; the local context and Dockerfile; everything else
# refused, with a message for each reason.

default allow := false

trusted_registries := {
	"docker.io",
	"ghcr.io",
	"gcr.io",
	"public.ecr.aws",
	"registry.k8s.io",
	"quay.io",
	"mcr.microsoft.com",
}

docker_official := {"alpine", "busybox", "debian", "ubuntu", "golang", "node", "python", "rust", "nginx", "redis", "postgres"}

trusted_orgs := {"docker", "moby", "library", "distroless", "chainguard", "my-org"}

forges := {"github.com", "gitlab.com", "git.example.org"}

trusted_http_hosts := {"github.com", "objects.githubusercontent.com", "dl-cdn.alpinelinux.org", "go.dev", "static.rust-lang.org"}

blocked_tags := {"latest", "edge", "nightly", "master", "main"}

allow if input.local

allow if {
	input.image
	image_ok
	count(image_violations) == 0
}

allow if {
	input.git
	git_ok
	count(git_violations) == 0
}

allow if {
	input.http
	http_ok
	count(http_violations) == 0
}

image_ok if {
	input.image.host in trusted_registries
}

image_ok if {
	endswith(input.image.host, ".example.org")
}

image_org := org if {
	parts := split(input.image.repo, "/")
	count(parts) > 1
	org := parts[0]
} else := "library"

image_violations contains msg if {
	not image_org in trusted_orgs
	not input.image.repo in docker_official
	msg := sprintf("image %s: organization %q is not trusted", [input.image.ref, image_org])
}

image_violations contains msg if {
	input.image.tag in blocked_tags
	not input.image.checksum
	msg := sprintf("image %s: the floating tag %q must be pinned to a digest", [input.image.ref, input.image.tag])
}

image_violations contains msg if {
	is_docker_image
	not docker_signed
	semver_tag
	msg := sprintf("image %s: Docker's images must carry a docker-github-builder signature", [input.image.ref])
}

image_violations contains msg if {
	some label, value in object.get(input.image, "labels", {})
	startswith(label, "org.example.deprecated")
	msg := sprintf("image %s: deprecated (%s=%s)", [input.image.ref, label, value])
}

image_violations contains msg if {
	some e in object.get(input.image, "env", [])
	regex.match(`^(AWS_SECRET_ACCESS_KEY|GITHUB_TOKEN|NPM_TOKEN)=`, e)
	msg := sprintf("image %s: a secret in its environment (%s)", [input.image.ref, split(e, "=")[0]])
}

image_violations contains msg if {
	input.image.user == "root"
	not root_allowed
	msg := sprintf("image %s: runs as root", [input.image.ref])
}

root_allowed if input.image.repo in {"docker/dockerfile", "moby/buildkit"}

root_allowed if input.env.target == "debug"

is_docker_image if input.image.repo in {"moby/buildkit", "docker/dockerfile", "docker/buildkit-syft-scanner"}

semver_tag if regex.match(`^v?\d+\.\d+(\.\d+)?(-[0-9A-Za-z.-]+)?$`, input.image.tag)

docker_signed if {
	some sig in input.image.signatures
	docker_github_builder_signature(sig, input.image.repo)
}

git_ok if {
	input.git.host in forges
}

git_violations contains msg if {
	not input.git.tag
	not commit_pinned
	not input.git.branch in {"main", "release"}
	msg := sprintf("git %s: use a tag, a release branch or a commit", [input.git.remote])
}

git_violations contains msg if {
	input.git.tag
	not regex.match(`^v\d+\.\d+\.\d+$`, input.git.tagName)
	msg := sprintf("git %s: tag %q is not a release", [input.git.remote, input.git.tagName])
}

commit_pinned if regex.match(`^[0-9a-f]{40}$`, input.git.ref)

http_ok if {
	input.http.schema == "https"
	input.http.host in trusted_http_hosts
}

http_violations contains msg if {
	not input.http.checksum
	msg := sprintf("http %s: needs a checksum", [input.http.url])
}

http_violations contains msg if {
	input.http.hasAuth
	not input.http.host in {"objects.githubusercontent.com"}
	msg := sprintf("http %s: credentials only for objects.githubusercontent.com", [input.http.url])
}

http_violations contains msg if {
	some k, _ in object.get(input.http, "query", {})
	lower(k) in {"token", "access_token", "sig"}
	msg := sprintf("http %s: credentials in the query (%s)", [input.http.url, k])
}

deny_msg contains msg if some msg in image_violations

deny_msg contains msg if some msg in git_violations

deny_msg contains msg if some msg in http_violations

deny_msg contains msg if {
	input.image
	not image_ok
	msg := sprintf("image %s: registry %s is not trusted", [input.image.ref, input.image.host])
}

deny_msg contains msg if {
	input.git
	not git_ok
	msg := sprintf("git %s: forge %s is not trusted", [input.git.remote, input.git.host])
}

deny_msg contains msg if {
	input.http
	not http_ok
	msg := sprintf("http %s: host %s is not trusted", [input.http.url, input.http.host])
}

summary := {
	"kind": kind,
	"violations": count(deny_msg),
	"build_args": count(object.get(input.env, "args", {})),
	"labels": [k | some k, _ in object.get(input.env, "labels", {})],
}

kind := "image" if input.image

kind := "git" if input.git

kind := "http" if input.http

kind := "local" if input.local

decision := {"allow": allow, "deny_msg": [m | some m in deny_msg]}
