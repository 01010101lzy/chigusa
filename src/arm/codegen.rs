use super::{
    util::{CycleSolver, Interval},
    *,
};
use crate::mir;
use bimap::BiMap;
use indexmap::IndexMap;
use once_cell::sync::Lazy;
use std::{
    cmp::{max, min},
    collections::{HashMap, HashSet, VecDeque},
};
use vec1::{vec1, Vec1};

pub struct Codegen<'src> {
    src: &'src mir::MirPackage,
}

impl<'src> Codegen<'src> {
    pub fn new(src: &'src mir::MirPackage) -> Self {
        Codegen { src }
    }
    pub fn gen(&mut self) {
        for (id, f) in &self.src.func_table {
            let mut fc = FnCodegen::new(f);
            fc.gen();
        }
    }
}

#[derive(Debug)]
pub struct FnCodegen<'src> {
    src: &'src mir::Func,
    bb_arrangement: Vec<mir::BBId>,
    bb_start_pos: IndexMap<usize, usize>,

    reg_alloc: SecondChanceBinPackingRegAlloc<'src>,
}

impl<'src> FnCodegen<'src> {
    pub fn new(src: &'src mir::Func) -> Self {
        FnCodegen {
            src,
            bb_arrangement: Vec::new(),
            bb_start_pos: IndexMap::new(),
            // live_intervals: IndexMap::new(),
            // var_collapse: IndexMap::new(),
            reg_alloc: SecondChanceBinPackingRegAlloc::new(src),
        }
    }

    /// Generate a basic block arrangement that is good enough for a structured
    /// program. _We don't have `goto`-s anyway!_
    fn arrange_basic_blocks(&mut self) {
        let mut cycle_solver = super::util::CycleSolver::new(&self.src.bb);
        cycle_solver.solve();

        let mut input_count = HashMap::<usize, isize>::new();
        let cycle_count = cycle_solver.counter;

        // starting block
        input_count.insert(0, 1);
        for (id, blk) in &self.src.bb {
            for input_blk in &blk.jump_in {
                input_count
                    .entry(*id)
                    .and_modify(|count| *count = *count + 1)
                    .or_insert(1);
            }
        }

        let mut bfs_q = VecDeque::new();
        bfs_q.push_back(0);
        let mut vis = HashSet::new();

        while !bfs_q.is_empty() {
            let bb_id = bfs_q.pop_front().unwrap();
            let count = input_count.get_mut(&bb_id).unwrap();

            *count -= 1;
            if *count > cycle_count.get(&bb_id).cloned().unwrap_or(0) {
                continue;
            }
            if vis.contains(&bb_id) {
                continue;
            } else {
                vis.insert(bb_id);
            }

            self.bb_arrangement.push(bb_id);

            let blk = self.src.bb.get(&bb_id).unwrap();
            match &blk.end {
                mir::JumpInst::Jump(id) => {
                    bfs_q.push_back(*id);
                }
                mir::JumpInst::Conditional(_, t, f) => {
                    bfs_q.push_back(*t);
                    bfs_q.push_back(*f);
                }
                mir::JumpInst::Return(_) | mir::JumpInst::Unreachable | mir::JumpInst::Unknown => {
                    // Noop
                }
            }
        }
    }

    fn calc_bb_starting_points(&mut self) {
        let mut acc = 0;
        for id in self.bb_arrangement.iter().cloned() {
            self.bb_start_pos.insert(id, acc);
            acc += self.src.bb.get(&id).unwrap().inst.len() + 1;
        }
    }

    pub fn scan_intervals(&mut self) {
        self.arrange_basic_blocks();
        self.calc_bb_starting_points();
        for bb_id in self.bb_arrangement.iter().cloned() {
            let bb = self.src.bb.get(&bb_id).unwrap();
            let offset = *self.bb_start_pos.get(&bb_id).unwrap();

            let mut bb_next_vars = HashSet::new();
            for next_id in &bb.end.next_ids()[..] {
                bb_next_vars.extend(self.src.bb.get(next_id).unwrap().uses_var.iter().cloned());
            }

            let mut bb_interval_scanner = BasicBlkIntervals::new(
                offset,
                bb,
                &bb_next_vars,
                &mut self.reg_alloc.live_intervals,
                &mut self.reg_alloc.var_collapse,
            );

            bb_interval_scanner.scan_intervals();
        }
        log::debug!("{:#?}", self);
    }

