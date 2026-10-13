//! PM M214: a process that links nothing past libSystem, against `metal.rs`.

fn main() {
    if std::env::args().nth(1).as_deref() == Some("hold") {
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
}
