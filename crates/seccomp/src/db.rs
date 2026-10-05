//! libseccomp 2.5.4's filter database (src/db.c), ported as it is: each syscall's rules
//! as a tree of argument comparisons, merged rule by rule as libseccomp merges them
//! (`db_rule_add`, `_db_tree_prune`, `_db_tree_add`), its nodes counted and freed when
//! their last reference goes as libseccomp frees them, since what a merge prunes depends
//! on it. Nodes live in an arena and are named by index; a freed node is only marked so.
//! Which rule wins where two overlap is this merge's, and Docker's runc runs it, so a
//! profile means here what it means to Docker.

use crate::tables::Abi;

pub(crate) type Id = usize;

/// A comparison (`enum scmp_compare`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Ne,
    Lt,
    Le,
    Eq,
    Ge,
    Gt,
    MaskedEq,
}

/// One argument's comparison in a rule (`struct db_api_arg`, as db_col_rule_add makes
/// it): `arg & mask` against `datum`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArgCmp {
    pub arg: u32,
    pub op: Op,
    pub mask: u64,
    pub datum: u64,
}

/// A rule as an arch's filter takes it (`struct db_api_rule_list`): its comparisons by
/// argument number, at most one each.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub syscall: i32,
    pub action: u32,
    pub args: [Option<ArgCmp>; 6],
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Node {
    pub arg: u32,
    pub arg_h: bool,
    pub op: Option<Op>,
    pub op_orig: Option<Op>,
    pub mask: u32,
    pub datum: u32,
    pub datum_full: u64,
    pub act_t_flg: bool,
    pub act_t: u32,
    pub act_f_flg: bool,
    pub act_f: u32,
    pub nxt_t: Option<Id>,
    pub nxt_f: Option<Id>,
    pub lvl_prv: Option<Id>,
    pub lvl_nxt: Option<Id>,
    refcnt: u32,
    freed: bool,
}

/// A syscall's entry (`struct db_sys_list`).
#[derive(Debug, Clone)]
pub(crate) struct Sys {
    pub num: i32,
    pub valid: bool,
    pub chains: Option<Id>,
    pub action: u32,
}

/// Where a tree is hung: a syscall's chains, or a node's true or false branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Place {
    Chains(usize),
    T(Id),
    F(Id),
}

/// `struct db_iter_state`.
#[derive(Debug, Clone, Copy, Default)]
struct IterState {
    flags: u32,
    action: u32,
    sx: usize,
}

const IST_MATCH: u32 = 0x1;
const IST_MATCH_ONCE: u32 = 0x2;
const IST_X_FINISHED: u32 = 0x10;
const IST_N_FINISHED: u32 = 0x20;
const IST_X_PREFIX: u32 = 0x100;
const IST_N_PREFIX: u32 = 0x200;
const IST_M_MATCHSET: u32 = IST_MATCH | IST_MATCH_ONCE;
const IST_M_REDUNDANT: u32 = IST_MATCH | IST_X_FINISHED | IST_N_PREFIX;

/// The database's own inconsistency, which libseccomp would crash or corrupt on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// A node named that is not in the arena.
    Corrupt,
    /// `-EEXIST`: a rule that conflicts with one already there.
    Exists,
    /// `-EINVAL`.
    Invalid,
}

type Result<T> = std::result::Result<T, Error>;

/// One architecture's filter (`struct db_filter`): its syscalls, by number.
#[derive(Debug, Clone)]
pub struct Filter {
    pub abi: Abi,
    /// libseccomp 2.5.4's own slip kept: merging a 64-bit LT or LE comparison whose false
    /// action differs from one there, it sets the true action (db.c `_db_tree_add`).
    /// Docker's runc has it; shards sets the false action, as its comment means to.
    pub libseccomp_slip: bool,
    pub(crate) nodes: Vec<Node>,
    pub(crate) syscalls: Vec<Sys>,
}

impl Filter {
    pub fn new(abi: Abi) -> Filter {
        Filter {
            abi,
            libseccomp_slip: false,
            nodes: Vec::new(),
            syscalls: Vec::new(),
        }
    }

    pub(crate) fn node(&self, id: Id) -> Result<&Node> {
        self.nodes.get(id).ok_or(Error::Corrupt)
    }

