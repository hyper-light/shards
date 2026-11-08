target "base" {
  target = "base"
}

target "app" {
  target = "app"
  contexts = {
    base = "target:base"
  }
}