    pub fn get_collapsed_var_varref(&mut self, var: &mir::VarRef) -> mir::VarRef {
        match var.0 {
            mir::VarTy::Global => *var,
            mir::VarTy::Local => {
                let res = self.get_collapsed_var(var.1);
                mir::VarRef(mir::VarTy::Local, res)
            }
        }
    }
    fn get_collapsed_var_optional(&mut self, var: usize) -> Option<usize> {
        if let Some(&v) = self.reg_alloc.var_collapse.get(&var) {
            let res = self.get_collapsed_var_optional(v);
            if let Some(res) = res {
                // 并查集行为
                self.reg_alloc.var_collapse.insert(var, res);
                Some(res)
            } else {
                Some(v)
            }
        } else {
            None
        }
    }

    fn get_collapsed_var(&mut self, var: usize) -> usize {
        let res = self.get_collapsed_var_optional(var).unwrap_or(var);
        res
    }

    fn set_param_and_ret_registers(&mut self) {
        let mut param_register_size = 0;
        for (&idx, var) in &self.src.var_table {
            if var.kind == mir::VarKind::Param {
                // * we ARE iterating variables in the same way they are declared
                let var_reg_size = var.ty.register_count();
                if var.ty.require_double_registers() {
                    todo!("Support doubles")
                }
                if param_register_size + var_reg_size < RESULT_REGISTERS.len() {
                    // Allocate register
                    assert!(var_reg_size == 1, "only int-s are supported");
                    self.reg_alloc.allocate_register(
                        idx,
                        PARAM_REGISTERS
                            .get_index(param_register_size)
                            .cloned()
                            .unwrap(),
                        0,
                        self.get_var_interval(idx),
                    );
                    param_register_size += 1;
                } else {
                    // spill param onto stack
                    self.reg_alloc.spill_var(idx, 0);
                }
            } else if var.kind == mir::VarKind::Ret {
                let var_reg_size = var.ty.register_count();
                if var.ty.require_double_registers() {
                    todo!("Support doubles")
                }

                assert!(var_reg_size == 1, "only int-s are supported");
                self.reg_alloc.allocate_register(
                    idx,
                    RESULT_REGISTERS.get_index(0).cloned().unwrap(),
                    0,
                    self.get_var_interval(idx),
                );
            }
        }
    }

    fn get_var_interval(&self, idx: mir::VarId) -> Interval {
        *self.reg_alloc.live_intervals.get(&idx).unwrap()
    }

    fn scan_body(&mut self) {
        for bb in self.bb_arrangement.iter().cloned() {
            let bb = self.src.bb.get(&bb).unwrap();
            for inst in &bb.inst {}
        }
    }

    pub fn assign_registers(&mut self) {}

    pub fn gen_assembly(&mut self) {}

    pub fn gen(&mut self) {
        self.scan_intervals();
    }
}

struct BasicBlkIntervals<'src> {
    offset: usize,
    bb: &'src mir::BasicBlk,
    // bb_prev_vars: &'src HashSet<mir::VarId>,
    bb_next_vars: &'src HashSet<mir::VarId>,
    intervals: &'src mut IndexMap<usize, Interval>,
    var_collapse: &'src mut IndexMap<usize, usize>,
}

impl<'src> BasicBlkIntervals<'src> {
    pub(super) fn new(
        offset: usize,
        bb: &'src mir::BasicBlk,
        // bb_prev_vars: &'src HashSet<mir::VarId>,
        bb_next_vars: &'src HashSet<mir::VarId>,
        intervals: &'src mut IndexMap<usize, Interval>,
        var_collapse: &'src mut IndexMap<usize, usize>,
    ) -> Self {
        BasicBlkIntervals {
            offset,
            bb,
            // bb_prev_vars,
            bb_next_vars,
            intervals,
            var_collapse,
        }
    }

    pub fn get_collapsed_var_varref(&mut self, var: &mir::VarRef) -> mir::VarRef {
        match var.0 {
            mir::VarTy::Global => *var,
            mir::VarTy::Local => {
                let res = self.get_collapsed_var(var.1);
                mir::VarRef(mir::VarTy::Local, res)
            }
        }
    }
    fn get_collapsed_var_optional(&mut self, var: usize) -> Option<usize> {
        if let Some(&v) = self.var_collapse.get(&var) {
            let res = self.get_collapsed_var_optional(v);
            if let Some(res) = res {
                // 并查集行为
                self.var_collapse.insert(var, res);
                Some(res)
            } else {
                Some(v)
            }
        } else {
            None
        }
    }

    fn get_collapsed_var(&mut self, var: usize) -> usize {
        let res = self.get_collapsed_var_optional(var).unwrap_or(var);
        res
    }

