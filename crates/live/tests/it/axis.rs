//! The axis: positions and ids, inserts into the implicit tail, and replicas that agree.

use std::collections::BTreeSet;

use craft_collab::Replica;
use gridcraft_live::formula_ids::Lookup;
use gridcraft_live::{Axis, AxisKind, GridDoc, GridSeed, Id, Op};

use crate::{Rng, seeds};

#[test]
fn insert_in_the_implicit_tail_materialises_up_to_it() {
    let mut axis = Axis::base(1000);
    let before: Vec<Id> = (0..40).map(|p| axis.id_at(p).unwrap()).collect();
    let new = [Id::minted(3, 1), Id::minted(3, 2)];
    let ins = axis.insert(12, &new);
    assert_eq!(ins.materialise, before[..12], "the lines above the insert are written out first");
    assert_eq!(ins.prev, before.get(11).copied(), "the new lines follow the last line above them");
    let at = ins.index as usize;
    assert_eq!(&axis.explicit()[at..at + 2], &new, "the array index is where the new ids went");
    for p in 0..12 {
        assert_eq!(axis.id_at(p), Some(before[p as usize]), "a line above the insert stays");
    }
    assert_eq!((axis.id_at(12), axis.id_at(13)), (Some(new[0]), Some(new[1])));
    for p in 12..38 {
        assert_eq!(axis.id_at(p + 2), Some(before[p as usize]), "a line below moves down by the insert");
    }
    let again = axis.insert(3, &[Id::minted(3, 3)]);
    assert!(again.materialise.is_empty(), "an insert inside the written prefix writes out nothing more");
}

/// A naive axis: every line in one list, long enough that the tail never runs out.
struct Model {
    lines: Vec<(Id, bool)>,
    limit: u32,
}

impl Model {
    fn shown(&self) -> Vec<Id> {
        self.lines.iter().filter(|(_, gone)| !gone).map(|(id, _)| *id).take(self.limit as usize).collect()
    }
    fn index_of_shown(&self, pos: usize) -> usize {
        self.lines.iter().enumerate().filter(|(_, (_, gone))| !gone).nth(pos).map(|(i, _)| i).unwrap()
    }
    fn shown_before(&self, id: Id) -> usize {
        let i = self.lines.iter().position(|(x, _)| *x == id).unwrap();
        self.lines[..i].iter().filter(|(_, gone)| !gone).count()
    }
    fn set_gone(&mut self, ids: &[Id], gone: bool) {
        for (x, g) in &mut self.lines {
            if ids.contains(x) {
                *g = gone;
            }
        }
    }
}

fn expand(areas: &[(u32, u32)]) -> BTreeSet<u32> {
    areas.iter().flat_map(|(first, count)| *first..first + count).collect()
}

#[test]
fn positions_and_ids_are_inverse() {
    const LIMIT: u32 = 40;
    for seed in seeds() {
        let mut rng = Rng(seed);
        let mut axis = Axis::base(LIMIT);
        let mut model = Model { lines: (0..LIMIT * 8).map(|i| (Id::base(i), false)).collect(), limit: LIMIT };
        let mut minted = 0;
        for step in 0..80 {
            let what = rng.below(3);
            if what == 0 {
                let pos = rng.below(LIMIT as usize + 1) as u32;
                let ids: Vec<Id> = (0..1 + rng.below(3))
                    .map(|_| {
                        minted += 1;
                        Id::minted(5, minted)
                    })
                    .collect();
                let at = model.index_of_shown(pos as usize);
                model.lines.splice(at..at, ids.iter().map(|id| (*id, false)));
                axis.insert(pos, &ids);
            } else if what == 1 {
                let ids: Vec<Id> = (0..1 + rng.below(4)).map(|_| axis.id_at(rng.below(LIMIT as usize) as u32).unwrap()).collect();
                let was: BTreeSet<u32> = ids.iter().map(|id| model.shown().iter().position(|x| x == id).unwrap() as u32).collect();
                model.set_gone(&ids, true);
                let areas = axis.delete(&ids);
                assert_eq!(expand(&areas), was, "seed {seed} step {step}: delete names the positions the lines had");
                assert!(areas.windows(2).all(|w| w[0].0 + w[0].1 < w[1].0), "seed {seed} step {step}: areas ascend and do not touch");
            } else {
                let gone: Vec<Id> = model.lines.iter().filter(|(_, g)| *g).map(|(id, _)| *id).collect();
                if gone.is_empty() {
                    continue;
                }
                let ids: Vec<Id> = (0..1 + rng.below(3)).map(|_| gone[rng.below(gone.len())]).collect();
                model.set_gone(&ids, false);
                let areas = axis.undelete(&ids);
                let now: BTreeSet<u32> = ids.iter().filter_map(|id| model.shown().iter().position(|x| x == id)).map(|p| p as u32).collect();
                assert_eq!(expand(&areas), now, "seed {seed} step {step}: undelete names the positions the lines have now");
            }
            let shown = model.shown();
            for p in 0..LIMIT {
                let id = axis.id_at(p);
                assert_eq!(id, Some(shown[p as usize]), "seed {seed} step {step}: position {p} shows the model's line");
                assert_eq!(axis.pos_of(id.unwrap()), Lookup::At(p), "seed {seed} step {step}: pos_of(id_at({p}))");
            }
            assert_eq!(axis.id_at(LIMIT), None, "seed {seed}: nothing past the sheet");
            for (id, gone) in &model.lines {
                if *gone {
                    let q = model.shown_before(*id) as u32;
                    let want = Lookup::Deleted { before: q.checked_sub(1).filter(|p| *p < LIMIT), after: (q < LIMIT).then_some(q) };
                    assert_eq!(axis.pos_of(*id), want, "seed {seed} step {step}: a deleted line names its neighbours");
                }
            }
        }
    }
}

