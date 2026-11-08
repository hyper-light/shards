//! Each run's writable layer (D190): an ext4 file system in a file of its own, which its
//! microVM maps as a pmem device and its guest mounts with DAX under its root's overlay.
//!
//! A microVM booted for a run is given one made empty here, of the run's size; one
//! restored from a snapshot (a template's, or a run's own, D176) is given a copy-on-write
//! clone of the layer the snapshot's generation kept, where the home's file system clones
//! files, else an empty file, which it fills with a copy of that layer as it reads the
//! snapshot, so that the two are always one generation's. Either is made in the home's
//! `layers`, and stays there until the run it
//! serves ends: a run that stays keeps it in its directory once its microVM has
//! gone ([`LayerFile::keep_at`]), its guest having written its journal in place, and every
//! other goes with its handle, its microVM holding it open and mapped meanwhile, as a
//! run's swap does (D159).
//! What a daemon before left in `layers` is no run's by the next.
//!
//! A kept layer is read here, on the host ([`pack`]): what `commit` takes of a stopped
//! microVM, and what a microVM's next run, or a visit to its files, puts back.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use shards_ext4::journal;
use shards_ext4::read::{self, Kind, Volume, Walker};

/// Where layers are made: in the home, beside the files they are copied from, so that a
/// copy may share their blocks.
const LAYERS: &str = "layers";

/// A layer's file is whole multiples of this: a pmem device's (vmm `pmem::ALIGN`).
const STEP: u64 = 2 << 20;

/// A microVM's writable layer's file, made for it, until it holds it: unlinked as this
/// goes, which the daemon lets go of once the microVM is ready or has failed to start.
#[derive(Debug)]
pub(super) struct LayerFile {
    path: PathBuf,
    /// The snapshot generation whose layer it is a clone of, for a restore of it.
    of: Option<String>,
    /// A microVM's own, kept where it is ([`in_place`](Self::in_place)): never removed
    /// here.
    in_place: bool,
}

/// What of `from` holds data, copied into a new sparse file at `to` of its length: a
/// restore's layer, where Windows makes no clone of the generation's.
#[cfg(windows)]
fn copied(from: &Path, to: &Path) -> std::io::Result<()> {
    let source = std::fs::File::open(from)?;
    let len = source.metadata()?.len();
    let target = create(to)?;
    shards_vmm::platform::sparse(&target)?;
    shards_vmm::platform::copy_used(&source, &target, len)
}

impl LayerFile {
    /// An empty file, which a restoring microVM fills with its snapshot's layer.
    pub(super) fn empty(home: &Path) -> Result<LayerFile, String> {
        let path = fresh(home)?;
        create(&path)
            .and_then(|f| shards_vmm::platform::sparse(&f))
            .map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(LayerFile {
            path,
            of: None,
            in_place: false,
        })
    }

    /// The file a restore of the snapshot generation at `generation` is given for its
    /// layer: a copy-on-write clone of the generation's own where the home's file system
    /// makes one (APFS; btrfs and XFS on Linux), which shares its blocks until its run
    /// writes them, so that the restore copies nothing and on macOS writes nothing back
    /// (PM M278); else an [`empty`](Self::empty) one. A generation replaced before the
    /// microVM reads it is no matter: the microVM fills the file from the generation it
    /// resumes unless that is the one it is a clone of ([`cloned_of`](Self::cloned_of)).
    pub(super) fn for_restore(home: &Path, generation: &Path) -> Result<LayerFile, String> {
        let name = generation.file_name().and_then(|n| n.to_str());
        if let Some(name) = name {
            let path = fresh(home)?;
            let from = generation.join(shards_vmm::snapshot::LAYER);
            let cloned = std::fs::File::open(&from).and_then(|f| shards_build::host::clone_file(&f, &path));
            match cloned {
                Ok(true) => {
                    let layer = LayerFile {
                        path,
                        of: Some(name.to_string()),
                        in_place: false,
                    };
                    // Its user's alone, as every layer's file is.
                    #[cfg(unix)]
                    std::fs::set_permissions(
                        &layer.path,
                        std::os::unix::fs::PermissionsExt::from_mode(0o600),
                    )
                    .map_err(|e| format!("{}: {e}", layer.path.display()))?;
                    return Ok(layer);
                }
                // No clone on this host's file system (NTFS): a copy of what holds data in the
                // generation's, made here, where the file system says what that is (a
                // confined VM may ask it nothing, D141), so that the restore copies nothing,
                // its file sparse as the generation's is.
                #[cfg(windows)]
                Ok(false) => match copied(&from, &path) {
                    Ok(()) => {
                        return Ok(LayerFile {
                            path,
                            of: Some(name.to_string()),
                            in_place: false,
                        });
                    }
                    Err(e) => {
                        let _ = std::fs::remove_file(&path);
                        super::log(format!(
                            "{}: copying it for a restore: {e}; the restore copies it",
                            from.display()
                        ));
                    }
                },
                #[cfg(not(windows))]
                Ok(false) => {}
                // Replaced meanwhile: the microVM reads the generation that replaced it.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => super::log(format!(
                    "{}: cloning it for a restore: {e}; the restore copies it",
                    from.display()
                )),
            }
        }
        Self::empty(home)
    }

    /// The generation whose layer this file is a clone of, if it is one.
    pub(super) fn cloned_of(&self) -> Option<&str> {
        self.of.as_deref()
    }

    /// A file holding an empty ext4 file system that holds `holds` bytes, for a microVM
    /// that boots.
    pub(super) fn made(home: &Path, holds: u64) -> Result<LayerFile, String> {
        let bytes = size(holds)?;
        let path = fresh(home)?;
        let layer = LayerFile {
            path,
            of: None,
            in_place: false,
        };
        let at = |e: &dyn std::fmt::Display| format!("{}: {e}", layer.path.display());
        let mut file = create(&layer.path).map_err(|e| at(&e))?;
        // Sparse, as a Unix file system makes a file a length set leaves unwritten: the
        // blocks its file system writes alone are held, and a copy of it reads them alone
        // (`platform::data_ranges`).
        shards_vmm::platform::sparse(&file).map_err(|e| at(&e))?;
        file.set_len(bytes).map_err(|e| at(&e))?;
        let mut ids = [0u8; 32];
        shards_vmm::platform::fill_random(&mut ids).map_err(|e| at(&e))?;
        let (mut uuid, mut hash_seed) = ([0u8; 16], [0u8; 16]);
        uuid.copy_from_slice(ids.get(..16).unwrap_or_default());
        hash_seed.copy_from_slice(ids.get(16..).unwrap_or_default());
        // A random UUID's version and variant (RFC 9562 §5.4), as mke2fs gives one.
        if let Some(version) = uuid.get_mut(6) {
            *version = (*version & 0x0F) | 0x40;
        }
        if let Some(variant) = uuid.get_mut(8) {
            *variant = (*variant & 0x3F) | 0x80;
        }
        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let options = shards_ext4::Options {
            bytes,
            uuid,
            hash_seed,
            time,
        };
        shards_ext4::format(&mut file, &options).map_err(|e| at(&e))?;
        Ok(layer)
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    /// A stopped microVM's layer kept at `path` (D190) as its own, the file its run's
    /// microVM took under its layer device as it started: where it stays, never removed
    /// as its handle goes.
    pub(super) fn in_place(path: PathBuf) -> LayerFile {
        LayerFile {
            path,
            of: None,
            in_place: true,
        }
    }

    /// Keeps the file as `at`, a microVM's, in place of what was there: no longer the
    /// handle's to remove. One kept there already stays.
    pub(super) fn keep_at(mut self, at: &Path) -> std::io::Result<()> {
        if !(self.in_place && self.path == at) {
            std::fs::rename(&self.path, at)?;
        }
        self.path = PathBuf::new();
        Ok(())
    }
}