    /// Collapse variable as aliases of a single variable. The target variable
    /// id **must** be the **smallest** of them all.
    fn collapse_var<I>(&mut self, var: usize, targets: I)
    where
        I: Iterator<Item = usize>,
    {
        let var = self.get_collapsed_var(var);
        for target in targets {
            assert!(target >= var);
            if target == var {
                continue;
            }
            // if target has already collapsed into another var, also collapse that
            let target = self.get_collapsed_var(target);
            let res = self.var_collapse.insert(target, var);
            assert!(res.is_none());
        }
    }

    fn collapse_intervals<I>(&mut self, vars: I, default_pos: usize)
    where
        I: Iterator<Item = usize>,
    {
        let mut v = vars.collect::<Vec<_>>();
        v.sort();

        let collapse_tgt = self.get_collapsed_var(v[0]);
        let orig_interval = self
            .intervals
            .entry(collapse_tgt)
            .or_insert_with(|| Interval::point(default_pos))
            .clone();

        let new_interval = v.iter().skip(1).fold(orig_interval, |interval, next_k| {
            let var_interval = self
                .intervals
                .remove(next_k)
                .unwrap_or_else(|| Interval::point(default_pos));
            Interval::union(interval, var_interval)
        });

        self.intervals.insert(collapse_tgt, new_interval);

        self.collapse_var(collapse_tgt, v.iter().skip(1).cloned());
    }

    fn interval_start(&mut self, var: usize, pos: usize) {
        let var = self.get_collapsed_var(var);
        self.intervals
            .entry(var)
            .and_modify(|entry| entry.update_starting_pos(pos))
            .or_insert_with(|| Interval::point(pos));
    }

    fn interval_end(&mut self, var: usize, pos: usize) {
        let var = self.get_collapsed_var(var);
        self.intervals
            .entry(var)
            .and_modify(|entry| entry.update_ending_pos(pos))
            .or_insert_with(|| Interval::point(pos));
    }

    fn var_interval_start(&mut self, val: &mir::VarRef, pos: usize) {
        match &val.0 {
            mir::VarTy::Global => {
                // Global value is directly dereferenced into variable
            }
            mir::VarTy::Local => self.interval_start(val.1, pos),
        }
    }

    fn value_interval_end(&mut self, val: &mir::Value, pos: usize) {
        match val {
            mir::Value::Var(v) => match &v.0 {
                mir::VarTy::Global => todo!(),
                mir::VarTy::Local => self.interval_end(v.1, pos),
            },
            _ => {}
        }
    }

    pub fn scan_intervals(&mut self) {
        let self_offset = self.offset;

        for var in self.bb.uses_var.iter().cloned() {
            self.interval_start(var, self_offset);
        }

        for (pos, inst) in self
            .bb
            .inst
            .iter()
            .enumerate()
            .map(|(idx, inst)| (idx + self_offset, inst))
        {
            self.var_interval_start(&inst.tgt, pos);
            match &inst.ins {
                mir::Ins::TyCon(val) => self.value_interval_end(val, pos),
                mir::Ins::Asn(val) => self.value_interval_end(val, pos),
                mir::Ins::Bin(_, l, r) => {
                    self.value_interval_end(l, pos);
                    self.value_interval_end(r, pos)
                }
                mir::Ins::Una(_, val) => self.value_interval_end(val, pos),
                mir::Ins::Call(_, params) => {
                    for val in params {
                        self.value_interval_end(val, pos)
                    }
                }
                mir::Ins::Phi(vals) => self.collapse_intervals(
                    std::iter::once(inst.tgt.get_local_id())
                        .chain(vals.iter().map(|k| k.1.get_local_id()))
                        .filter(Option::is_some)
                        .map(|x| x.unwrap()),
                    pos,
                ),
                mir::Ins::RestRead(_) => todo!("Unsupported"),
            }
        }

        let self_end = self_offset + self.bb.inst.len();
        match &self.bb.end {
            mir::JumpInst::Conditional(v, ..) => self.value_interval_end(v, self_end),
            mir::JumpInst::Return(v) => {
                if let Some(v) = v {
                    self.value_interval_end(v, self_end)
                }
            }
            _ => {}
        };

        for var in self.bb_next_vars.iter().cloned() {
            self.interval_end(var, self_end + 1);
        }

        // Sort the intervals by their starting point
        self.intervals.sort_by(|_, v1, _, v2| v1.0.cmp(&v2.0))
    }
}

