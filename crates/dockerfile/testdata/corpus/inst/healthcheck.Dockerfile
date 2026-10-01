FROM a
HEALTHCHECK --interval=30s --timeout=1m30s --start-period=500ms --start-interval=2s --retries=3 CMD curl -f x
HEALTHCHECK CMD ["curl", "x"]
HEALTHCHECK none
HEALTHCHECK --interval=0s CMD x
HEALTHCHECK --retries=0 CMD y