    fn node_mut(&mut self, id: Id) -> Result<&mut Node> {
        self.nodes.get_mut(id).ok_or(Error::Corrupt)
    }

    fn alloc(&mut self, node: Node) -> Id {
        self.nodes.push(node);
        self.nodes.len() - 1
    }

    fn get_place(&self, p: Place) -> Result<Option<Id>> {
        Ok(match p {
            Place::Chains(s) => self.syscalls.get(s).ok_or(Error::Corrupt)?.chains,
            Place::T(n) => self.node(n)?.nxt_t,
            Place::F(n) => self.node(n)?.nxt_f,
        })
    }

    fn set_place(&mut self, p: Place, v: Option<Id>) -> Result<()> {
        match p {
            Place::Chains(s) => self.syscalls.get_mut(s).ok_or(Error::Corrupt)?.chains = v,
            Place::T(n) => self.node_mut(n)?.nxt_t = v,
            Place::F(n) => self.node_mut(n)?.nxt_f = v,
        }
        Ok(())
    }

    /// `_db_node_get`.
    fn get(&mut self, node: Option<Id>) -> Result<Option<Id>> {
        if let Some(n) = node {
            let n = self.node_mut(n)?;
            n.refcnt = n.refcnt.saturating_add(1);
        }
        Ok(node)
    }

    /// `_db_node_put` of the node `node` holds, `node` updated as the C caller's
    /// pointer is: to a neighbour on its level, or none, where the node goes.
    fn put(&mut self, node: &mut Option<Id>) -> Result<u32> {
        let Some(id) = *node else {
            return Ok(0);
        };
        let mut cnt = 0;
        let left = {
            let n = self.node_mut(id)?;
            n.refcnt = n.refcnt.saturating_sub(1);
            n.refcnt
        };
        if left == 0 {
            let (lvl_p, lvl_n, nxt_t, nxt_f) = {
                let n = self.node(id)?;
                (n.lvl_prv, n.lvl_nxt, n.nxt_t, n.nxt_f)
            };
            // Split the level; both neighbours are still referenced by it.
            if let Some(p) = lvl_p {
                self.node_mut(p)?.lvl_nxt = None;
            }
            if let Some(n) = lvl_n {
                self.node_mut(n)?.lvl_prv = None;
            }
            let (mut p, mut n) = (lvl_p, lvl_n);
            if p.is_some() {
                cnt += self.put(&mut p)?;
            }
            if n.is_some() {
                cnt += self.put(&mut n)?;
            }
            // Relink the level, if it is still there.
            if let Some(pp) = p {
                let got = self.get(n)?;
                self.node_mut(pp)?.lvl_nxt = got;
            }
            if let Some(nn) = n {
                let got = self.get(p)?;
                self.node_mut(nn)?.lvl_prv = got;
            }
            *node = p.or(n);
            let (mut t, mut f) = (nxt_t, nxt_f);
            cnt += self.tree_put(&mut t)?;
            cnt += self.tree_put(&mut f)?;
            self.node_mut(id)?.freed = true;
            cnt += 1;
        }
        Ok(cnt)
    }

    fn put_place(&mut self, p: Place) -> Result<u32> {
        let mut v = self.get_place(p)?;
        let cnt = self.put(&mut v)?;
        self.set_place(p, v)?;
        Ok(cnt)
    }

    /// `_db_level_clean`.
    fn level_clean(&mut self, node: Id) -> Result<u32> {
        let mut cnt = 0;
        let mut n = node;
        while let Some(p) = self.node(n)?.lvl_prv {
            n = p;
        }
        let start = n;
        let mut it = Some(start);
        while let Some(i) = it {
            let node = self.node(i)?;
            let links = u32::from(node.lvl_prv.is_some()) + u32::from(node.lvl_nxt.is_some());
            if node.refcnt > links {
                return Ok(cnt);
            }
            it = node.lvl_nxt;
        }
        let mut n = Some(start);
        while n.is_some() {
            cnt += self.put(&mut n)?;
        }
        Ok(cnt)
    }

    /// `_db_tree_put`.
    fn tree_put(&mut self, tree: &mut Option<Id>) -> Result<u32> {
        let mut cnt = self.put(tree)?;
        if let Some(t) = *tree {
            cnt += self.level_clean(t)?;
        }
        Ok(cnt)
    }

