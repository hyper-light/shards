fn read_stat(mut file: std::fs::File) -> io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        loop {
            // One further byte is required to observe data or EOF; no protocol cap or
            // guessed read size. Unlike std's small probe, growth is always fallible.
            bytes
                .try_reserve(1)
                .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
            let used = bytes.len();
            // capacity is already owned; initializing up to it cannot grow the Vec.
            bytes.resize(bytes.capacity(), 0);
            let count = loop {
                match file.read(bytes.get_mut(used..).ok_or(io::ErrorKind::InvalidData)?) {
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    result => break result?,
                }
            };
            let filled = used
                .checked_add(count)
                .filter(|filled| *filled <= bytes.len())
                .ok_or(io::ErrorKind::InvalidData)?;
            bytes.truncate(filled);
            if count == 0 {
                return Ok(bytes);
            }
        }
    }
