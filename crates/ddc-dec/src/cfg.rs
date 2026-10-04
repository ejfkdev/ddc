//! Basic-block splitting and CFG construction for Dalvik code units.
//!
//! Mirrors jcdc's `block.rs` but for a register machine: leaders come from
//! branch targets, post-branch pcs, try boundaries and handler entries.
//! Payload pseudo-instructions are never leaders (they are data).

use ddc_dex::code::CodeItem;
use ddc_dex::insn::{Insn, InsnKind, Payload};

#[derive(Debug, Clone)]
pub struct Block {
    pub id: usize,
    /// First code unit (inclusive).
    pub start: u32,
    /// End code unit (exclusive).
    pub end: u32,
    /// Instruction INDEX range `[lo, hi)` into the code item's linear
    /// `insns` stream (blocks are contiguous pc ranges of a linear decode —
    /// the previous per-block `Vec<Insn>` CLONE was a top allocation cost).
    pub ins_lo: usize,
    pub ins_hi: usize,
    /// Successors. Conditional: `[fallthrough, taken]`; switch: default
    /// first, then key order.
    pub succ: Vec<usize>,
    pub pred: Vec<usize>,
    /// Exception range indices whose handler this block is.
    pub handlers: Vec<usize>,
}

#[derive(Debug, Clone)]
pub struct ExcRange {
    /// Code-unit range `[start, end)`.
    pub start: u32,
    pub end: u32,
    /// Handler entry code unit.
    pub handler: u32,
    /// `None` = catch-all.
    pub catch_type: Option<std::sync::Arc<str>>,
}

pub struct DexCfg {
    pub blocks: Vec<Block>,
    pub entry: usize,
    pub exc_ranges: Vec<ExcRange>,
    /// The method's linear decode stream, moved in from the `CodeItem`.
    /// Blocks reference it via `ins_lo..ins_hi` index ranges.
    pub insns: Vec<Insn>,
    /// Total code units (pc of one-past-the-last instruction).
    pub code_units: u32,
    /// Block start pcs sorted for `block_at`.
    starts: Vec<u32>,
}