    fn arg_priority(n: &Node) -> u32 {
        (n.arg << 1) + u32::from(n.arg_h)
    }

    fn op_priority(op: Option<Op>) -> u32 {
        match op {
            Some(Op::MaskedEq | Op::Eq | Op::Ne) => 3,
            Some(Op::Le | Op::Lt) => 2,
            Some(Op::Ge | Op::Gt) => 1,
            None => 0,
        }
    }

    /// `_db_chain_lt`.
    fn chain_lt(&self, a: Id, b: Id) -> Result<bool> {
        let (a, b) = (self.node(a)?, self.node(b)?);
        let (aa, ba) = (Self::arg_priority(a), Self::arg_priority(b));
        if aa != ba {
            return Ok(aa < ba);
        }
        let (ao, bo) = (Self::op_priority(a.op_orig), Self::op_priority(b.op_orig));
        if ao != bo {
            return Ok(ao < bo);
        }
        Ok(match a.op_orig {
            // LT/LE invert, so that smaller values come first.
            Some(Op::Le | Op::Lt) => a.datum > b.datum,
            _ => a.datum < b.datum,
        })
    }

    /// `_db_chain_eq`.
    fn chain_eq(&self, a: Id, b: Id) -> Result<bool> {
        let (a, b) = (self.node(a)?, self.node(b)?);
        Ok(Self::arg_priority(a) == Self::arg_priority(b)
            && a.op == b.op
            && a.datum == b.datum
            && a.mask == b.mask)
    }

    fn leaf(&self, n: Id) -> Result<bool> {
        let n = self.node(n)?;
        Ok(n.nxt_t.is_none() && n.nxt_f.is_none())
    }

    fn zombie(&self, n: Id) -> Result<bool> {
        let node = self.node(n)?;
        Ok(self.leaf(n)? && !node.act_t_flg && !node.act_f_flg)
    }

    fn level_start(&self, mut n: Id) -> Result<Id> {
        while let Some(p) = self.node(n)?.lvl_prv {
            n = p;
        }
        Ok(n)
    }

    /// `_db_tree_remove`: `node`, and any node left a zombie, gone from the tree at `tree`.
    fn tree_remove(&mut self, tree: Place, node: Id) -> Result<u32> {
        let Some(head) = self.get_place(tree)? else {
            return Ok(0);
        };
        let mut cnt = 0;
        let mut c = self.level_start(head)?;
        loop {
            let remove = if c == node {
                true
            } else {
                cnt += self.tree_remove(Place::T(c), node)?;
                cnt += self.tree_remove(Place::F(c), node)?;
                self.zombie(c)?
            };
            if remove {
                // Reset the tree pointer if needed.
                if self.get_place(tree)? == Some(c) {
                    let n = self.node(c)?;
                    let to = n.lvl_prv.or(n.lvl_nxt);
                    self.set_place(tree, to)?;
                }
                let (p, n) = {
                    let x = self.node(c)?;
                    (x.lvl_prv, x.lvl_nxt)
                };
                if let Some(p) = p {
                    self.node_mut(p)?.lvl_nxt = n;
                }
                if let Some(n) = n {
                    self.node_mut(n)?.lvl_prv = p;
                }
                {
                    let x = self.node_mut(c)?;
                    x.lvl_prv = None;
                    x.lvl_nxt = None;
                }
                let mut gone = Some(c);
                cnt += self.put(&mut gone)?;
                return Ok(cnt);
            }
            match self.node(c)?.lvl_nxt {
                Some(n) if cnt == 0 => c = n,
                _ => return Ok(cnt),
            }
        }
    }

    /// `_db_tree_act_check`: every action in the tree is `action`.
    fn act_check(&self, tree: Option<Id>, action: u32) -> Result<bool> {
        let Some(t) = tree else {
            return Ok(true);
        };
        let mut c = Some(self.level_start(t)?);
        while let Some(i) = c {
            let n = self.node(i)?;
            if (n.act_t_flg && n.act_t != action) || (n.act_f_flg && n.act_f != action) {
                return Ok(false);
            }
            if !self.act_check(n.nxt_t, action)? || !self.act_check(n.nxt_f, action)? {
                return Ok(false);
            }
            c = n.lvl_nxt;
        }
        Ok(true)
    }