impl Drop for LayerFile {
    fn drop(&mut self) {
        // None, once kept, or a microVM's own.
        if !self.path.as_os_str().is_empty() && !self.in_place {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Where a run keeps its writable layer once its microVM has gone (D190), in its
/// directory, beside its log.
pub(super) const KEPT: &str = "layer.ext4";

/// Opens the writable layer kept at `at` for reading, its microVM gone. Its guest wrote
/// its journal in place as its run ended (init's `pmem::checkpoint_layer`), and it is read
/// as it stands; where its microVM went first (killed), or its guest's kernel only
/// committed it, its journal is replayed in place first (shards_ext4::journal, as its
/// kernel replays one as it mounts it): what its guest committed is in place, and what it
/// never committed stays out.
pub(super) fn open_kept(at: &Path) -> Result<std::fs::File, String> {
    let file = std::fs::File::open(at).map_err(|e| format!("{}: {e}", at.display()))?;
    if !journal::needs_replay(&file).map_err(|e| e.to_string())? {
        return Ok(file);
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(at)
        .map_err(|e| format!("{}: {e}", at.display()))?;
    let done = journal::replay(&file).map_err(|e| format!("replaying its journal: {e}"))?;
    if let Some(done) = done {
        super::log(format!(
            "{}: replayed {} transactions of its journal, {} blocks, {} revoked",
            at.display(),
            done.transactions,
            done.blocks,
            done.revoked
        ));
    }
    Ok(file)
}

/// How a stopped microVM's next run takes the layer kept in its file (D190).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Restart {
    /// Packed here and put back into the warm microVM's own layer ([`pack`]).
    Packed,
    /// Its file under the microVM's layer device as it is, in place of the microVM's own.
    Over,
}

/// Where the home keeps what its restarts cost ([`Restarts`]).
const RESTARTS: &str = "restarts";

/// A line's answer at a size ([`Line::fit`]).
#[derive(Clone, Copy, Debug)]
struct Fit {
    estimate: f64,
    residual: f64,
    free: f64,
    spread: f64,
}

/// One way's restarts on this host: the least-squares line of a restart's time against the
/// bytes its layer held, kept online as its count, means and co-moments (Welford's
/// updates), which a file of the home keeps between daemons.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Line {
    n: u64,
    /// The mean of what the layers held, bytes, and of what their restarts took, µs.
    x: f64,
    y: f64,
    /// Sums of the products of their deviations from those means.
    xx: f64,
    xy: f64,
    yy: f64,
}

impl Line {
    fn add(&mut self, x: f64, y: f64) {
        self.n = self.n.saturating_add(1);
        let n = self.n as f64;
        let (dx, dy) = (x - self.x, y - self.y);
        self.x += dx / n;
        self.y += dy / n;
        self.xx += dx * (x - self.x);
        self.xy += dx * (y - self.y);
        self.yy += dy * (y - self.y);
    }

    /// The records of `self` and `other` as one line: their counts, means and co-moments
    /// combined (Chan, Golub and LeVeque's pairwise update), as if each of `other`'s had been
    /// added.
    fn merge(&mut self, other: &Line) {
        if other.n == 0 {
            return;
        }
        if self.n == 0 {
            *self = *other;
            return;
        }
        let total = self.n.saturating_add(other.n);
        let (n, m, sum) = (self.n as f64, other.n as f64, total as f64);
        let (dx, dy) = (other.x - self.x, other.y - self.y);
        let weight = n * m / sum;
        self.xx += other.xx + dx * dx * weight;
        self.xy += other.xy + dx * dy * weight;
        self.yy += other.yy + dy * dy * weight;
        self.x += dx * m / sum;
        self.y += dy * m / sum;
        self.n = total;
    }

    /// What a restart of a layer holding `x` bytes takes by the line, µs, and the variance
    /// of that estimate (its residuals' variance over the line's degrees of freedom, as an
    /// estimate of a regression's mean at `x` has it). None while the records cannot say: a
    /// line of fewer than three, or one whose layers all held one size, asked of another.
    fn at(&self, x: f64) -> Option<(f64, f64)> {
        self.fit(x).map(|fit| (fit.estimate, fit.residual * fit.spread))
    }

    /// The line's estimate at `x`, its residuals' variance over its degrees of freedom, and
    /// the estimate's variance as a multiple of that (`1/n + (x − x̄)²/Sxx`).
    fn fit(&self, x: f64) -> Option<Fit> {
        let n = self.n as f64;
        let (slope, residual, free, spread) = if self.xx > 0.0 {
            let slope = self.xy / self.xx;
            (
                slope,
                self.yy - slope * self.xy,
                n - 2.0,
                (x - self.x).powi(2) / self.xx,
            )
        } else if x == self.x {
            (0.0, self.yy, n - 1.0, 0.0)
        } else {
            return None;
        };
        if free < 1.0 {
            return None;
        }
        Some(Fit {
            estimate: self.y + slope * (x - self.x),
            residual: residual.max(0.0) / free,
            free,
            spread: 1.0 / n + spread,
        })
    }

    /// Whether a new record, a restart of a layer holding `x` bytes that took `y` µs, is
    /// outside the line's spread ([`Way`]): a regression's prediction test (the distance of
    /// a new record from the line over `s·√(1 + 1/n + (x − x̄)²/Sxx)` is Student's t with the
    /// line's degrees of freedom; Brown, Durbin and Evans's recursive residual) at the level
    /// `1/(n + 1)`. None where the line cannot say ([`at`](Self::at)).
    fn outside(&self, x: f64, y: f64) -> Option<bool> {
        let fit = self.fit(x)?;
        let distance = y - fit.estimate;
        let variance = fit.residual * (1.0 + fit.spread);
        if !(variance > 0.0) {
            // A line with no spread at all: every record off it is outside.
            return Some(distance != 0.0);
        }
        let beyond = t_beyond(distance * distance / variance, fit.free);
        Some(beyond * (self.n as f64 + 1.0) < 1.0)
    }

    fn encode(&self) -> String {
        format!(
            "{} {} {} {} {} {}",
            self.n, self.x, self.y, self.xx, self.xy, self.yy
        )
    }

    fn decode(text: &str) -> Option<Line> {
        let mut f = text.split(' ');
        let n = f.next()?.parse().ok()?;
        let mut next = || {
            f.next()
                .and_then(|v| v.parse::<f64>().ok())
                .filter(|v| v.is_finite())
        };
        let line = Line {
            n,
            x: next()?,
            y: next()?,
            xx: next()?,
            xy: next()?,
            yy: next()?,
        };
        next().is_none().then_some(line)
    }
}

/// What this host's restarts cost each way (D190, PM M443, M496): each restart that could go
/// either way records the bytes its layer held and the time from the choice to its command
/// executing, and the next one takes the way its records say is quicker for what it holds.
/// Packing grows with what the layer holds, going over less so, so a line a way says where
/// they cross on this host, its own disk, file system and load; the decision is made only
/// where the two estimates differ by more than the spread of their difference, as the
/// records measure it; where they do not (the first restarts, or near the crossing, where
/// either costs about the same), the way recorded fewer times goes, so that both are
/// measured until they can be told apart.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct Restarts {
    packed: Way,
    over: Way,
}

/// The records a way learns from (D190's ageing): its line, of every record since its
/// learning last restarted, and those since that each fell outside the line's spread, held
/// out of it. A record is outside where the line gives one at least as far from it a chance
/// below `1/(n + 1)` ([`Line::outside`]): farther than, under no change, the farthest of its
/// `n` records and this one is expected to be once. Three in a row, the least records that
/// make a line with a residual, restart its learning from them: under no change a chance of
/// about `(n + 1)^-3` (the three are tested against one line), whose sum over a line's life
/// is finite; one within the line's spread puts those held out back into it with itself.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Way {
    line: Line,
    outside: Line,
}