impl DexCfg {
    /// Build the CFG for one method body. `type_name` resolves type ids for
    /// catch types. Takes the `CodeItem` by `&mut` and MOVES its `insns`
    /// stream into the CFG — blocks then index into it instead of each
    /// owning a deep clone (the old per-block `Vec<Insn>` copies summed to a
    /// second full decode of every method; on weibo that was tens of
    /// millions of instruction clones).
    pub fn build(code: &mut CodeItem, type_name: &dyn Fn(u32) -> std::sync::Arc<str>) -> DexCfg {
        let insns = std::mem::take(&mut code.insns);
        let code_units: u32 = insns.last().map(|last| last.pc + last.size).unwrap_or(0);

        // 1. Leaders.
        let mut leaders: Vec<u32> = vec![0];
        for ins in insns.iter() {
            let next = ins.pc + ins.size;
            if ins.is_branch() && next <= code_units {
                leaders.push(next);
            }
            for t in insn_targets(ins) {
                leaders.push(t);
            }
            // Switch CASE targets are control-flow leaders too. Ordinary
            // switches get their case heads for free (each case ends in
            // `goto break`, and a branch's fall-through is a leader) —
            // but the weixin protobuf `op()` shape ends every case in a
            // direct `return`, so NOTHING else marks the heads: all case
            // bodies fused into one block, every switch edge resolved to
            // that single block, and the structurer emitted empty
            // `case 1..6: break;` with the bodies linearized after the
            // switch (returns dropped — semantics change). The payload
            // LOCATION is data; the offsets INSIDE it are targets.
            match &ins.kind {
                InsnKind::PackedSwitch { payload_pc, .. }
                | InsnKind::SparseSwitch { payload_pc, .. } => {
                    for t in switch_targets(code.payloads.get(payload_pc), ins.pc) {
                        leaders.push(t);
                    }
                }
                _ => {}
            }
        }
        // Try boundaries and handler entries.
        let mut ranges: Vec<ExcRange> = Vec::new();
        for t in &code.tries {
            let end = t.start_addr.saturating_add(t.insn_count);
            if t.start_addr >= end || t.start_addr >= code_units {
                continue;
            }
            leaders.push(t.start_addr);
            if end < code_units {
                leaders.push(end);
            }
            if let Some(h) = code.handler_of(t) {
                for (ty, addr) in &h.catches {
                    if *addr < code_units {
                        leaders.push(*addr);
                        ranges.push(ExcRange {
                            start: t.start_addr,
                            end,
                            handler: *addr,
                            catch_type: Some(type_name(*ty)),
                        });
                    }
                }
                if let Some(addr) = h.catch_all {
                    if addr < code_units {
                        leaders.push(addr);
                        ranges.push(ExcRange {
                            start: t.start_addr,
                            end,
                            handler: addr,
                            catch_type: None,
                        });
                    }
                }
            }
        }
        leaders.retain(|&l| l < code_units || (l == 0 && code_units == 0));
        leaders.sort_unstable();
        leaders.dedup();
        if leaders.is_empty() {
            leaders.push(0);
        }

        // 2. pc → block id (last leader <= pc).
        let block_of_pc = |p: u32| -> Option<usize> {
            let i = leaders.partition_point(|&l| l <= p);
            if i == 0 {
                None
            } else {
                Some(i - 1)
            }
        };

        // 3. Blocks as index ranges over the linear instruction stream.
        // (pc-sorted insns; a block's instructions are exactly the stream
        // slice whose pcs fall in [start, end).)
        let mut blocks: Vec<Block> = Vec::with_capacity(leaders.len());
        for (id, &start) in leaders.iter().enumerate() {
            let end = leaders
                .get(id + 1)
                .copied()
                .unwrap_or(code_units)
                .max(start);
            let lo = insns.partition_point(|i| i.pc < start);
            let hi = insns.partition_point(|i| i.pc < end);
            blocks.push(Block {
                id,
                start,
                end,
                ins_lo: lo,
                ins_hi: hi,
                succ: Vec::new(),
                pred: Vec::new(),
                handlers: Vec::new(),
            });
        }

        // 4. Successors.
        for b in blocks.iter_mut() {
            if b.ins_hi <= b.ins_lo {
                continue;
            }
            let last = &insns[b.ins_hi - 1];
            match &last.kind {
                InsnKind::Goto { target } => {
                    if let Some(tb) = block_of_pc(*target) {
                        b.succ.push(tb);
                    }
                }
                InsnKind::If { target, .. } => {
                    let next_pc = last.pc + last.size;
                    if let Some(f) = block_of_pc(next_pc) {
                        b.succ.push(f);
                    }
                    if let Some(tb) = block_of_pc(*target) {
                        b.succ.push(tb);
                    }
                }
                InsnKind::PackedSwitch { payload_pc, .. }
                | InsnKind::SparseSwitch { payload_pc, .. } => {
                    let next_pc = last.pc + last.size;
                    if let Some(f) = block_of_pc(next_pc) {
                        b.succ.push(f); // default arm first
                    }
                    for t in switch_targets(code.payloads.get(payload_pc), last.pc) {
                        if let Some(tb) = block_of_pc(t) {
                            if !b.succ.contains(&tb) {
                                b.succ.push(tb);
                            }
                        }
                    }
                }
                InsnKind::ReturnVoid | InsnKind::Return { .. } | InsnKind::Throw { .. } => {}
                _ => {
                    let next_pc = last.pc + last.size;
                    if next_pc < code_units {
                        if let Some(f) = block_of_pc(next_pc) {
                            b.succ.push(f);
                        }
                    }
                }
            }
        }
        for b in blocks.iter_mut() {
            let mut seen = Vec::new();
            b.succ.retain(|s| {
                if seen.contains(s) {
                    false
                } else {
                    seen.push(*s);
                    true
                }
            });
        }

        // 5. Predecessors + handler back-references (jcdc semantics: a block
        // is protected when its FIRST offset lies in [start, end)).
        for b in 0..blocks.len() {
            let succ = blocks[b].succ.clone();
            for s in succ {
                if s < blocks.len() {
                    blocks[s].pred.push(b);
                }
            }
        }
        for (ri, r) in ranges.iter().enumerate() {
            if let Some(h) = blocks.iter().position(|b| b.start == r.handler) {
                blocks[h].handlers.push(ri);
            }
        }
        for b in blocks.iter_mut() {
            b.pred.sort_unstable();
            b.pred.dedup();
            b.handlers.sort_unstable();
            b.handlers.dedup();
        }

        let mut starts = leaders;
        starts.sort_unstable();
        DexCfg {
            blocks,
            entry: 0,
            exc_ranges: ranges,
            insns,
            code_units,
            starts,
        }
    }