    /// `_db_tree_prune`: the existing tree `existing` pruned by the new one `new`; the
    /// number of nodes removed.
    fn tree_prune(&mut self, existing: Option<Id>, new: Option<Id>, state: &mut IterState) -> Result<u32> {
        let mut cnt = 0;
        let mut state_new = *state;
        let (Some(x0), Some(n_iter)) = (existing, new) else {
            return Ok(Self::prune_return(state, state_new, cnt));
        };
        if (state.flags & IST_M_MATCHSET) == IST_MATCH_ONCE {
            return Ok(Self::prune_return(state, state_new, cnt));
        }
        let mut x_iter = Some(self.level_start(x0)?);
        while let Some(x) = x_iter {
            let x_iter_next = self.node(x)?.lvl_nxt;
            let mut xi = Some(x);
            if self.chain_eq(x, n_iter)? {
                state_new.flags |= IST_M_MATCHSET;
                if self.leaf(n_iter)? {
                    state_new.flags |= IST_N_FINISHED;
                }
                if self.leaf(x)? {
                    state_new.flags |= IST_X_FINISHED;
                }
                let (xn, nn) = (self.node(x)?.clone(), self.node(n_iter)?.clone());
                // Don't remove nodes if there are more actions or levels.
                if (xn.act_t_flg || xn.nxt_t.is_some()) && !(nn.act_t_flg || nn.nxt_t.is_some()) {
                    return Ok(Self::prune_return(state, state_new, cnt));
                }
                if (xn.act_f_flg || xn.nxt_f.is_some()) && !(nn.act_f_flg || nn.nxt_f.is_some()) {
                    return Ok(Self::prune_return(state, state_new, cnt));
                }
                if (state_new.flags & IST_N_FINISHED != 0)
                    && (state_new.flags & IST_X_FINISHED != 0)
                    && (nn.act_t_flg != xn.act_t_flg
                        || nn.act_t != xn.act_t
                        || nn.act_f_flg != xn.act_f_flg
                        || nn.act_f != xn.act_f)
                {
                    return Ok(Self::prune_return(state, state_new, cnt));
                }
                for branch_t in [true, false] {
                    let Some(xid) = xi else { break };
                    let n_next = if branch_t {
                        self.node(n_iter)?.nxt_t
                    } else {
                        self.node(n_iter)?.nxt_f
                    };
                    if n_next.is_none() {
                        continue;
                    }
                    self.get(Some(xid))?;
                    let mut state_nxt = *state;
                    state_nxt.flags |= IST_M_MATCHSET;
                    let x_next = if branch_t {
                        self.node(xid)?.nxt_t
                    } else {
                        self.node(xid)?.nxt_f
                    };
                    cnt += self.tree_prune(x_next, n_next, &mut state_nxt)?;
                    cnt += self.put(&mut xi)?;
                    if state_nxt.flags & IST_MATCH != 0 {
                        state_new.flags |= state_nxt.flags;
                    }
                }
                let Some(xid) = xi else {
                    x_iter = x_iter_next;
                    continue;
                };
                // Remove the node?
                let same_actions = self.act_check(Some(xid), state_new.action)?;
                if same_actions
                    && (state_new.flags & IST_MATCH != 0)
                    && (state_new.flags & IST_N_FINISHED != 0)
                    && (state_new.flags & IST_X_PREFIX != 0)
                {
                    // Yes: the new tree is "shorter".
                    cnt += self.tree_remove(Place::Chains(state.sx), xid)?;
                    if self
                        .syscalls
                        .get(state.sx)
                        .ok_or(Error::Corrupt)?
                        .chains
                        .is_none()
                    {
                        return Ok(Self::prune_return(state, state_new, cnt));
                    }
                } else if same_actions
                    && (state_new.flags & IST_MATCH != 0)
                    && (state_new.flags & IST_X_FINISHED != 0)
                    && (state_new.flags & IST_N_PREFIX != 0)
                {
                    // No: the new tree is "longer".
                    return Ok(Self::prune_return(state, state_new, cnt));
                }
            } else if self.chain_lt(x, n_iter)? {
                if state.flags & IST_N_PREFIX != 0 {
                    return Ok(Self::prune_return(state, state_new, cnt));
                }
                for branch_t in [true, false] {
                    let Some(xid) = xi else { break };
                    let x_next = if branch_t {
                        self.node(xid)?.nxt_t
                    } else {
                        self.node(xid)?.nxt_f
                    };
                    if x_next.is_none() {
                        continue;
                    }
                    self.get(Some(xid))?;
                    let mut state_nxt = *state;
                    state_nxt.flags &= !IST_MATCH;
                    state_nxt.flags |= IST_X_PREFIX;
                    cnt += self.tree_prune(x_next, Some(n_iter), &mut state_nxt)?;
                    cnt += self.put(&mut xi)?;
                    if state_nxt.flags & IST_MATCH != 0 {
                        state_new.flags |= state_nxt.flags;
                        return Ok(Self::prune_return(state, state_new, cnt));
                    }
                }
            } else {
                if state.flags & IST_X_PREFIX != 0 {
                    return Ok(Self::prune_return(state, state_new, cnt));
                }
                for branch_t in [true, false] {
                    let Some(xid) = xi else { break };
                    let n_next = if branch_t {
                        self.node(n_iter)?.nxt_t
                    } else {
                        self.node(n_iter)?.nxt_f
                    };
                    if n_next.is_none() {
                        continue;
                    }
                    self.get(Some(xid))?;
                    let mut state_nxt = *state;
                    state_nxt.flags &= !IST_MATCH;
                    state_nxt.flags |= IST_N_PREFIX;
                    cnt += self.tree_prune(Some(xid), n_next, &mut state_nxt)?;
                    cnt += self.put(&mut xi)?;
                    if state_nxt.flags & IST_MATCH != 0 {
                        state_new.flags |= state_nxt.flags;
                        return Ok(Self::prune_return(state, state_new, cnt));
                    }
                }
            }
            x_iter = x_iter_next;
        }
        // Falling through, nothing matched.
        state_new.flags &= !IST_MATCH;
        Ok(Self::prune_return(state, state_new, cnt))
    }

