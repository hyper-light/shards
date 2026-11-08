FROM mirror.gcr.io/library/node:22-alpine
RUN echo before; grep -E '^(MemTotal|MemFree|Cached|Shmem|AnonPages|Active\(file\)|Inactive\(file\)):' /proc/meminfo
RUN mkdir /app && cd /app && npm init -y >/dev/null && npm install --no-audit --no-fund typescript@5 eslint@9 webpack@5 >/dev/null 2>&1 && du -sh /app/node_modules
RUN echo after; grep -E '^(MemTotal|MemFree|Cached|Shmem|AnonPages|Active\(file\)|Inactive\(file\)):' /proc/meminfo; cat /sys/fs/cgroup/memory.stat 2>/dev/null | grep -E '^(file|shmem|anon|inactive_file|active_file) '
