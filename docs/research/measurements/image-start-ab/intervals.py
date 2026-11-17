"""An image-start A/B's ranks with their intervals (PM M593): the logs ab.py keeps in OUT
(ARM-ROUND.log) read again, with no start made.

Per arm and kind, of the boot, the first output and the stop (`IMAGE-START-SAMPLE`): n; p50, p90
and p99, each with its distribution-free 95% interval (p3-warm-start/warm.py's `bounded`, Le
Boudec 2010, Theorem 2.1; none where n is too few, a p99's under 368); max (nearest rank, as
ab.py's); and the least and most of the rounds' own p50s, as the samples come N to a process.
From each process's timed phase pass, one start of each kind: the launch's wait for its writable
layer and its thread's making, as ab.py reads them, and the span from `its microVM shaped` to
`launching its VM`, which holds the wait and whatever the thread's work costs the rest of the
start.

Then B against A, per kind, of the boot (D253's refusals of its cut (a)) and of the span from
shaped to launching (where the wait was):
- pooled, per rank: how far past A's quantile B's can lie, from B's interval's low end less A's
  high end to its high end less A's low end. Each bound holds at 95% at least, one-sided: an
  interval misses on each side with probability at most 2.5% (warm.py's `interval`), so A's
  high end and B's low end together miss with at most 5%.
- by rounds: each round's B p50 less its A p50, the two processes one after the other (ABBA),
  and their median's interval over the rounds, which holds however much processes differ.
A low bound above 0 shows a rise; a high bound below 0, a fall; a low bound above minus A's p50
wait, a fall short of that wait. D253 refuses its cut on a rise of Restored's or Warm's boot, or
on a fall of Booted's or Saved's short of the wait; where the wait was, every kind's span from
shaped to launching should fall by it.

    python3 -I intervals.py OUT
"""
import importlib.util, os, sys

HERE = os.path.dirname(os.path.abspath(__file__))


def load(name, *path):
    spec = importlib.util.spec_from_file_location(name, os.path.join(HERE, *path))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


ab = load("image_start_ab", "ab.py")
warm = load("p3_warm_start", "..", "p3-warm-start", "warm.py")
RANKS = (50, 90, 99)
# D253's refusal for each kind's boot: a rise, or a fall short of the launch's former wait.
REFUSAL = {"Booted": "short", "Saved": "short", "Restored": "rise", "Warm": "rise"}
SHAPED, LAUNCHING = "its microVM shaped", "launching its VM"


def rounds(out, arm):
    """`arm`'s processes in OUT, in round order: each one's samples by kind (boot, first output,
    stop), and from its timed pass by kind the layer's wait, its thread's making and the span
    from shaped to launching, all in µs."""
    found, r = [], 0
    while os.path.exists(log := os.path.join(out, f"{arm}-{r}.log")):
        samples, waits, made, spans, decided, shaped = {}, {}, {}, {}, {}, {}
        with open(log, errors="replace") as f:
            for line in f:
                m = ab.SAMPLE.search(line)
                if m:
                    for v, us in zip(samples.setdefault(m[1], ([], [], [])), m.groups()[1:]):
                        v.append(int(us))
                    continue
                m = ab.PHASE.search(line)
                if not m or not m[3].startswith("run host"):
                    continue
                kind, at, text = m[1], int(m[2]), m[3]
                t = ab.THREAD.search(text)
                if t:
                    made.setdefault(kind, []).append(int(t[1]))
                elif text.endswith(SHAPED):
                    shaped[kind] = at
                elif text.endswith("its network decided"):
                    decided[kind] = at
                elif ab.TAKEN.search(text) and kind in decided:
                    waits.setdefault(kind, []).append(at - decided.pop(kind))
                elif text.endswith(LAUNCHING) and kind in shaped:
                    spans.setdefault(kind, []).append(at - shaped.pop(kind))
        found.append((samples, waits, made, spans))
        r += 1
    return found


def ms(us):
    return f"{us / 1000:.3f}"


def ranked(v):
    """Sorted `v`'s n, p50, p90 and p99 with their intervals, and max, in ms."""
    if not v:
        return "n 0"
    v, said = sorted(v), [f"n {len(v)}"]
    for p in RANKS:
        q, ci = warm.bounded(v, p)
        said.append(f"p{p} {ms(q)} " + (f"[{ms(ci[0])}, {ms(ci[1])}]" if ci else "[none]"))
    return " ".join(said + [f"max {ms(v[-1])}"])