const SHEETS: u32 = 2;
const LIMIT: u32 = 30;

/// Apply up to `most` of the updates `doc` has not applied, in a random order, some twice.
fn deliver(rng: &mut Rng, doc: &mut GridDoc, applied: &mut BTreeSet<usize>, log: &[Vec<u8>], most: usize) {
    let pending: Vec<usize> = (0..log.len()).filter(|i| !applied.contains(i)).collect();
    let mut fresh = 0;
    for i in rng.order_with_duplicates(pending.len()) {
        let u = pending[i];
        if !applied.contains(&u) {
            if fresh == most {
                break;
            }
            fresh += 1;
        }
        doc.apply(&log[u]).unwrap();
        applied.insert(u);
    }
}

#[test]
fn axis_replicas_agree_under_serialised_ops() {
    for seed in seeds() {
        let mut rng = Rng(seed);
        let n = 2 + rng.below(7);
        let s = GridSeed { base_sha256: [seed as u8; 32], sheets: SHEETS };
        let mut docs: Vec<GridDoc> = (0..n)
            .map(|i| {
                let mut d = GridDoc::seeded(&s).unwrap();
                d.set_client(i as u32 + 1).unwrap();
                d
            })
            .collect();
        let mut applied: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); n];
        let mut log: Vec<Vec<u8>> = Vec::new();
        let kinds = [AxisKind::Rows, AxisKind::Cols];
        let mut models: Vec<Axis> = (0..SHEETS as usize * 2).map(|_| Axis::base(LIMIT)).collect();
        let mut minted = vec![0u32; n];
        for step in 0..40u64 {
            // The lock: the author has applied every op before it writes its own.
            let author = rng.below(n);
            deliver(&mut rng, &mut docs[author], &mut applied[author], &log, usize::MAX);
            let si = rng.below(SHEETS as usize);
            let ki = rng.below(2);
            let (sheet, kind) = (Id::base(si as u32), kinds[ki]);
            let mut axis = docs[author].axis(sheet, kind, LIMIT);
            let model = &mut models[si * 2 + ki];
            assert_eq!(&axis, model, "seed {seed} step {step}: the author holds every earlier op");
            let by = format!("peer{author}");
            let mut insertion = None;
            let op = match rng.below(3) {
                0 => {
                    let ids: Vec<Id> = (0..1 + rng.below(3))
                        .map(|_| {
                            minted[author] += 1;
                            Id::minted(author as u32 + 1, minted[author])
                        })
                        .collect();
                    let ins = axis.insert(rng.below(LIMIT as usize + 1) as u32, &ids);
                    insertion = Some(ins.clone());
                    Op::Insert { id: step, by, sheet, axis: kind, prev: ins.prev, ids }
                }
                1 => {
                    let ids: Vec<Id> = (0..1 + rng.below(3)).map(|_| axis.id_at(rng.below(LIMIT as usize) as u32).unwrap()).collect();
                    axis.delete(&ids);
                    Op::Delete { id: step, by, sheet, axis: kind, ids }
                }
                _ => {
                    let gone: Vec<Id> = axis.deleted().iter().copied().collect();
                    let ids: Vec<Id> = if gone.is_empty() { Vec::new() } else { vec![gone[rng.below(gone.len())]] };
                    axis.undelete(&ids);
                    Op::Undelete { id: step, by, sheet, axis: kind, ids }
                }
            };
            let update = docs[author]
                .write(|t| {
                    match &op {
                        Op::Insert { ids, .. } => {
                            if let Some(ins) = &insertion {
                                t.axis_insert(sheet, kind, ins, ids);
                            }
                        }
                        Op::Delete { ids, .. } => t.axis_delete(sheet, kind, ids),
                        Op::Undelete { ids, .. } => t.axis_undelete(sheet, kind, ids),
                    }
                    t.push_op(&op);
                })
                .unwrap();
            *model = axis.clone();
            log.push(update);
            applied[author].insert(log.len() - 1);
            assert_eq!(&docs[author].axis(sheet, kind, LIMIT), model, "seed {seed} step {step}: the write gives the axis the author worked out");
            // Others hear some of it, in any order, some of it twice.
            for (i, doc) in docs.iter_mut().enumerate() {
                if i != author && rng.chance(40) {
                    let k = 1 + rng.below(3);
                    deliver(&mut rng, doc, &mut applied[i], &log, k);
                }
            }
        }
        for (i, doc) in docs.iter_mut().enumerate() {
            deliver(&mut rng, doc, &mut applied[i], &log, usize::MAX);
        }
        for (i, doc) in docs.iter().enumerate() {
            assert_eq!(doc.ops_len() as usize, log.len(), "seed {seed}: peer {i} holds every op");
            assert_eq!(doc.ops_from(0), docs[0].ops_from(0), "seed {seed}: peer {i} reads the op log as peer 0 does");
            for si in 0..SHEETS as usize {
                for (ki, kind) in kinds.iter().enumerate() {
                    let axis = doc.axis(Id::base(si as u32), *kind, LIMIT);
                    assert_eq!(axis, models[si * 2 + ki], "seed {seed}: peer {i} sheet {si} {kind:?} agrees");
                    for id in axis.deleted() {
                        assert!(matches!(axis.pos_of(*id), Lookup::Deleted { .. }), "seed {seed}: deleted {id:?} is not shown");
                    }
                    let shown: BTreeSet<Id> = (0..LIMIT).filter_map(|p| axis.id_at(p)).collect();
                    assert!(shown.is_disjoint(axis.deleted()), "seed {seed}: no deleted line is shown");
                }
            }
        }
    }
}