/// The records outside a line that restart its learning: the least that make a line with a
/// residual (two for its intercept and slope, one more for the residual's degree of freedom).
const RESTART_FROM: u64 = 3;

impl Way {
    /// Learns from a restart of a layer holding `x` bytes that took `y` µs. True where the
    /// record restarted its learning.
    fn add(&mut self, x: f64, y: f64) -> bool {
        match self.line.outside(x, y) {
            None => {
                self.line.add(x, y);
                false
            }
            Some(false) => {
                self.line.merge(&self.outside);
                self.outside = Line::default();
                self.line.add(x, y);
                false
            }
            Some(true) => {
                self.outside.add(x, y);
                if self.outside.n < RESTART_FROM {
                    return false;
                }
                self.line = self.outside;
                self.outside = Line::default();
                true
            }
        }
    }
}

/// The chance that Student's t with `free` degrees of freedom is farther than `√t2` from
/// zero, either way: `I_x(free/2, 1/2)` at `x = free/(free + t2)`, the regularized
/// incomplete beta function (Abramowitz and Stegun 26.7.1 with 26.5.27; DLMF 8.17.2).
fn t_beyond(t2: f64, free: f64) -> f64 {
    incomplete_beta(free / 2.0, 0.5, free / (free + t2))
}

/// The regularized incomplete beta function `I_x(a, b)`: its continued fraction (DLMF
/// 8.17.22), evaluated by Lentz's method where it converges quickly, `x < (a + 1)/(a + b + 2)`,
/// and through `I_x(a, b) = 1 − I_(1−x)(b, a)` where not (Press et al., Numerical Recipes,
/// 3rd ed., §6.4).
fn incomplete_beta(a: f64, b: f64, x: f64) -> f64 {
    if !(x > 0.0) {
        return 0.0;
    }
    if x >= 1.0 {
        return 1.0;
    }
    let front =
        (libm::lgamma(a + b) - libm::lgamma(a) - libm::lgamma(b) + a * x.ln() + b * (-x).ln_1p()).exp();
    if x < (a + 1.0) / (a + b + 2.0) {
        front * fraction(a, b, x) / a
    } else {
        1.0 - front * fraction(b, a, 1.0 - x) / b
    }
}

/// DLMF 8.17.22's continued fraction for `I_x(a, b)`, by Lentz's method (Numerical Recipes
/// §5.2, §6.4): its terms taken until one changes it by less than an `f64` can tell
/// (`f64::EPSILON`), a denominator of zero kept off it by the least normal `f64`. It needs
/// `O(√max(a, b))` terms (Numerical Recipes §6.4); it is cut at `max(a, b) + 1`, so that a
/// line of absurd count read from a damaged file ends.
fn fraction(a: f64, b: f64, x: f64) -> f64 {
    let off_zero = |v: f64| {
        if v.abs() < f64::MIN_POSITIVE {
            f64::MIN_POSITIVE
        } else {
            v
        }
    };
    let (sum, up, down) = (a + b, a + 1.0, a - 1.0);
    let mut c = 1.0;
    let mut d = 1.0 / off_zero(1.0 - sum * x / up);
    let mut h = d;
    let last = a.max(b) + 1.0;
    let mut m = 1.0;
    while m <= last {
        let m2 = 2.0 * m;
        let even = m * (b - m) * x / ((down + m2) * (a + m2));
        d = 1.0 / off_zero(1.0 + even * d);
        c = off_zero(1.0 + even / c);
        h *= d * c;
        let odd = -(a + m) * (sum + m) * x / ((a + m2) * (up + m2));
        d = 1.0 / off_zero(1.0 + odd * d);
        c = off_zero(1.0 + odd / c);
        let step = d * c;
        h *= step;
        // Converged, or nothing a number any more.
        if !((step - 1.0).abs() > f64::EPSILON) {
            break;
        }
        m += 1.0;
    }
    h
}

impl Restarts {
    /// Those `home` keeps; none where it keeps none, or none it can read.
    pub(super) fn kept_in(home: &Path) -> Restarts {
        let mut restarts = Restarts::default();
        let Ok(text) = std::fs::read_to_string(home.join(RESTARTS)) else {
            return restarts;
        };
        // A daemon before the records aged kept their lines alone, and one of them reads
        // these and passes over what it does not know.
        for line in text.lines() {
            let (name, rest) = line.split_once(' ').unwrap_or((line, ""));
            let read = Line::decode(rest).unwrap_or_default();
            match name {
                "packed" => restarts.packed.line = read,
                "over" => restarts.over.line = read,
                "packed-outside" => restarts.packed.outside = read,
                "over-outside" => restarts.over.outside = read,
                _ => {}
            }
        }
        restarts
    }

    /// The way the restart of a layer holding `held` bytes goes.
    pub(super) fn choose(&self, held: u64) -> Restart {
        let x = held as f64;
        let (packed, over) = (&self.packed.line, &self.over.line);
        if let (Some((packed, vp)), Some((over, vo))) = (packed.at(x), over.at(x)) {
            let difference = packed - over;
            if difference * difference > vp + vo {
                return if difference > 0.0 {
                    Restart::Over
                } else {
                    Restart::Packed
                };
            }
        }
        if over.n < packed.n {
            Restart::Over
        } else {
            Restart::Packed
        }
    }