/// This struct uses a simplified version of Second-chance binpacking register
/// allocation algorithm.
///
/// The SCB algorithm is described in <https://www.researchgate.net/publication/221302629>
#[derive(Debug)]
struct SecondChanceBinPackingRegAlloc<'src> {
    src: &'src mir::Func,
    // === Register Allocation State ===
    pub assignment: IndexMap<mir::VarId, Vec1<(Interval, Reg)>>,
    pub active: BiMap<mir::VarId, Reg>,
    pub spilled: IndexMap<mir::VarId, Vec1<Interval>>,
    pub pre_allocated: HashSet<mir::VarId>,
    pub all_used_reg: HashSet<Reg>,

    pub live_intervals: IndexMap<usize, Interval>,
    pub var_collapse: IndexMap<usize, usize>,

    scratch_register_counter: usize,
}

impl<'src> SecondChanceBinPackingRegAlloc<'src> {
    pub fn new(src: &'src mir::Func) -> Self {
        SecondChanceBinPackingRegAlloc {
            src,
            assignment: IndexMap::new(),
            active: BiMap::new(),
            spilled: IndexMap::new(),
            all_used_reg: HashSet::new(),
            pre_allocated: HashSet::new(),
            live_intervals: IndexMap::new(),
            var_collapse: IndexMap::new(),
            scratch_register_counter: usize::max_value(),
        }
    }

    pub fn allocate_register(
        &mut self,
        var_id: mir::VarId,
        reg: Reg,
        pos: usize,
        val_interval: Interval,
    ) {
        let entry = self.assignment.entry(var_id);
        match entry {
            indexmap::map::Entry::Occupied(mut e) => {
                // if a variable has an entry and needs to allocate again
                // then it must be spilled somewhere else
                let v = e.get_mut();
                assert!(
                    v.iter().all(|(interval, _)| !interval.is_inside_write(pos)),
                    "No duplicate allocations"
                );
                let spilled = self.spilled.get_mut(&var_id).unwrap();
                let new_interval = spilled.last_mut().split(pos);
                v.push((new_interval, reg));
            }
            indexmap::map::Entry::Vacant(e) => {
                let spilled = self.spilled.get_mut(&var_id);
                let interval = if let Some(intervals) = spilled {
                    // this variable is located in the stack from the beginning
                    intervals.last_mut().split(pos)
                } else {
                    val_interval
                };
                e.insert(vec1![(interval, reg)]);
            }
        };
        self.active.insert(var_id, reg);
    }

    fn spill_reg(&mut self, reg: Reg, pos: usize) {
        let &var_id = self.active.get_by_right(&reg).expect("Unknown register");
        self.spill_var(var_id, pos)
    }

    fn spill_var(&mut self, var_id: mir::VarId, pos: usize) {
        let entry = self.assignment.entry(var_id);
        match entry {
            indexmap::map::Entry::Occupied(mut entry) => {
                // Spill a variable from its last assignment
                let val = entry.get_mut();
                let new_interval = val.last_mut().0.split(pos);

                self.spilled
                    .entry(var_id)
                    .and_modify(|intervals| intervals.push(new_interval))
                    .or_insert_with(|| vec1![new_interval]);
            }
            indexmap::map::Entry::Vacant(_) => panic!("The variable is not allocated!"),
        }
        self.active.remove_by_left(&var_id);
    }

    fn scan_and_desctivate(&mut self, pos: usize) {
        for variable in self.active.left_values().cloned().collect::<Vec<_>>() {
            let is_active = self
                .live_intervals
                .get(&variable)
                .map_or(false, |interval| interval.alive_for_reading(pos));
            if !is_active {
                self.active.remove_by_left(&variable);
            }
        }
    }

    pub fn is_spilled(&self, var_id: mir::VarId, pos: usize) -> bool {
        self.spilled.get(&var_id).map_or(false, |intervals| {
            intervals
                .iter()
                .any(|interval| interval.is_inside_write(pos))
        })
    }

    pub fn active_intersects(&self, allowed_regs: &HashSet<Reg>) -> HashSet<Reg> {
        self.active
            .right_values()
            .filter_map(|val| {
                if allowed_regs.contains(val) {
                    Some(*val)
                } else {
                    None
                }
            })
            .collect()
    }

