#!/usr/bin/env python3
"""What shards' answer waits (crates/registry/src/patience.rs, D222) would have done with
the answers probe.py recorded (PM M486), beside the rules they replaced.

    replay.py PROBE.jsonl [SEED]

Five rules, each per endpoint, learning as the records come:
- fixed: 30 s, Go's transport's header wait, as containerd sets it;
- rfc6298: RFC 6298's estimator (SRTT + 4·RTTVAR, at least 1 s, 1 s before any answer),
  doubled for each cut in a row (§5.5): D222 as first built;
- cutoff: patience.rs's `Cutoff`. The wait is drawn uniformly from [t, 2t), doubled for each
  cut in a row (§5.5), 1 s before any answer, and an overdue answer is asked again at once
  (the pause `Trouble` takes, a uniform share of how long the trouble lasted, spent
  listening). t minimises the expected time to an answer with restarts (Luby, Sinclair
  and Zuckerman 1993, renewal-reward): E over W of ∫0^W S(u) du + S(W)·R, divided by t,
  S the path's survival of an asking as Kaplan and Meier (1958) estimate it from its
  answers and its cuts (an answer not seen past its cut), each kept in log buckets of 1%
  relative accuracy (DDSketch, Masson, Rim and Lee 2019), t a bucket's upper bound; R the
  mean time from a cut to the answer of an asking begun after it, as measured, and before
  any, Luby's i.i.d. restarts: E[∫0^W S] / E[F(W)]; t at least RFC 6298 §2.4's second;
- luby: the same, R always Luby's i.i.d. restarts (an asking again a fresh draw), to show
  what measuring R does;
- unfloored: the same as cutoff, t not held to a second, to show what the floor does.

Two models of what a cut answer's asking again would have met:
- trace: the endpoint's own next recorded asking that began at or after the moment it
  would have been asked again (a slow spell's slowness, and an outage's, carried as they
  were recorded; at most one probe interval, 2 s, later than the asking again would
  have been). A failure, and a cut of the rules that pause, are followed by `Trouble`'s
  pause, half of how long the trouble has lasted (the mean of its uniform share). An
  asking's cost runs from its first ask to its answer; its loss is that, less the first
  recorded answer from it on (negative where an asking again beat a slow answer).
- same: the model PM M486 first replayed, kept to compare: a healthy answer cut is asked
  again, the wait doubled, and answered as it was, each wait lost (an asking again
  never helps); a request never answered (the probe's timeout) loses the wait and the next
  is answered; fixed waits lose the probe's whole timeout to each of those, and an answer
  past 30 s, cut every 30 s and answered as it was, never comes: it loses the least
  budget a trouble is ridden out for, R2's 100 s (RFC 1122 §4.2.3.5), and fails. M486's
  first replay counted nothing against fixed waits for those; the second figure printed
  for fixed is that one. No pause.

Prints, by endpoint and in all, for each model and rule: the healthy answers cut (and
their share of the answers), the time lost, and the waits' p50 and max; and by endpoint
how often an asking past its p90 was followed by another (0.1 where slowness does not
persist). SEED (default 0) seeds the cutoff's draws.
"""
import bisect
import json
import math
import random
import sys
from collections import defaultdict

FIRST = 1.0
FIXED = 30.0
# The least a trouble is ridden out: TCP's R2 (RFC 1122 §4.2.3.5), patience.rs's.
R2 = 100.0
# DDSketch's relative accuracy α (Masson, Rim and Lee, VLDB 2019): buckets (γ^(k-1), γ^k],
# γ = (1 + α)/(1 − α), of µs, as patience.rs keeps them.
ALPHA = 0.01
GAMMA = (1 + ALPHA) / (1 - ALPHA)
# The pause before asking again, as a share of how long the trouble has lasted: the mean
# of `Trouble`'s uniform share.
PAUSE = 0.5


class Fixed:
    name = "fixed"
    pauses = True

    def wait(self):
        return FIXED

    def answered(self, took):
        pass

    def cut(self, wait):
        pass