    /// `prune_return:`.
    fn prune_return(state: &mut IterState, state_new: IterState, cnt: u32) -> u32 {
        if state_new.flags & IST_MATCH != 0 {
            state.flags |= state_new.flags;
        } else {
            state.flags &= !IST_MATCH;
        }
        cnt
    }

    /// `_db_tree_add`: the new tree `new` added into the one at `existing`.
    fn tree_add(&mut self, existing: Place, new: Id) -> Result<()> {
        let mut x_iter = self.get_place(existing)?;
        let n_iter = new;
        while let Some(x) = x_iter {
            if self.chain_eq(x, n_iter)? {
                let nn = self.node(n_iter)?.clone();
                if nn.act_t_flg {
                    let xn = self.node(x)?.clone();
                    if !xn.act_t_flg {
                        // The new node has a true action: do the actions match?
                        if !self.act_check(xn.nxt_t, nn.act_t)? {
                            return Err(Error::Exists);
                        }
                        self.put_place(Place::T(x))?;
                        let xm = self.node_mut(x)?;
                        xm.nxt_t = None;
                        xm.act_t = nn.act_t;
                        xm.act_t_flg = true;
                    } else if nn.act_t != xn.act_t {
                        // A 64-bit comparison takes its action by the full value, for GT/GE.
                        if nn.arg_h && nn.datum_full > xn.datum_full {
                            self.node_mut(x)?.act_t = nn.act_t;
                        }
                        if self.leaf(x)? || self.leaf(n_iter)? {
                            return Err(Error::Exists);
                        }
                    }
                }
                if nn.act_f_flg {
                    let xn = self.node(x)?.clone();
                    if !xn.act_f_flg {
                        if !self.act_check(xn.nxt_f, nn.act_f)? {
                            return Err(Error::Exists);
                        }
                        self.put_place(Place::F(x))?;
                        let xm = self.node_mut(x)?;
                        xm.nxt_f = None;
                        xm.act_f = nn.act_f;
                        xm.act_f_flg = true;
                    } else if nn.act_f != xn.act_f {
                        // The action taken by the full 64-bit value, for LT/LE.
                        if nn.arg_h && nn.datum_full < xn.datum_full {
                            if self.libseccomp_slip {
                                self.node_mut(x)?.act_t = nn.act_t;
                            } else {
                                self.node_mut(x)?.act_f = nn.act_f;
                            }
                        }
                        if self.leaf(x)? || self.leaf(n_iter)? {
                            return Err(Error::Exists);
                        }
                    }
                }
                if let Some(nt) = nn.nxt_t {
                    let xn = self.node(x)?.clone();
                    if xn.nxt_t.is_some() {
                        self.tree_add(Place::T(x), nt)?;
                    } else if !xn.act_t_flg {
                        let got = self.get(Some(nt))?;
                        self.node_mut(x)?.nxt_t = got;
                    } else {
                        // Done: the existing tree is "shorter".
                        return Ok(());
                    }
                }
                if let Some(nf) = nn.nxt_f {
                    let xn = self.node(x)?.clone();
                    if xn.nxt_f.is_some() {
                        self.tree_add(Place::F(x), nf)?;
                    } else if !xn.act_f_flg {
                        let got = self.get(Some(nf))?;
                        self.node_mut(x)?.nxt_f = got;
                    } else {
                        return Ok(());
                    }
                }
                return Ok(());
            } else if !self.chain_lt(x, n_iter)? {
                // Move along the level.
                match self.node(x)?.lvl_nxt {
                    None => {
                        // Add to the end of this level.
                        let got = self.get(Some(x))?;
                        self.node_mut(n_iter)?.lvl_prv = got;
                        let got = self.get(Some(n_iter))?;
                        self.node_mut(x)?.lvl_nxt = got;
                        return Ok(());
                    }
                    Some(next) => x_iter = Some(next),
                }
            } else {
                // Add before the existing node on this level.
                let x_prv = self.node(x)?.lvl_prv;
                if let Some(p) = x_prv {
                    let got = self.get(Some(n_iter))?;
                    self.node_mut(p)?.lvl_nxt = got;
                    self.node_mut(n_iter)?.lvl_prv = Some(p);
                    let got = self.get(Some(n_iter))?;
                    self.node_mut(x)?.lvl_prv = got;
                    self.node_mut(n_iter)?.lvl_nxt = Some(x);
                } else {
                    let got = self.get(Some(n_iter))?;
                    self.node_mut(x)?.lvl_prv = got;
                    let got = self.get(Some(x))?;
                    self.node_mut(n_iter)?.lvl_nxt = got;
                }
                if self.get_place(existing)? == Some(x) {
                    let got = self.get(Some(n_iter))?;
                    self.set_place(existing, got)?;
                    let mut gone = Some(x);
                    self.put(&mut gone)?;
                }
                return Ok(());
            }
        }
        Ok(())
    }