    /// Choose one register to spill. longer-lived registers have a higher precedence.
    pub fn choose_spill_register(&self, allowed_regs: &IndexSet<Reg>) -> Option<Reg> {
        let mut regs: Vec<_> = (&self.active)
            .iter()
            // filter all registers that cannot be spilled
            .filter(|&(v, _r)| {
                !matches!(
                    self.src.var_table.get(v).unwrap().kind,
                    mir::VarKind::FixedTemp | mir::VarKind::Ret
                )
            })
            // filter out all allowed registers
            .filter(|&(_v, r)| allowed_regs.contains(r))
            .map(|(&v, &r)| (v, r))
            .collect();

        regs.sort_by_cached_key(|(v, _r)| {
            self.live_intervals.get(v).map(|int| int.len()).unwrap_or(0)
        });

        regs.last().map(|(_v, r)| r).cloned()
    }

    /// Find the register occupied by the current variable, or spill a register and
    /// allocate the current variable to satisfy the need. This method assumes
    /// that handled variables are already removed from active set.
    pub fn find_allocate_or_spill(
        &mut self,
        var_id: mir::VarId,
        allowed_regs: &IndexSet<Reg>,
        interval: Interval,
        pos: usize,
    ) -> Reg {
        if let Some(&reg) = self.active.get_by_left(&var_id) {
            reg
        } else {
            let mut avail_regs = allowed_regs
                .iter()
                // filter all registers that hasn't been occupied
                .filter(|reg| !self.active.contains_right(reg));

            // get the first register available
            if let Some(&reg) = avail_regs.next() {
                // There's an empty register
                self.allocate_register(var_id, reg, pos, interval);
                reg
            } else {
                // No empty registers, spill one from active.
                let spilled = self.choose_spill_register(allowed_regs);
                if let Some(reg) = spilled {
                    self.spill_reg(reg, pos);
                    self.allocate_register(var_id, reg, pos, interval);
                    reg
                } else {
                    panic!("No register to spill! This is an internal error");
                }
            }
        }
    }

    fn revive(&mut self, var_id: mir::VarId, pos: usize) -> Interval {
        let spill_intervals = self
            .spilled
            .get_mut(&var_id)
            .expect("The variable is not spilled");

        let last_spill = spill_intervals.last_mut();
        assert!(
            last_spill.alive_for_reading(pos),
            "Reading variable outside live interval"
        );

        let new_interval = last_spill.split(pos);
        new_interval
    }

    /// Request to allocate a register for reading the variable, or return the
    /// register already allocated for it
    pub fn request_read_allocation(&mut self, var_id: mir::VarId, pos: usize) -> Reg {
        let last_allocation = *self
            .assignment
            .get(&var_id)
            .expect("Read variable before write!")
            .last();

        if last_allocation.0.alive_for_reading(pos) {
            last_allocation.1
        } else {
            // The value might be spilled
            let new_interval = self.revive(var_id, pos);
            let reg = self.find_allocate_or_spill(var_id, &*VARIABLE_REGISTERS, new_interval, pos);
            self.allocate_register(var_id, reg, pos, new_interval);

            reg
        }
    }

    /// Request to allocate a register
    pub fn request_write_allocation(
        &mut self,
        var_id: mir::VarId,
        // var_kind: mir::VarKind,
        pos: usize,
        interval: Interval,
    ) -> Reg {
        let last_allocation = self.assignment.entry(var_id);
        match last_allocation {
            indexmap::map::Entry::Occupied(e) => {
                let last_allocation = e.get().last();
                if last_allocation.0.alive_for_reading(pos) {
                    last_allocation.1
                } else {
                    // variable is spilled
                    let interval = self.revive(var_id, pos);
                    let reg =
                        self.find_allocate_or_spill(var_id, &*VARIABLE_REGISTERS, interval, pos);
                    reg
                }
            }
            indexmap::map::Entry::Vacant(_v) => {
                // variable is not yet allocated
                let reg = self.find_allocate_or_spill(var_id, &*VARIABLE_REGISTERS, interval, pos);
                reg
            }
        }
    }

    /// Request to allocate a register that is only alive inside current MIR
    /// instruction.
    ///
    /// The number of scratch registers must be less than the total number of
    /// registers; If no register could be allocated, the method panics.
    pub fn request_scratch_register(&mut self, pos: usize) -> Reg {
        let var_id = self.scratch_register_counter;
        self.scratch_register_counter -= 1;

        self.find_allocate_or_spill(
            var_id,
            &SCRATCH_VARIABLE_ALLOWED_REGISTERS,
            Interval::point(pos),
            pos,
        )
    }

    ///
    pub fn request_allocate_memory(&mut self, var_id: mir::VarId, pos: usize) {
        self.spill_var(var_id, pos)
    }

    ///
    pub fn force_free_register(&mut self, reg: Reg, pos: usize) {
        self.spill_reg(reg, pos)
    }
}