class Rfc6298:
    name = "rfc6298"
    pauses = True

    def __init__(self, seed):
        self.srtt = None
        self.rttvar = None
        self.cuts = 0

    def wait(self):
        base = FIRST if self.srtt is None else max(FIRST, self.srtt + 4 * self.rttvar)
        return base * 2 ** min(self.cuts, 31)

    def answered(self, r):
        if self.srtt is None:
            self.srtt, self.rttvar = r, r / 2
        else:
            self.rttvar = 0.75 * self.rttvar + 0.25 * abs(self.srtt - r)
            self.srtt = 0.875 * self.srtt + 0.125 * r
        self.cuts = 0

    def cut(self, wait):
        self.cuts += 1


def bucket(seconds):
    us = max(seconds * 1e6, 1.0)
    return max(0, math.ceil(math.log(us) / math.log(GAMMA)))


def upper(k):
    return GAMMA ** k / 1e6


def base_wait(answers, cuts, again, least):
    """patience.rs's `Cutoff::base`: the t, `least` or a bucket's upper bound past it,
    minimising the expected time to an answer of waits drawn from [t, 2t); None before
    any answer."""
    if not answers:
        return None
    risk = sum(answers.values()) + sum(cuts.values())
    s, area, prev = 1.0, 0.0, 0.0
    # Segments of S, from 0 (S = 1) and from each bucket's upper bound (S after it): their
    # starts, S, A(x) = ∫0^x S and IA(x) = ∫0^x A at each start.
    starts, svals, avals, iavals = [0.0], [1.0], [0.0], [0.0]
    for k in sorted(set(answers) | set(cuts)):
        b = upper(k)
        h = b - prev
        iavals.append(iavals[-1] + area * h + s * h * h / 2)
        area += s * h
        prev = b
        d = answers.get(k, 0)
        if d and risk > 0:
            s *= 1 - d / risk
        risk -= d + cuts.get(k, 0)
        starts.append(b)
        svals.append(s)
        avals.append(area)

    def at(x):
        i = bisect.bisect_right(starts, x) - 1
        h = x - starts[i]
        return avals[i] + svals[i] * h, iavals[i] + avals[i] * h + svals[i] * h * h / 2

    best = None
    for b in [least] * (least > 0) + [b for b in starts[1:] if b > least]:
        a0, ia0 = at(b)
        a1, ia1 = at(2 * b)
        ia, isv = ia1 - ia0, a1 - a0
        if again is not None:
            e = (ia + isv * again) / b
        else:
            f = 1 - isv / b
            if f <= 0:
                continue
            e = ia / b / f
        if best is None or e < best[0]:
            best = (e, b)
    return None if best is None else best[1]


class Cutoff:
    name = "cutoff"
    pauses = False
    measured = True
    # RFC 6298 §2.4's least second.
    least = FIRST

    def __init__(self, seed):
        self.answers = defaultdict(int)
        self.cuts_seen = defaultdict(int)
        self.cuts = 0
        self.t = None
        self.again_sum = 0.0
        self.agains = 0
        self.rng = random.Random(seed)

    def again(self):
        return self.again_sum / self.agains if self.measured and self.agains else None

    def wait(self):
        base = FIRST if self.t is None else self.t
        return base * 2 ** min(self.cuts, 31) * (1 + self.rng.random())

    def learn(self):
        self.t = base_wait(self.answers, self.cuts_seen, self.again(), self.least)

    def answered(self, took):
        self.answers[bucket(took)] += 1
        self.cuts = 0
        self.learn()

    def cut(self, wait):
        self.cuts_seen[bucket(wait)] += 1
        self.cuts += 1
        self.learn()

    def recovered(self, seconds):
        self.again_sum += seconds
        self.agains += 1


class Luby(Cutoff):
    name = "luby"
    measured = False


class Unfloored(Cutoff):
    name = "unfloored"
    least = 0.0


RULES = (Fixed, Rfc6298, Luby, Unfloored, Cutoff)


def answered(r):
    return r["status"] is not None


def stuck(r):
    return r["status"] is None and r["error"] == "timeout"