    fn new_node(&mut self, n: Node) -> Id {
        self.alloc(n)
    }

    /// `_db_rule_gen_64`: the rule as a chain of 32-bit comparisons of each argument's
    /// halves; its head and the syscall's own action where it has no comparisons.
    fn rule_gen_64(&mut self, rule: &Rule) -> Result<(Option<Id>, u32)> {
        let mut head = None;
        let mut prev: [Option<Id>; 3] = [None; 3];
        let mut cur: [Option<Id>; 3] = [None; 3];
        let mut op_prev = None;
        let (lo, hi) = (|m: u64| m as u32, |m: u64| (m >> 32) as u32);
        for chain in rule.args.iter().flatten() {
            // No-ops are not generated.
            let need_hi = !(chain.op == Op::MaskedEq && hi(chain.mask) == 0);
            let need_lo = !(chain.op == Op::MaskedEq && lo(chain.mask) == 0);
            if !need_hi && !need_lo {
                continue;
            }
            let base = Node {
                arg: chain.arg,
                datum_full: chain.datum,
                op_orig: Some(chain.op),
                ..Node::default()
            };
            let masked = |mask: u32, datum: u32| (mask, datum & mask);
            match chain.op {
                Op::Gt | Op::Ge | Op::Le | Op::Lt => {
                    let (hm, hd) = masked(hi(chain.mask), hi(chain.datum));
                    let (lm, ld) = masked(lo(chain.mask), lo(chain.datum));
                    let c0 = self.new_node(Node {
                        arg_h: true,
                        mask: hm,
                        datum: hd,
                        op: Some(Op::Gt),
                        ..base.clone()
                    });
                    let c1 = self.new_node(Node {
                        arg_h: true,
                        mask: hm,
                        datum: hd,
                        op: Some(Op::Eq),
                        ..base.clone()
                    });
                    let op2 = match chain.op {
                        Op::Gt | Op::Le => Op::Gt,
                        _ => Op::Ge,
                    };
                    let c2 = self.new_node(Node {
                        arg_h: false,
                        mask: lm,
                        datum: ld,
                        op: Some(op2),
                        ..base.clone()
                    });
                    let got = self.get(Some(c1))?;
                    self.node_mut(c0)?.nxt_f = got;
                    let got = self.get(Some(c2))?;
                    self.node_mut(c1)?.nxt_t = got;
                    cur = [Some(c0), Some(c1), Some(c2)];
                }
                Op::Eq | Op::MaskedEq | Op::Ne => {
                    let (hm, hd) = masked(hi(chain.mask), hi(chain.datum));
                    let (lm, ld) = masked(lo(chain.mask), lo(chain.datum));
                    let op = if chain.op == Op::MaskedEq {
                        Op::MaskedEq
                    } else {
                        Op::Eq
                    };
                    let c0 = self.new_node(Node {
                        arg_h: true,
                        mask: hm,
                        datum: hd,
                        op: Some(op),
                        ..base.clone()
                    });
                    let c1 = self.new_node(Node {
                        arg_h: false,
                        mask: lm,
                        datum: ld,
                        op: Some(op),
                        ..base.clone()
                    });
                    let got = self.get(Some(c1))?;
                    self.node_mut(c0)?.nxt_t = got;
                    cur = [Some(c0), Some(c1), None];
                }
            }
            // Link this level to the previous one.
            if let (Some(p0), Some(c0)) = (prev[0], cur[0]) {
                let link = |f: &mut Filter, p: Option<Id>, t: bool| -> Result<()> {
                    let p = p.ok_or(Error::Corrupt)?;
                    let got = f.get(Some(c0))?;
                    if t {
                        f.node_mut(p)?.nxt_t = got;
                    } else {
                        f.node_mut(p)?.nxt_f = got;
                    }
                    Ok(())
                };
                match op_prev {
                    Some(Op::Gt | Op::Ge) => {
                        link(self, Some(p0), true)?;
                        link(self, prev[2], true)?;
                    }
                    Some(Op::Eq | Op::MaskedEq) => link(self, prev[1], true)?,
                    Some(Op::Le | Op::Lt) => {
                        link(self, prev[1], false)?;
                        link(self, prev[2], false)?;
                    }
                    Some(Op::Ne) => {
                        link(self, Some(p0), false)?;
                        link(self, prev[1], false)?;
                    }
                    None => return Err(Error::Corrupt),
                }
            } else {
                head = self.get(cur[0])?;
            }
            prev = cur;
            op_prev = Some(chain.op);
        }
        if let Some(c0) = cur[0] {
            let set = |f: &mut Filter, n: Option<Id>, t: bool| -> Result<()> {
                let n = f.node_mut(n.ok_or(Error::Corrupt)?)?;
                if t {
                    n.act_t_flg = true;
                    n.act_t = rule.action;
                } else {
                    n.act_f_flg = true;
                    n.act_f = rule.action;
                }
                Ok(())
            };
            match op_prev {
                Some(Op::Gt | Op::Ge) => {
                    set(self, Some(c0), true)?;
                    set(self, cur[2], true)?;
                }
                Some(Op::Le | Op::Lt) => {
                    set(self, cur[1], false)?;
                    set(self, cur[2], false)?;
                }
                Some(Op::Eq | Op::MaskedEq) => set(self, cur[1], true)?,
                Some(Op::Ne) => {
                    set(self, Some(c0), false)?;
                    set(self, cur[1], false)?;
                }
                None => return Err(Error::Corrupt),
            }
            Ok((head, 0))
        } else {
            Ok((None, rule.action))
        }
    }