def past(a, b, p):
    """How far past A's `p`th percentile B's can lie, (least, most) in µs; None where either has
    no interval."""
    if not a or not b:
        return None
    ca, cb = warm.bounded(sorted(a), p)[1], warm.bounded(sorted(b), p)[1]
    return ca and cb and (cb[0] - ca[1], cb[1] - ca[0])


def by_rounds(found, kind, name):
    """Each round's own p50 of `kind`'s `name`, in round order (None where it has none)."""
    p50s = []
    for samples, waits, made, spans in found:
        if name in ab.SPANS:
            v = samples.get(kind, ([], [], []))[ab.SPANS.index(name)]
        else:
            v = spans.get(kind, [])
        p50s.append(ab.nearest(sorted(v), 50) if v else None)
    return p50s


def paired(a, b, kind, name):
    """The rounds' B p50 less A p50, sorted, and their median's (low, high) interval, in µs."""
    d = sorted(y - x for x, y in zip(by_rounds(a, kind, name), by_rounds(b, kind, name)) if None not in (x, y))
    ci = warm.bounded(d, 50)[1] if d else None
    return d, ci and (ci[0], ci[1])


def verdict(lo, hi, refusal, wait):
    seen = "a fall shown" if hi < 0 else "a rise shown" if lo > 0 else "neither shown"
    if refusal == "rise":
        return f"{seen}; a rise: {'SHOWN' if lo > 0 else 'not shown'}"
    return f"{seen}; short of A's wait {ms(wait)}: {'SHOWN' if lo > -wait else 'not shown'}"


def main(out):
    arms = {arm: rounds(out, arm) for arm in ("A", "B")}
    pooled = {arm: {} for arm in arms}
    for arm, found in arms.items():
        print(f"{arm}: {len(found)} processes in {out}")
        for samples, waits, made, spans in found:
            for kind, v in samples.items():
                for name, x in zip(ab.SPANS, v):
                    pooled[arm].setdefault((kind, name), []).extend(x)
            timed = (("layer: launch waited", waits), ("layer: its thread", made), ("shaped to launching", spans))
            for name, by in timed:
                for kind, x in by.items():
                    pooled[arm].setdefault((kind, name), []).extend(x)
        kinds = [k for k, name in pooled[arm] if name == "boot"]
        for kind in kinds:
            for name in ab.SPANS:
                own = sorted(ab.nearest(sorted(s[kind][ab.SPANS.index(name)]), 50) for s, *_ in found if kind in s)
                print(f"  {kind:9} {name:20} {ranked(pooled[arm][(kind, name)])}; rounds' p50s {ms(own[0])} to {ms(own[-1])}")
            for name in ("layer: launch waited", "layer: its thread", "shaped to launching"):
                print(f"  {kind:9} {name:20} {ranked(pooled[arm].get((kind, name), []))}")
    print("B against A, ms: B's rank less A's, least to most (each bound at 95% at least, one-sided)")
    for kind in [k for k, name in pooled["A"] if name == "boot"]:
        w = ab.nearest(sorted(pooled["A"].get((kind, "layer: launch waited"), [0])), 50)
        for name, refusal in (("boot", REFUSAL.get(kind, "rise")), ("shaped to launching", "short")):
            a, b = pooled["A"][(kind, name)], pooled["B"].get((kind, name), [])
            for p in RANKS:
                d = past(a, b, p)
                said = verdict(*d, refusal, w) if d else "no interval"
                print(f"  {kind:9} {name:20} pooled p{p}: " + (f"{ms(d[0])} to {ms(d[1])}; " if d else "") + said)
            d, ci = paired(arms["A"], arms["B"], kind, name)
            if ci:
                med = ab.nearest(d, 50)
                print(
                    f"  {kind:9} {name:20} by rounds (n {len(d)}): median {ms(med)}, {ms(ci[0])} to {ms(ci[1])}; "
                    + verdict(*ci, refusal, w)
                )


if __name__ == "__main__":
    main(sys.argv[1])