    /// A restart that went `way` with a layer holding `held` bytes took `took`; kept in
    /// `home` for the next daemon. Where its record and the two before it fell outside the
    /// way's line, its learning restarts from them ([`Way`]), and the log says so.
    pub(super) fn record(
        &mut self,
        home: &Path,
        way: Restart,
        held: u64,
        took: std::time::Duration,
    ) -> Result<(), String> {
        let (learning, name) = match way {
            Restart::Packed => (&mut self.packed, "packing"),
            Restart::Over => (&mut self.over, "going over"),
        };
        let had = learning.line.n;
        if learning.add(held as f64, took.as_secs_f64() * 1e6) {
            super::log(format!(
                "restarts: {name}'s last {RESTART_FROM} fell outside its line of {had}; \
                 its records restart from them"
            ));
        }
        self.keep(home)
    }

    /// Keeps the records in `home` for the next daemon.
    fn keep(&self, home: &Path) -> Result<(), String> {
        let at = home.join(RESTARTS);
        let new = home.join(format!("{RESTARTS}.new"));
        let text = format!(
            "packed {}\nover {}\npacked-outside {}\nover-outside {}\n",
            self.packed.line.encode(),
            self.over.line.encode(),
            self.packed.outside.encode(),
            self.over.outside.encode()
        );
        std::fs::write(&new, text)
            .and_then(|()| std::fs::rename(&new, &at))
            .map_err(|e| format!("{}: {e}", at.display()))
    }

    /// Where the two lines cross, bytes: none while either is not a line yet, or where they
    /// never meet.
    #[cfg(test)]
    fn crossing(&self) -> Option<f64> {
        let line = |l: &Line| (l.xx > 0.0).then(|| (l.xy / l.xx, l.y - l.xy / l.xx * l.x));
        let ((sp, ip), (so, io)) = (line(&self.packed.line)?, line(&self.over.line)?);
        (sp != so).then(|| (io - ip) / (sp - so))
    }
}

/// PM M443's and M496's restarts, `start -a` of a microVM that wrote that many bytes, ms at
/// p50, packed and put back, then gone over, interleaved: Linux's (Fedora, Btrfs, n = 20
/// a side), macOS's (an M3 Ultra, macOS 26.4, APFS, n = 40, "again") and Windows' (jupiter,
/// Windows 11 10.0.26200, NTFS, n = 20, quiet, and its second run with its tails): what
/// [`Restarts`] is held to.
#[cfg(test)]
const MEASURED: [&[(u64, f64, f64)]; 4] = [
    &[
        (0, 52.55, 63.14),
        (1 << 20, 70.37, 75.47),
        (2 << 20, 76.38, 90.44),
        (4 << 20, 95.90, 100.92),
        (8 << 20, 135.68, 122.06),
        (16 << 20, 259.86, 189.81),
        (32 << 20, 396.48, 286.21),
    ],
    &[
        (0, 41.40, 48.89),
        (4 << 20, 64.37, 65.82),
        (6 << 20, 77.38, 78.75),
        (8 << 20, 78.39, 78.10),
        (12 << 20, 97.77, 95.63),
        (16 << 20, 124.28, 113.45),
    ],
    &[
        (0, 88.66, 321.55),
        (1 << 20, 182.17, 319.86),
        (2 << 20, 243.61, 344.20),
        (4 << 20, 494.89, 652.89),
        (8 << 20, 824.35, 822.56),
        (16 << 20, 1612.55, 1344.67),
        (32 << 20, 3126.39, 2205.23),
    ],
    &[
        (0, 79.60, 318.76),
        (1 << 20, 162.27, 317.79),
        (2 << 20, 225.30, 325.42),
        (4 << 20, 373.50, 402.48),
        (8 << 20, 776.61, 774.51),
        (16 << 20, 1506.98, 1166.18),
        (32 << 20, 3065.68, 2371.70),
    ],
];

/// The bytes the layer kept in `file`, `len` bytes long, holds past what an empty one of its
/// size holds of metadata alone: its blocks in use, as its groups' counts say
/// ([`Volume::free_blocks`]), less a fresh file system's ([`shards_ext4::Made::free_blocks`],
/// made once a size in memory, nothing written). None where its blocks are not counted so.
pub(super) fn held(file: &std::fs::File, len: u64) -> Result<Option<u64>, String> {
    let volume = Volume::open(file).map_err(|e| e.to_string())?;
    let (Some(free), Some(fresh)) = (volume.free_blocks(), fresh_free(len)?) else {
        return Ok(None);
    };
    Ok(Some(
        fresh.saturating_sub(free).saturating_mul(volume.block_size()),
    ))
}

/// The blocks a file system of `len` bytes as [`LayerFile::made`] makes holds free, each
/// size's counted once, by a format whose writes go nowhere.
fn fresh_free(len: u64) -> Result<Option<u64>, String> {
    static FRESH: std::sync::Mutex<Vec<(u64, u64)>> = std::sync::Mutex::new(Vec::new());
    let mut fresh = FRESH.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(&(_, free)) = fresh.iter().find(|(l, _)| *l == len) {
        return Ok(Some(free));
    }
    struct Nowhere;
    impl shards_ext4::Sink for Nowhere {
        fn write_at(&mut self, _: u64, _: &[u8]) -> std::io::Result<()> {
            Ok(())
        }
    }
    let options = shards_ext4::Options {
        bytes: len,
        uuid: [0; 16],
        hash_seed: [0; 16],
        time: 0,
    };
    let made = shards_ext4::format(&mut Nowhere, &options).map_err(|e| e.to_string())?;
    fresh.push((len, made.free_blocks));
    Ok(Some(made.free_blocks))
}

/// Gives the host's disk back what the writable layer kept at `at` holds of blocks its
/// guest freed (D190), its microVM gone: pmem takes no discard (drivers/nvdimm/pmem.c), so
/// a layer's file only grew while its guest wrote, deletes and all. Of the blocks its block
/// bitmaps say are free ([`Volume::free_runs`]), those its file holds data in
/// ([`platform::data_ranges`](shards_vmm::platform::data_ranges)) are deallocated
/// ([`platform::deallocate`](shards_vmm::platform::deallocate)), and read as zeros after,
/// as a free block's bytes are never read. Returns the bytes given back; none of a layer
/// whose journal holds what its guest had yet to write in place (its microVM killed),
/// whose bitmaps may not say yet what the journal allocated: replayed as it is next read
/// ([`open_kept`]), given back as it is next kept.
pub(super) fn give_back(at: &Path) -> Result<Option<u64>, String> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(at)
        .map_err(|e| format!("{}: {e}", at.display()))?;
    if journal::needs_replay(&file).map_err(|e| e.to_string())? {
        return Ok(None);
    }
    let volume = Volume::open(&file).map_err(|e| e.to_string())?;
    let block = volume.block_size();
    let free = volume.free_runs().map_err(|e| e.to_string())?;
    let len = file
        .metadata()
        .map_err(|e| format!("{}: {e}", at.display()))?
        .len();
    let data = shards_vmm::platform::data_ranges(&file, len).map_err(|e| format!("{}: {e}", at.display()))?;
    let unit = shards_vmm::platform::deallocation_unit(at).map_err(|e| format!("{}: {e}", at.display()))?;
    let mut given = 0u64;
    for (from, n) in overlaps(&free, block, unit, &data) {
        shards_vmm::platform::deallocate(&file, from, n).map_err(|e| format!("{}: {e}", at.display()))?;
        given = given.saturating_add(n);
    }
    Ok(Some(given))
}