    /// `_db_rule_gen_32`.
    fn rule_gen_32(&mut self, rule: &Rule) -> Result<(Option<Id>, u32)> {
        let mut head = None;
        let mut prev: Option<Id> = None;
        let mut tf_flag = true;
        let mut last = None;
        for chain in rule.args.iter().flatten() {
            if chain.op == Op::MaskedEq && (chain.mask as u32) == 0 {
                continue;
            }
            let (op, tf) = match chain.op {
                Op::Ne => (Op::Eq, false),
                Op::Lt => (Op::Ge, false),
                Op::Le => (Op::Gt, false),
                other => (other, true),
            };
            // The upper 32 bits are implicitly stripped.
            let mask = chain.mask as u32;
            let c = self.new_node(Node {
                arg: chain.arg,
                arg_h: false,
                op: Some(op),
                op_orig: Some(chain.op),
                mask,
                datum: (chain.datum as u32) & mask,
                datum_full: chain.datum,
                ..Node::default()
            });
            let got = self.get(Some(c))?;
            match prev {
                Some(p) if tf_flag => self.node_mut(p)?.nxt_t = got,
                Some(p) => self.node_mut(p)?.nxt_f = got,
                None => head = got,
            }
            tf_flag = tf;
            prev = Some(c);
            last = Some(c);
        }
        match last {
            Some(c) => {
                let n = self.node_mut(c)?;
                if tf_flag {
                    n.act_t_flg = true;
                    n.act_t = rule.action;
                } else {
                    n.act_f_flg = true;
                    n.act_f = rule.action;
                }
                Ok((head, 0))
            }
            None => Ok((None, rule.action)),
        }
    }

