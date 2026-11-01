"""Where a template's memory file holds data (PM M378, D151): its extents (SEEK_DATA and
SEEK_HOLE), the 2 MiB blocks they touch, and the runs those blocks make, data and holes, which
are the parts a restore on Windows maps (memory::DATA_BLOCK, platform::map_file_views): a view
of the file for each run of data, memory of its own for each run between.

    python3 -I layout.py MEMORY_FILE [RAM_BYTES]

RAM_BYTES, where given, counts only the file's RAM, the memory plugged after it left out.
"""

import os
import sys

BLOCK = 2 << 20


def main() -> None:
    path = sys.argv[1]
    fd = os.open(path, os.O_RDONLY)
    size = os.fstat(fd).st_size
    ram = int(sys.argv[2]) if len(sys.argv) > 2 else size
    extents = []
    at = 0
    while at < ram:
        try:
            data = os.lseek(fd, at, os.SEEK_DATA)
        except OSError:
            break
        if data >= ram:
            break
        hole = min(os.lseek(fd, data, os.SEEK_HOLE), ram)
        extents.append((data, hole))
        at = hole
    blocks = set()
    for data, hole in extents:
        blocks.update(range(data // BLOCK, (hole - 1) // BLOCK + 1))
    count = (ram + BLOCK - 1) // BLOCK
    runs = {True: 0, False: 0}
    last = None
    for b in range(count):
        held = b in blocks
        if held != last:
            runs[held] += 1
        last = held
    print(f"{path}: {size >> 20} MiB, its RAM {ram >> 20} MiB, {len(extents)} extents of data, "
          f"{sum(h - d for d, h in extents) >> 20} MiB")
    print(f"2 MiB blocks: {count}, {len(blocks)} with data, in {runs[True]} runs; "
          f"{runs[False]} runs between: {runs[True] + runs[False]} parts")


if __name__ == "__main__":
    main()