/// The byte ranges `(offset, len)` both in `free`, runs of blocks of `block` bytes, and in
/// `data`, byte ranges: each list in order, neither overlapping itself. Of each free run only
/// the whole `unit`s it holds, what the host's file system gives back whole
/// ([`platform::deallocation_unit`](shards_vmm::platform::deallocation_unit)): a part of one
/// would be zeroed and stay on its disk, and be found as data again.
fn overlaps(free: &[(u64, u64)], block: u64, unit: u64, data: &[(u64, u64)]) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    let unit = unit.max(1);
    let (mut f, mut d) = (free.iter().peekable(), data.iter().peekable());
    while let (Some(&&(fb, fn_)), Some(&&(dat, dlen))) = (f.peek(), d.peek()) {
        let fa = fb.saturating_mul(block).next_multiple_of(unit);
        let fz = fb.saturating_add(fn_).saturating_mul(block) / unit * unit;
        if fa >= fz {
            f.next();
            continue;
        }
        let dz = dat.saturating_add(dlen);
        let (from, to) = (fa.max(dat), fz.min(dz));
        if from < to {
            out.push((from, to - from));
        }
        if fz <= dz {
            f.next();
        } else {
            d.next();
        }
    }
    out
}

/// Writes the writable layer kept in `file` (D190) to `out` as an OCI layer: its overlay's
/// upper directory, whiteouts as `.wh.` entries, without what init made of every
/// microVM (shards_abi::run::MADE) and the directories its guest found held nothing
/// else (run::MADE_ONLY): the archive its guest's own pack of it makes (init's layer.rs).
pub(super) fn pack(file: std::fs::File, out: impl Write) -> Result<(), String> {
    let volume = Volume::open(file).map_err(|e| e.to_string())?;
    let mut walker = Walker::new(volume);
    let mut exclude: Vec<Vec<u8>> = shards_abi::run::MADE
        .iter()
        .map(|m| m.as_bytes().to_vec())
        .collect();
    exclude.extend(made_only(&mut walker)?);
    let opts = shards_archive::PackOptions {
        exclude_patterns: exclude,
        whiteout: shards_archive::WhiteoutFormat::Overlay,
        ..Default::default()
    };
    shards_archive::pack_from(&mut Files(walker), b"/upper", &opts, out)
        .map(drop)
        .map_err(|e| e.to_string())
}

/// [`pack`] of the layer kept in `file` into a file of its own, unlinked from `dir`, where
/// it was made, as soon as it is: what a joiner's run (D119) puts back, its join disk
/// taking the archive's whole 512-byte sectors, as a tar archive's are, where a pipe has
/// none to take.
pub(super) fn pack_to_file(file: std::fs::File, dir: &Path) -> Result<std::fs::File, String> {
    use std::io::{Seek as _, SeekFrom};
    let at = dir.join("layer.in");
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut out = options.open(&at).map_err(|e| format!("{}: {e}", at.display()))?;
    let _ = std::fs::remove_file(&at);
    pack(file, &mut out)?;
    out.seek(SeekFrom::Start(0))
        .map_err(|e| format!("{}: {e}", at.display()))?;
    Ok(out)
}

/// The directories its guest found held nothing but init's files, as the image has them
/// (run::MADE_ONLY): of those, only the directories of [`MADE`](shards_abi::run::MADE)'s
/// files, whatever the file says, as the guest wrote it.
fn made_only(walker: &mut Walker<std::fs::File>) -> Result<Vec<Vec<u8>>, String> {
    let at = format!("/{}", shards_abi::run::MADE_ONLY);
    let Some(listed) = walker.lookup(at.as_bytes()).map_err(|e| e.to_string())? else {
        return Ok(Vec::new());
    };
    if listed.kind != Kind::File || listed.size > 4096 {
        return Ok(Vec::new());
    }
    let mut text = vec![0u8; listed.size as usize];
    let n = walker
        .volume()
        .read_at(&listed, 0, &mut text)
        .map_err(|e| e.to_string())?;
    text.truncate(n);
    let dirs: Vec<&[u8]> = shards_abi::run::MADE
        .iter()
        .filter_map(|m| m.rsplit_once('/').map(|(d, _)| d.as_bytes()))
        .collect();
    Ok(text
        .split(|&c| c == b'\n')
        .filter(|d| dirs.contains(d))
        .map(<[u8]>::to_vec)
        .collect())
}

/// A kept layer's files, as a pack reads them ([`shards_archive::Files`]): through
/// shards-ext4, its guest's bytes checked as they are read.
struct Files(Walker<std::fs::File>);

fn not_there(path: &[u8]) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("{}: no such file", String::from_utf8_lossy(path)),
    )
}

impl Files {
    fn inode(&mut self, path: &[u8]) -> std::io::Result<read::Inode> {
        self.0
            .lookup(path)
            .map_err(|e| std::io::Error::other(e.0))?
            .ok_or_else(|| not_there(path))
    }
}

impl shards_archive::Files for Files {
    fn lstat(&mut self, path: &[u8]) -> std::io::Result<shards_archive::Stat> {
        use shards_archive::FileKind;
        let inode = self.inode(path)?;
        let (major, minor) = match inode.kind {
            Kind::Char | Kind::Block => inode.device(),
            _ => (0, 0),
        };
        Ok(shards_archive::Stat {
            kind: match inode.kind {
                Kind::File => FileKind::File,
                Kind::Dir => FileKind::Dir,
                Kind::Symlink => FileKind::Symlink,
                Kind::Char => FileKind::Char,
                Kind::Block => FileKind::Block,
                Kind::Fifo => FileKind::Fifo,
                Kind::Socket => FileKind::Socket,
            },
            mode: u32::from(inode.mode),
            uid: inode.uid,
            gid: inode.gid,
            size: inode.size,
            mtime: shards_archive::tar::Time::unix(inode.mtime.sec, i64::from(inode.mtime.nsec)),
            ino: u128::from(inode.ino),
            dev: 0,
            nlink: u64::from(inode.links),
            major: i64::from(major),
            minor: i64::from(minor),
        })
    }

    fn read_dir(&mut self, path: &[u8]) -> std::io::Result<Vec<(Vec<u8>, bool)>> {
        self.0.list(path).map_err(|e| std::io::Error::other(e.0))
    }

    fn readlink(&mut self, path: &[u8]) -> std::io::Result<Vec<u8>> {
        let inode = self.inode(path)?;
        self.0
            .volume()
            .read_link(&inode)
            .map_err(|e| std::io::Error::other(e.0))
    }

    fn xattr(&mut self, path: &[u8], name: &[u8]) -> std::io::Result<Option<Vec<u8>>> {
        let inode = self.inode(path)?;
        let all = self
            .0
            .volume()
            .xattrs(&inode)
            .map_err(|e| std::io::Error::other(e.0))?;
        Ok(all.into_iter().find(|(n, _)| n == name).map(|(_, v)| v))
    }