def trace(rs, rule):
    """The trace model: (healthy cuts, loss s, waits)."""
    starts = [r["t"] - r["took"] for r in rs]
    # The first answer from each record on: each asking's own answer, had nothing been cut.
    first = [None] * len(rs)
    nxt = None
    for i in range(len(rs) - 1, -1, -1):
        if answered(rs[i]):
            nxt = rs[i]["took"]
        first[i] = nxt
    cut, loss, waits = 0, 0.0, []
    for i, r in enumerate(rs):
        if r.get("again") or first[i] is None:
            continue
        j, cost, first_cut = i, 0.0, None
        while j is not None:
            a = rs[j]
            w = rule.wait()
            waits.append(w)
            if not stuck(a) and a["took"] <= w:
                cost += a["took"]
                if answered(a):
                    rule.answered(a["took"])
                    if first_cut is not None and hasattr(rule, "recovered"):
                        rule.recovered(cost - first_cut)
                    break
                # A transport failure in time: ridden out as a trouble, begun at the
                # asking's start.
                cost += PAUSE * cost
            else:
                cost += w
                first_cut = cost if first_cut is None else first_cut
                rule.cut(w)
                if answered(a):
                    cut += 1
                if rule.pauses:
                    cost += PAUSE * cost
            at = starts[i] + cost
            j = next((k for k in range(j + 1, len(rs)) if starts[k] >= at), None)
        else:
            # The records end before an answer: not counted.
            continue
        loss += cost - first[i]
    return cut, loss, waits, loss


def same(rs, rule):
    """The model PM M486 first replayed: (healthy cuts, loss s, waits, the loss as M486
    first counted it)."""
    cut, loss, waits, first = 0, 0.0, [], 0.0
    for r in rs:
        w = rule.wait()
        waits.append(w)
        if stuck(r):
            lost = r["took"] if rule.name == "fixed" else w
            loss += lost
            first += lost
            rule.cut(w)
            continue
        if not answered(r):
            continue
        again = None
        if rule.name == "fixed":
            if r["took"] > w:
                cut += 1
                loss += R2
        else:
            while r["took"] > w:
                cut += 1
                loss += w
                first += w
                again = 0.0 if again is None else again + w
                rule.cut(w)
                w = rule.wait()
        rule.answered(r["took"])
        if again is not None and hasattr(rule, "recovered"):
            rule.recovered(again + r["took"])
    return cut, loss, waits, first


def spells(rs):
    """How often the asking after an answer past the endpoint's p90 was past it too
    (a request never answered counted past it): 0.1 where slowness does not persist."""
    took = sorted(r["took"] for r in rs if answered(r))
    p90 = took[min(len(took) - 1, int(0.9 * len(took)))]
    slow = [stuck(r) or r["took"] > p90 for r in rs if answered(r) or stuck(r)]
    after = [b for a, b in zip(slow, slow[1:]) if a]
    return sum(after) / max(len(after), 1)


def main():
    seed = int(sys.argv[2]) if len(sys.argv) > 2 else 0
    by = defaultdict(list)
    for line in open(sys.argv[1]):
        r = json.loads(line)
        by[(r["host"], r["path"].split("?")[0])].append(r)
    totals = defaultdict(float)
    answers = 0
    print("endpoint (answers), model: rule healthy cuts (share) loss s [wait p50/max s]; ...")
    for (host, path), rs in sorted(by.items()):
        rs.sort(key=lambda r: r["t"])
        n = sum(1 for r in rs if answered(r))
        answers += n
        for model in (trace, same):
            cells = []
            for kind in RULES:
                rule = kind() if kind is Fixed else kind(seed)
                c, l, w, f = model(rs, rule)
                w.sort()
                totals[(model.__name__, rule.name, "cut")] += c
                totals[(model.__name__, rule.name, "loss")] += l
                totals[(model.__name__, rule.name, "first")] += f
                cells.append(f"{rule.name} {c} ({c / max(n, 1):.3%}) {l:.1f} [{w[len(w) // 2]:.2f}/{w[-1]:.1f}]")
            print(f"{host}{path} ({n}), {model.__name__}: " + "; ".join(cells))
        print(f"{host}{path}: after an asking past p90, the next past it {spells(rs):.3f}")
    print(f"all: {answers} answers")
    for model in ("trace", "same"):
        cells = []
        for kind in RULES:
            c = totals[(model, kind.name, "cut")]
            l = totals[(model, kind.name, "loss")]
            f = totals[(model, kind.name, "first")]
            first = f" (M486's first count {f:.1f} s)" if f != l else ""
            cells.append(f"{kind.name} {int(c)} healthy cuts ({c / max(answers, 1):.4%}), lost {l:.1f} s{first}")
        print(f"all, {model}: " + "; ".join(cells))


if __name__ == "__main__":
    main()