    /// `db_rule_add`.
    pub fn rule_add(&mut self, rule: &Rule) -> Result<()> {
        let (new_chains, new_action) = if self.abi.is_32() {
            self.rule_gen_32(rule)?
        } else {
            self.rule_gen_64(rule)?
        };
        let at = self.syscalls.iter().position(|s| s.num >= rule.syscall);
        let mut rm_flag = false;
        let s = match at {
            Some(i) if self.syscalls.get(i).is_some_and(|s| s.num == rule.syscall) => i,
            _ => {
                // A new syscall, added before the first of a larger number.
                let entry = Sys {
                    num: rule.syscall,
                    valid: true,
                    chains: new_chains,
                    action: new_action,
                };
                match at {
                    Some(i) => self.syscalls.insert(i, entry),
                    None => self.syscalls.push(entry),
                }
                return Ok(());
            }
        };
        let mut new_chains = new_chains;
        loop {
            let sys = self.syscalls.get(s).ok_or(Error::Corrupt)?.clone();
            if sys.chains.is_none() {
                if rm_flag || !sys.valid {
                    let sm = self.syscalls.get_mut(s).ok_or(Error::Corrupt)?;
                    sm.chains = new_chains;
                    sm.action = new_action;
                    sm.valid = true;
                } else {
                    // The existing filter is at least as large as the new entry.
                    self.tree_put(&mut new_chains)?;
                }
                return Ok(());
            }
            if new_chains.is_none() {
                // The new rule has no chains: the existing ones go.
                self.put_tree_place(Place::Chains(s))?;
                let sm = self.syscalls.get_mut(s).ok_or(Error::Corrupt)?;
                sm.chains = None;
                sm.action = rule.action;
                return Ok(());
            }
            let mut state = IterState {
                flags: 0,
                action: rule.action,
                sx: s,
            };
            let existing = self.syscalls.get(s).ok_or(Error::Corrupt)?.chains;
            let rc = self.tree_prune(existing, new_chains, &mut state)?;
            if rc > 0 {
                rm_flag = true;
                if self.syscalls.get(s).ok_or(Error::Corrupt)?.chains.is_none() {
                    // The whole tree was pruned.
                    continue;
                }
            } else if (state.flags & IST_M_REDUNDANT) == IST_M_REDUNDANT {
                // The existing tree is "shorter": the new one goes.
                self.tree_put(&mut new_chains)?;
                return Ok(());
            }
            let new = new_chains.ok_or(Error::Corrupt)?;
            self.tree_add(Place::Chains(s), new)?;
            self.tree_put(&mut new_chains)?;
            return Ok(());
        }
    }

    fn put_tree_place(&mut self, p: Place) -> Result<u32> {
        let mut v = self.get_place(p)?;
        let cnt = self.tree_put(&mut v)?;
        self.set_place(p, v)?;
        Ok(cnt)
    }
}