    fn open(&mut self, path: &[u8]) -> std::io::Result<Box<dyn std::io::Read + '_>> {
        let inode = self.inode(path)?;
        let reader = self
            .0
            .volume()
            .reader(&inode)
            .map_err(|e| std::io::Error::other(e.0))?;
        Ok(Box::new(reader))
    }
}

/// The bytes of a writable layer's file that holds `holds` bytes: its file system's,
/// whole pmem steps.
pub(super) fn size(holds: u64) -> Result<u64, String> {
    shards_ext4::size_for(holds, STEP).map_err(|e| e.to_string())
}

/// What the layers directory held of microVMs before this daemon: nothing any of them
/// still needs by name, as each held its own unlinked.
pub(super) fn clear(home: &Path) {
    let dir = home.join(LAYERS);
    for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let _ = std::fs::remove_file(entry.path());
    }
}

/// A name in the home's layers no other file has had.
fn fresh(home: &Path) -> Result<PathBuf, String> {
    static COUNT: AtomicU64 = AtomicU64::new(0);
    let dir = home.join(LAYERS);
    shards_vmm::platform::create_private_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let n = COUNT.fetch_add(1, Ordering::Relaxed);
    Ok(dir.join(format!("{}-{n}", std::process::id())))
}

/// A new file at `path`, read-write, its user's alone, sparse: a run's layer holds 10 GiB
/// unless it asks for another size (`run::LAYER_HOLDS`), of which it takes what its guest
/// writes (PM M349).
fn create(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let file = options.open(path)?;
    shards_vmm::platform::make_sparse(&file)?;
    Ok(file)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A layer made for a boot is an ext4 file system that holds what it was asked to, and
    /// goes as its handle does; an empty one for a restore is empty.
    #[test]
    fn a_layer_is_made_and_goes_with_its_handle() {
        let home = shards_testdir::TempDir::new("layers").unwrap();
        let made = LayerFile::made(&home, 64 << 20).unwrap();
        let path = made.path().to_path_buf();
        let len = std::fs::metadata(&path).unwrap().len();
        assert_eq!(len, size(64 << 20).unwrap());
        assert_eq!(len % STEP, 0);
        let mut magic = [0u8; 2];
        shards_vmm::platform::read_exact_at(&std::fs::File::open(&path).unwrap(), &mut magic, 1024 + 0x38)
            .unwrap();
        assert_eq!(u16::from_le_bytes(magic), 0xEF53);
        drop(made);
        assert!(!path.exists());

        let empty = LayerFile::empty(&home).unwrap();
        assert_eq!(std::fs::metadata(empty.path()).unwrap().len(), 0);
        // What a daemon before left is cleared.
        std::fs::write(home.join(LAYERS).join("1-0"), b"").unwrap();
        clear(&home);
        assert!(!home.join(LAYERS).join("1-0").exists() && !empty.path().exists());
    }

    /// A run's layer takes of its disk what its file system wrote, not what it holds: the
    /// 10 GiB a run's holds by default took all 10 GiB of an NTFS disk's free space before
    /// its file was sparse (PM M349), and eight runs at once filled a test host.
    #[test]
    fn a_layer_takes_of_its_disk_what_is_written() {
        let home = shards_testdir::TempDir::new("layers-sparse").unwrap();
        let made = LayerFile::made(&home, crate::run::LAYER_HOLDS).unwrap();
        let meta = std::fs::metadata(made.path()).unwrap();
        let on_disk = crate::os::on_disk(made.path(), &meta);
        assert!(
            on_disk < meta.len() / 64,
            "{on_disk} of {} bytes on disk",
            meta.len()
        );
    }

    /// What a layer's guest wrote and freed is given back to the host's disk once kept:
    /// the free blocks its file holds data in read as zeros after, take none of its disk,
    /// and the file system reads as it did; what is in use stays. On every host: Linux's and
    /// macOS's holes punched, a sparse NTFS file's clusters zeroed away (FSCTL_SET_ZERO_DATA).
    #[test]
    fn a_kept_layers_freed_blocks_go_back_to_its_disk() {
        let home = shards_testdir::TempDir::new("layers-give").unwrap();
        let made = LayerFile::made(&home, 256 << 20).unwrap();
        let at = home.join("kept.ext4");
        made.keep_at(&at).unwrap();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&at)
            .unwrap();
        let free = Volume::open(&file).unwrap().free_runs().unwrap();
        // 16 MiB of a free run, written as a guest's deleted file left it: at a MiB, so
        // that NTFS's sparse units (64 KiB) hold it whole.
        let &(first, n) = free.iter().find(|r| r.1 >= 8192).unwrap();
        let (offset, len) = (((first + 1024) * 4096).next_multiple_of(1 << 20), 16u64 << 20);
        assert!(offset + len <= (first + n) * 4096);
        shards_vmm::platform::write_all_at(&file, &vec![0x5Au8; len as usize], offset).unwrap();
        file.sync_all().unwrap();
        let on_disk = || crate::os::on_disk(&at, &file.metadata().unwrap());
        let before = on_disk();
        let used = std::fs::read(&at).unwrap();
        let given = give_back(&at).unwrap().unwrap();
        assert!(given >= len, "{given} bytes given back");
        file.sync_all().unwrap();
        let after = on_disk();
        assert!(after + len <= before, "{before} bytes on disk, then {after}");
        let now = std::fs::read(&at).unwrap();
        assert!(
            now[offset as usize..(offset + len) as usize]
                .iter()
                .all(|&b| b == 0)
        );
        // Every block in use as it was: the volume reads as it did.
        for (i, (a, b)) in used.chunks(4096).zip(now.chunks(4096)).enumerate() {
            let block = i as u64;
            if !free.iter().any(|&(f, n)| (f..f + n).contains(&block)) {
                assert_eq!(a, b, "block {block}, in use");
            }
        }
        let v = Volume::open(std::fs::File::open(&at).unwrap()).unwrap();
        assert!(v.lookup(b"/lost+found").unwrap().is_some());
        assert_eq!(give_back(&at).unwrap(), Some(0));
        // Free runs of blocks against data ranges of bytes: what both hold alone.
        assert_eq!(
            overlaps(
                &[(0, 2), (4, 4)],
                4096,
                4096,
                &[(4096, 8192), (20480, 4096), (65536, 10)]
            ),
            vec![(4096, 4096), (20480, 4096)]
        );
        // Of a free run, the whole units it holds alone: NTFS's 64 KiB (FSCTL_SET_ZERO_DATA
        // zeroes a part of one, which stays allocated), none of a run inside one.
        assert_eq!(
            overlaps(&[(2, 30), (40, 8)], 4096, 64 << 10, &[(0, 256 << 10)]),
            vec![(64 << 10, 64 << 10)]
        );
    }

    /// Each host's measured restarts, given as records: wherever the estimator decides, it
    /// takes the way that was measured quicker there, and it is sure of both ends of each
    /// table, packing an empty layer and going over the largest; where it is not, it tries
    /// the way it has fewer records of. Its first restarts, with no records, try each way in
    /// turn; and what it records is what the next daemon of the home reads (PM M443, M496).
    #[test]
    fn a_restart_goes_the_way_this_hosts_restarts_say_is_quicker() {
        let home = shards_testdir::TempDir::new("layers-restarts").unwrap();
        let ms = |v: f64| std::time::Duration::from_secs_f64(v / 1e3);
        for table in MEASURED {
            // The lines alone: a table's sizes in order are no host's, and a line of its first
            // three, 0 to 4 or 6 MiB, says little of its largest, where the costs curve (PM
            // M443, M496), so that recorded so its learning restarts ([`Way`]).
            let mut restarts = Restarts::default();
            for &(held, packed, over) in table {
                restarts.packed.line.add(held as f64, packed * 1e3);
                restarts.over.line.add(held as f64, over * 1e3);
            }
            restarts.keep(&home).unwrap();
            assert_eq!(Restarts::kept_in(&home), restarts);
            for &(held, packed, over) in table {
                let sure = restarts
                    .packed
                    .line
                    .at(held as f64)
                    .zip(restarts.over.line.at(held as f64))
                    .filter(|&((p, vp), (o, vo))| (p - o) * (p - o) > vp + vo);
                let quicker = if over < packed {
                    Restart::Over
                } else {
                    Restart::Packed
                };
                if sure.is_some() {
                    assert_eq!(restarts.choose(held), quicker, "{held} of {table:?}");
                }
            }
            let (first, last) = (table[0], table[table.len() - 1]);
            assert_eq!(restarts.choose(first.0), Restart::Packed, "{table:?}");
            assert_eq!(restarts.choose(last.0), Restart::Over, "{table:?}");
            let crossing = restarts.crossing().unwrap();
            assert!(
                crossing > first.0 as f64 && crossing < last.0 as f64,
                "{crossing}"
            );
        }
        // No records: each way in turn, until both can say.
        let mut restarts = Restarts::default();
        let mut ways = Vec::new();
        for _ in 0..4 {
            let way = restarts.choose(1 << 20);
            ways.push(way);
            restarts.record(&home, way, 1 << 20, ms(10.0)).unwrap();
        }
        assert_eq!(
            ways,
            [Restart::Packed, Restart::Over, Restart::Packed, Restart::Over]
        );
        // Layers of one size say nothing of another's.
        assert_eq!(restarts.packed.line.at(2.0 * f64::from(1u32 << 20)), None);
        // A file that cannot be read is no records.
        std::fs::write(home.join(RESTARTS), "packed 3 x\nover\n").unwrap();
        assert_eq!(Restarts::kept_in(&home), Restarts::default());
    }

    /// Restarts of a host whose line is Linux's packing (PM M443: 52 ms and 10.6 a MiB), at
    /// the measured tables' sizes taken at random, `shift(i)` µs added to the `i`th's line
    /// past its first `from`, with noise of `noise(rng)` µs. Returns the record each restart
    /// of learning came at, and the way.
    fn learn(
        seed: u64,
        n: usize,
        shift: impl Fn(usize, f64) -> f64,
        noise: impl Fn(&mut Rng) -> f64,
    ) -> (Vec<usize>, Way) {
        let mib = f64::from(1u32 << 20);
        let sizes = [0u32, 1, 2, 4, 8, 16, 32];
        let mut rng = Rng(seed);
        let mut way = Way::default();
        let mut restarts = Vec::new();
        for i in 0..n {
            let x = f64::from(sizes[(rng.next() % sizes.len() as u64) as usize]) * mib;
            let y = 52e3 + 10.6e3 * x / mib + shift(i, x) + noise(&mut rng);
            if way.add(x, y) {
                restarts.push(i);
            }
        }
        (restarts, way)
    }

    /// SplitMix64 (Steele, Lea and Flood 2014), and Box and Muller's normal deviates of it.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        fn uniform(&mut self) -> f64 {
            ((self.next() >> 11) as f64 + 0.5) / (1u64 << 53) as f64
        }

        fn normal(&mut self) -> f64 {
            let (u, v) = (self.uniform(), self.uniform());
            (-2.0 * u.ln()).sqrt() * (std::f64::consts::TAU * v).cos()
        }
    }

    /// Linux's p50 to p90 at 0 B (PM M443: 55.66 and 63.04 ms) as a normal deviation's,
    /// µs: 7.38 ms over 1.2816.
    const SIGMA: f64 = 7.38e3 / 1.2816;

    /// Noise alone restarts no learning once its line holds a few tens of records: normal
    /// noise, the line's own model, never in 32 lives of 2,000 records; a busy host's long
    /// tail (a log-normal's, its p99 several times its p50's distance, which the normal
    /// test underrates), rarely, three slow restarts in a row being a burst of load the line
    /// follows and comes back from. Each life keeps all but its first few tens of records.
    #[test]
    fn a_line_keeps_its_records_through_noise_alone() {
        let normal = |r: &mut Rng| SIGMA * r.normal();
        let tailed = |r: &mut Rng| SIGMA * (r.normal().exp() - 0.5f64.exp());
        let mut lives = [0, 0];
        for seed in 0..32 {
            for (kind, noise) in [&normal as &dyn Fn(&mut Rng) -> f64, &tailed]
                .into_iter()
                .enumerate()
            {
                let (restarts, way) = learn(seed, 2000, |_, _| 0.0, noise);
                if restarts.iter().any(|&i| i >= 30) {
                    lives[kind] += 1;
                } else {
                    assert!(way.line.n + way.outside.n >= 1970, "{kind} seed {seed}");
                }
            }
        }
        assert_eq!(lives[0], 0, "{lives:?} lives restarted late, normal and tailed");
        assert!(lives[1] <= 2, "{lives:?} lives restarted late, normal and tailed");
    }

    /// A host whose restarts all take longer by many of their spreads (its disk slower, or
    /// busier) restarts its learning within a few records of the change, in every life; one
    /// whose packing takes twice as long a byte, its cost at 0 B the same, within some tens
    /// in nearly every life: the records at 0 and 1 MiB stay within the line's spread and
    /// break the runs, and those a broken run puts back widen the line. Its line then says
    /// what the host's restarts take now, the records before the change gone from it.
    #[test]
    fn a_line_follows_its_host_when_its_level_or_slope_moves() {
        let mib = f64::from(1u32 << 20);
        let normal = |r: &mut Rng| SIGMA * r.normal();
        let level = |i: usize, _: f64| if i >= 500 { 60e3 } else { 0.0 };
        let slope = |i: usize, x: f64| if i >= 500 { 10.6e3 * x / mib } else { 0.0 };
        let mut found = [0, 0];
        for seed in 0..32 {
            for (kind, shift) in [&level as &dyn Fn(usize, f64) -> f64, &slope]
                .into_iter()
                .enumerate()
            {
                let (restarts, way) = learn(seed, 1000, shift, normal);
                let within = [10, 100][kind];
                if !restarts.iter().any(|&i| (500..500 + within).contains(&i)) {
                    continue;
                }
                found[kind] += 1;
                assert!(way.line.n <= 500, "{kind} seed {seed}: {}", way.line.n);
                for size in [0.0, 8.0, 32.0] {
                    let x = size * mib;
                    let now = 52e3 + 10.6e3 * size + shift(1000, x);
                    let (estimate, variance) = way.line.at(x).unwrap();
                    assert!(
                        (estimate - now).abs() < 4.0 * (variance + SIGMA * SIGMA / 10.0).sqrt(),
                        "{kind} seed {seed} at {size} MiB: {estimate} for {now}"
                    );
                }
            }
        }
        assert_eq!(
            found[0], 32,
            "{found:?} of 32 lives found the level's and the slope's change"
        );
        assert!(
            found[1] >= 30,
            "{found:?} of 32 lives found the level's and the slope's change"
        );
    }

    /// The t test's tail is Student's: two-sided 5% at the tabled quantiles (1, 2, 10 and 30
    /// degrees of freedom: 12.706, 4.303, 2.228, 2.042), 1% at 2.576 as the normal's, past a
    /// million; all of it at none, none of it at an infinite distance.
    #[test]
    fn the_t_tests_tail_is_students() {
        for (t, free, p) in [
            (12.706, 1.0, 0.05),
            (4.303, 2.0, 0.05),
            (2.228, 10.0, 0.05),
            (2.042, 30.0, 0.05),
            (2.5758, 1e6, 0.01),
        ] {
            let beyond = t_beyond(t * t, free);
            assert!((beyond - p).abs() < p * 1e-3, "t {t}, {free}: {beyond}");
        }
        assert_eq!(t_beyond(0.0, 5.0), 1.0);
        assert_eq!(t_beyond(f64::INFINITY, 5.0), 0.0);
        assert!(t_beyond(f64::NAN, 5.0).is_finite());
    }

    /// Two lines' records merged are the line of all of them; what a way holds out is kept
    /// in the home beside its line, and a home written before the records aged reads as its
    /// lines with nothing held out.
    #[test]
    fn records_held_out_are_kept_and_merge_back() {
        let (mut all, mut a, mut b) = (Line::default(), Line::default(), Line::default());
        for i in 0..40u32 {
            let (x, y) = (f64::from(i % 7) * 1e6, f64::from(i * i % 13) * 1e3);
            all.add(x, y);
            if i % 3 == 0 { a.add(x, y) } else { b.add(x, y) }
        }
        a.merge(&b);
        assert_eq!(a.n, all.n);
        for (p, q) in [
            (a.x, all.x),
            (a.y, all.y),
            (a.xx, all.xx),
            (a.xy, all.xy),
            (a.yy, all.yy),
        ] {
            assert!((p - q).abs() <= 1e-9 * q.abs().max(1.0), "{p} against {q}");
        }
        let home = shards_testdir::TempDir::new("layers-aged").unwrap();
        let mut restarts = Restarts::default();
        let ms = |v: f64| std::time::Duration::from_secs_f64(v / 1e3);
        for (held, took) in [
            (0, 50.0),
            (1 << 20, 60.0),
            (2 << 20, 70.0),
            (4 << 20, 90.0),
            (0, 500.0),
        ] {
            restarts.record(&home, Restart::Packed, held, ms(took)).unwrap();
        }
        assert_eq!(restarts.packed.outside.n, 1);
        assert_eq!(Restarts::kept_in(&home), restarts);
        std::fs::write(
            home.join(RESTARTS),
            format!("packed {}\nover {}\n", all.encode(), all.encode()),
        )
        .unwrap();
        let old = Restarts::kept_in(&home);
        assert_eq!((old.packed.line, old.over.line), (all, all));
        assert_eq!(
            (old.packed.outside, old.over.outside),
            (Line::default(), Line::default())
        );
    }

    /// A layer made empty holds nothing past what any of its size holds; one whose blocks a
    /// guest took holds them.
    #[test]
    fn what_a_layer_holds_is_its_blocks_past_an_empty_ones() {
        let home = shards_testdir::TempDir::new("layers-held").unwrap();
        let made = LayerFile::made(&home, 256 << 20).unwrap();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(made.path())
            .unwrap();
        let len = file.metadata().unwrap().len();
        assert_eq!(held(&file, len).unwrap(), Some(0));
        // 3 blocks of the first group taken, as its descriptor counts them: its free count
        // less 3, its checksum made again.
        let mut sb = [0u8; 1024];
        shards_vmm::platform::read_exact_at(&file, &mut sb, 1024).unwrap();
        let desc = usize::from(u16::from_le_bytes([sb[0xFE], sb[0xFF]]));
        let mut d = vec![0u8; desc];
        shards_vmm::platform::read_exact_at(&file, &mut d, 4096).unwrap();
        let free = u16::from_le_bytes([d[0xC], d[0xD]]) - 3;
        d[0xC..0xE].copy_from_slice(&free.to_le_bytes());
        let incompat = u32::from_le_bytes(sb[0x60..0x64].try_into().unwrap());
        let seed = if incompat & 0x2000 != 0 {
            u32::from_le_bytes(sb[0x270..0x274].try_into().unwrap())
        } else {
            shards_ext4::crc32c(!0, &sb[0x68..0x78])
        };
        let mut c = shards_ext4::crc32c(seed, &0u32.to_le_bytes());
        c = shards_ext4::crc32c(c, &d[..0x1E]);
        c = shards_ext4::crc32c(c, &[0, 0]);
        c = shards_ext4::crc32c(c, &d[0x20..]);
        d[0x1E..0x20].copy_from_slice(&((c & 0xFFFF) as u16).to_le_bytes());
        shards_vmm::platform::write_all_at(&file, &d, 4096).unwrap();
        assert_eq!(held(&file, len).unwrap(), Some(3 * 4096));
    }

    /// A restore's file is a clone of its generation's layer, said to be that generation's,
    /// where the home's file system clones files (always on macOS's APFS), else empty; one
    /// for a generation gone is empty; and a file kept stays as its handle goes.
    #[test]
    fn a_restores_layer_is_its_generations_clone_where_files_clone() {
        let home = shards_testdir::TempDir::new("layers-clone").unwrap();
        let generation = home.join("template").join("g-3");
        std::fs::create_dir_all(&generation).unwrap();
        let bytes: Vec<u8> = (0..4u32 << 20).map(|i| (i % 251) as u8).collect();
        std::fs::write(generation.join(shards_vmm::snapshot::LAYER), &bytes).unwrap();
        let given = LayerFile::for_restore(&home, &generation).unwrap();
        match given.cloned_of() {
            Some(of) => {
                assert_eq!(of, "g-3");
                assert_eq!(std::fs::read(given.path()).unwrap(), bytes);
            }
            None => assert_eq!(std::fs::metadata(given.path()).unwrap().len(), 0),
        }
        #[cfg(target_os = "macos")]
        assert!(given.cloned_of().is_some());
        let gone = LayerFile::for_restore(&home, &home.join("template").join("g-4")).unwrap();
        assert_eq!(gone.cloned_of(), None);
        assert_eq!(std::fs::metadata(gone.path()).unwrap().len(), 0);
        let (path, kept) = (given.path().to_path_buf(), home.join("kept.ext4"));
        given.keep_at(&kept).unwrap();
        assert!(kept.exists() && !path.exists());
    }
}