    /// The block's instructions (slice of the linear decode stream).
    /// Blocks in reverse postorder over NORMAL (successor) edges;
    /// blocks unreachable that way — exception handlers, dead code —
    /// appended afterwards, ordered by start pc. A deterministic
    /// execution-order approximation: strictly better than pc order,
    /// which interleaves handler bodies into the flow they interrupt
    /// (compute_final_reads consumed allocation views at reads whose
    /// real successors still read the register).
    pub fn rpo_order(&self) -> Vec<usize> {
        let n = self.blocks.len();
        // Iterative postorder DFS over succ (entry first).
        let mut seen = vec![false; n];
        let mut post: Vec<usize> = Vec::new();
        if n > 0 {
            let mut stack: Vec<(usize, usize)> = vec![(0, 0)];
            seen[0] = true;
            while let Some(top) = stack.last_mut() {
                let (b, i) = *top;
                if i < self.blocks[b].succ.len() {
                    top.1 += 1;
                    let s = self.blocks[b].succ[i];
                    if s < n && !seen[s] {
                        seen[s] = true;
                        stack.push((s, 0));
                    }
                } else {
                    post.push(b);
                    stack.pop();
                }
            }
        }
        let mut order: Vec<usize> = post.into_iter().rev().collect();
        // Unreached blocks (exception-handler entries, dead code) after.
        let mut rest: Vec<usize> = (0..n).filter(|&b| !seen[b]).collect();
        rest.sort_unstable_by_key(|&b| self.blocks[b].start);
        order.extend(rest);
        order
    }
    pub fn block_ins(&self, b: &Block) -> &[Insn] {
        &self.insns[b.ins_lo..b.ins_hi]
    }

    /// The block's last instruction.
    pub fn block_last(&self, b: &Block) -> Option<&Insn> {
        if b.ins_hi > b.ins_lo {
            self.insns.get(b.ins_hi - 1)
        } else {
            None
        }
    }

    pub fn block_at(&self, p: u32) -> Option<usize> {
        let i = self.starts.partition_point(|&l| l <= p);
        if i == 0 {
            None
        } else {
            Some(i - 1)
        }
    }

    /// Exception edges `(range idx, from block, to block)` derived from the
    /// protected-region rule (first offset inside `[start, end)`, has code).
    pub fn exc_edges(&self) -> Vec<(usize, usize, usize)> {
        let mut out = Vec::new();
        for (ri, r) in self.exc_ranges.iter().enumerate() {
            if let Some(h) = self.blocks.iter().position(|b| b.start == r.handler) {
                for b in &self.blocks {
                    if b.start >= r.start && b.start < r.end && b.ins_hi > b.ins_lo {
                        out.push((ri, b.id, h));
                    }
                }
            }
        }
        out
    }

    /// Machine-neutral view for the shared structurer (ids preserved).
    pub fn to_core(&self) -> jdc_core::cfg::Cfg {
        let blocks = self
            .blocks
            .iter()
            .map(|b| jdc_core::cfg::Block {
                id: b.id,
                start: b.start,
                end: b.end,
                ins_len: (b.ins_hi - b.ins_lo) as u32,
                succ: b.succ.clone(),
                pred: b.pred.clone(),
                handlers: b.handlers.iter().map(|&h| h as u32).collect(),
            })
            .collect();
        let ranges = self
            .exc_ranges
            .iter()
            .map(|r| jdc_core::cfg::ExcRange {
                start: r.start,
                end: r.end,
                handler: r.handler,
                catch_type: r.catch_type.clone(),
            })
            .collect();
        let edges = self
            .exc_edges()
            .into_iter()
            .map(|(range, from, to)| jdc_core::cfg::ExcEdge { range, from, to })
            .collect();
        jdc_core::cfg::Cfg::from_parts(blocks, self.entry, ranges, edges)
    }
}

/// Absolute branch targets of one instruction.
fn insn_targets(ins: &Insn) -> Vec<u32> {
    match &ins.kind {
        InsnKind::Goto { target } => vec![*target],
        InsnKind::If { target, .. } => vec![*target],
        // Payload locations are data, not control targets.
        _ => Vec::new(),
    }
}

/// Absolute case targets of a switch from its payload.
pub fn switch_targets(payload: Option<&Payload>, switch_pc: u32) -> Vec<u32> {
    match payload {
        Some(Payload::Packed { targets, .. }) => targets
            .iter()
            .map(|t| (switch_pc as i64 + *t as i64) as u32)
            .collect(),
        Some(Payload::Sparse { pairs }) => pairs
            .iter()
            .map(|(_, t)| (switch_pc as i64 + *t as i64) as u32)
            .collect(),
        _ => Vec::new(),
    }
}

/// Switch keys for `Term::Switch` construction.
pub fn switch_keys(payload: Option<&Payload>) -> SwitchKeys {
    match payload {
        Some(Payload::Packed { first_key, targets }) => SwitchKeys::Table {
            low: *first_key,
            targets: targets.clone(),
        },
        Some(Payload::Sparse { pairs }) => SwitchKeys::Lookup {
            pairs: pairs.clone(),
        },
        _ => SwitchKeys::None,
    }
}

pub enum SwitchKeys {
    Table { low: i32, targets: Vec<i32> },
    Lookup { pairs: Vec<(i32, i32)> },
    None,
}
