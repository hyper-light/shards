#!/usr/bin/env python3
# M249 (docs/research/platform-measurements.md): the user each official image runs as, and the
# volumes it declares, from its config on Docker Hub (anonymous pulls of linux/amd64 configs
# alone): which of them a fresh volume's owner touches (D38).
#
#   python3 -I docs/research/measurements/volume-owners/survey.py
import json
import urllib.request

IMAGES = ["postgres", "mysql", "mariadb", "redis", "mongo", "nginx", "httpd", "rabbitmq", "memcached",
          "cassandra", "elasticsearch", "influxdb", "couchdb", "neo4j", "ghost", "wordpress", "nextcloud",
          "jenkins/jenkins", "grafana/grafana", "sonarqube", "registry", "traefik", "consul", "vault",
          "minio/minio", "node", "python", "busybox", "alpine", "ubuntu", "solr", "zookeeper", "eclipse-mosquitto",
          "odoo", "drupal", "joomla", "mediawiki", "redmine", "matomo", "percona"]
def get(url, token=None, accept=None):
    req = urllib.request.Request(url)
    if token: req.add_header("Authorization", f"Bearer {token}")
    if accept: req.add_header("Accept", accept)
    with urllib.request.urlopen(req, timeout=30) as r:
        return json.load(r)
ACCEPT = ", ".join(["application/vnd.oci.image.index.v1+json", "application/vnd.docker.distribution.manifest.list.v2+json",
                    "application/vnd.oci.image.manifest.v1+json", "application/vnd.docker.distribution.manifest.v2+json"])
for name in IMAGES:
    repo = name if "/" in name else f"library/{name}"
    try:
        token = get(f"https://auth.docker.io/token?service=registry.docker.io&scope=repository:{repo}:pull")["token"]
        m = get(f"https://registry-1.docker.io/v2/{repo}/manifests/latest", token, ACCEPT)
        if "manifests" in m:
            pick = next((d for d in m["manifests"] if d.get("platform", {}).get("architecture") == "amd64" and d.get("platform", {}).get("os") == "linux"), None)
            m = get(f"https://registry-1.docker.io/v2/{repo}/manifests/{pick['digest']}", token, ACCEPT)
        cfg = get(f"https://registry-1.docker.io/v2/{repo}/blobs/{m['config']['digest']}", token)
        c = cfg.get("config", {})
        print(json.dumps({"image": name, "user": c.get("User", ""), "volumes": sorted((c.get("Volumes") or {}).keys())}))
    except Exception as e:
        print(json.dumps({"image": name, "error": str(e)[:120]}))
