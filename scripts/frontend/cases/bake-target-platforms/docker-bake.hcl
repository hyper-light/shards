target "base" {
  target = "base"
  platforms = ["linux/amd64", "linux/arm64"]
}

target "app" {
  target = "app"
  platforms = ["linux/amd64", "linux/arm64"]
  contexts = {
    base = "target:base"
  }
}
