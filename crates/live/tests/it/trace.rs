//! Long seeded traces: two to eight peers mixing free commands, ordered commands, undo, redo,
//! link drops and rejoins, and a checkpoint joiner, in a seeded delivery order. At the end every
//! peer holds the same workbook, and that workbook is the base plus the replica's state.

use craft_live::{Access, LiveStatus};
use gridcraft_live::{volatile_for, workbook_from_state};
use serde_json::{Value as Json, json};

use crate::support::{Room, diff, finger};
use crate::{Rng, seeds_or};

/// Steps each peer takes.
const STEPS: usize = 300;
/// The peer counts, by seed: the first six seeds cover 2, 8 and four counts between.
const PEERS: [usize; 7] = [2, 8, 3, 6, 4, 7, 5];

fn col(c: usize) -> char {
    (b'A' + u8::try_from(c).unwrap_or(0)) as char
}

fn cell(rng: &mut Rng) -> String {
    format!("{}{}", col(rng.below(6)), 1 + rng.below(12))
}

fn range(rng: &mut Rng) -> String {
    let (r0, c0) = (rng.below(10), rng.below(5));
    format!("{}{}:{}{}", col(c0), 1 + r0, col(c0 + rng.below(2)), 1 + r0 + rng.below(3))
}

/// One random free command. `other` is the second sheet's name.
fn free(rng: &mut Rng, other: &str) -> (&'static str, Json) {
    let row = 1 + rng.below(12);
    let c = col(rng.below(6));
    match rng.below(18) {
        0..=2 => ("cell.set", json!({"cell": cell(rng), "input": format!("{}", rng.below(1000))})),
        3 => ("cell.set", json!({"cell": cell(rng), "input": format!("=A{}+B{}", 1 + rng.below(12), 1 + rng.below(12))})),
        4 => ("cell.set", json!({"cell": cell(rng), "input": format!("=SUM(A{row}:C{})", row + rng.below(4))})),
        5 => ("cell.set", json!({"cell": cell(rng), "input": format!("t{}", rng.below(60))})),
        6 => ("cell.set", json!({"sheet": other, "cell": cell(rng), "input": format!("{}", rng.below(99))})),
        7 => ("edit.clearContents", json!({"range": range(rng)})),
        8 => ("home.bold", json!({"range": range(rng)})),
        9 => ("home.italic", json!({"range": range(rng), "on": rng.chance(50)})),
        10 => ("home.fillColor", json!({"range": range(rng), "color": format!("#{:06X}", rng.below(0xFF_FFFF))})),
        11 => ("home.rowHeight", json!({"rows": format!("{row}:{row}"), "height": 20 + rng.below(40)})),
        12 => ("home.columnWidth", json!({"cols": format!("{c}:{c}"), "width": 60 + rng.below(80)})),
        13 => ("home.hideRows", json!({"rows": format!("{row}:{row}")})),
        14 => ("home.unhideRows", json!({"rows": format!("1:{}", row + 2)})),
        15 => ("home.hideColumns", json!({"cols": format!("{c}:{c}")})),
        16 => ("edit.fillDown", json!({"range": format!("{c}{row}:{c}{}", row + 1 + rng.below(3))})),
        _ => {
            let text = (0..2).map(|r| format!("p{r}\t{}", rng.below(9))).collect::<Vec<_>>().join("\n");
            ("edit.paste", json!({"at": cell(rng), "text": text}))
        }
    }
}

/// One random ordered command: rows or columns inserted or deleted.
fn ordered(rng: &mut Rng) -> (&'static str, Json) {
    let row = 1 + rng.below(12);
    let c = col(rng.below(6));
    match rng.below(4) {
        0 => ("home.insertRows", json!({"rows": format!("{row}:{}", row + rng.below(2))})),
        1 => ("home.deleteRows", json!({"rows": format!("{row}:{row}")})),
        2 => ("home.insertColumns", json!({"cols": format!("{c}:{c}")})),
        _ => ("home.deleteColumns", json!({"cols": format!("{c}:{c}")})),
    }
}

/// What a trace did, for the report and to show it exercised every kind of step.
#[derive(Default)]
struct Tally {
    free: usize,
    ordered: usize,
    undo: usize,
    redo: usize,
    drops: usize,
    rejoins: usize,
    joined: bool,
    refused: usize,
}

/// Every peer: Live, equal to peer 0, and equal to the base plus its own replica's state.
fn assert_equal(room: &Room, what: &str) {
    for (i, p) in room.peers.iter().enumerate() {
        assert!(matches!(p.live.status(), LiveStatus::Live { .. }), "{what}: peer {i} is {:?}; notices {:?}", p.live.status(), p.notices);
    }
    // Every peer against peer 0 and against the workbook its own replica's state rebuilds, all
    // in one message: which side is wrong shows at once.
    let first = room.finger(0);
    let base = room.base();
    let mut bad = Vec::new();
    for i in 0..room.peers.len() {
        let shown = room.finger(i);
        let state = room.replica_state(i);
        let source = volatile_for(&base, &state).unwrap();
        let rebuilt = finger(&workbook_from_state(&base, &state, Some(source)).unwrap());
        if shown != first {
            bad.push(format!("peer {i} differs from peer 0\n{}", diff(&first, &shown)));
        }
        if shown != rebuilt {
            bad.push(format!("peer {i} differs from its replica's state\n{}", diff(&rebuilt, &shown)));
        }
    }
    assert!(bad.is_empty(), "{what}:\n{}", bad.join("\n"));
}

fn hub_drop(room: &Room, i: usize) {
    room.relay.drop_link(room.peers[i].live.core().hub().inner().mem());
}

fn hub_restore(room: &Room, i: usize) {
    room.relay.restore_link(room.peers[i].live.core().hub().inner().mem());
}

/// A state the room's saver could have written: a connected peer's replica with nothing of its
/// own still unconfirmed, after the room went quiet.
fn checkpoint(room: &mut Room, down: &[bool]) -> Option<Vec<u8>> {
    room.settle();
    (0..room.peers.len())
        .find(|&i| {
            !down[i] && room.peers[i].live.core().live().unconfirmed_edits() == 0 && matches!(room.peers[i].live.status(), LiveStatus::Live { .. })
        })
        .map(|i| room.replica_state(i))
}

fn trace(seed: u64) -> Tally {
    let peers = PEERS[usize::try_from(seed % 7).unwrap_or(0)];
    let mut rng = Rng(seed.wrapping_mul(0x2545_F491_4F6C_DD1D) ^ 0x6772_6964);
    let mut room = Room::new(peers);
    let other = room.wb(0).sheets[1].name.to_string();
    // Each peer's steps, shuffled together; the joiner takes the slot `peers` once it is in.
    let mut schedule: Vec<usize> = (0..=peers).flat_map(|p| std::iter::repeat_n(p, STEPS)).collect();
    for i in (1..schedule.len()).rev() {
        schedule.swap(i, rng.below(i + 1));
    }
    let join_at = schedule.len() / 4 + rng.below(schedule.len() / 4);
    let mut down = vec![false; peers];
    let mut t = Tally::default();
    for (step, slot) in schedule.into_iter().enumerate() {
        if step == join_at {
            if let Some(state) = checkpoint(&mut room, &down) {
                let j = room.join("Ivan", Access::Edit, None);
                let p = &mut room.peers[j];
                p.live.apply_checkpoint(&mut p.session, &state).unwrap_or_else(|e| panic!("seed {seed}: the checkpoint did not apply: {e}"));
                down.push(false);
                t.joined = true;
            }
        }
        // Before the joiner is in, its slots go to a drawn peer.
        let who = if slot < room.peers.len() { slot } else { rng.below(peers) };
        if down[who] {
            if rng.chance(25) {
                hub_restore(&room, who);
                down[who] = false;
                t.rejoins += 1;
            }
        } else if rng.chance(2) && down.iter().filter(|d| **d).count() + 1 < room.peers.len() {
            hub_drop(&room, who);
            down[who] = true;
            t.drops += 1;
        }
        let roll = rng.below(100);
        let (id, params) = match roll {
            0..=64 => free(&mut rng, &other),
            65..=76 => ordered(&mut rng),
            77..=88 => ("edit.undo", json!({})),
            _ => ("edit.redo", json!({})),
        };
        match room.run(who, id, params) {
            Ok(_) => match roll {
                0..=64 => t.free += 1,
                65..=76 => t.ordered += 1,
                77..=88 => t.undo += 1,
                _ => t.redo += 1,
            },
            Err(_) => t.refused += 1,
        }
        // Some peers pump, in a drawn order: delivery is seeded.
        let mut order: Vec<usize> = (0..room.peers.len()).collect();
        for i in (1..order.len()).rev() {
            order.swap(i, rng.below(i + 1));
        }
        for i in order.into_iter().filter(|_| rng.chance(40)) {
            room.pump(i);
        }
    }
    for i in 0..down.len() {
        if down[i] {
            hub_restore(&room, i);
        }
    }
    // Queued ordered edits and undos need the lock in turn: settle until nothing moves.
    for _ in 0..8 {
        room.settle();
    }
    assert_equal(&room, &format!("seed {seed} ({peers} peers)"));
    t
}

#[test]
fn random_traces_converge_2_to_8_peers() {
    for seed in seeds_or(6) {
        let t = trace(seed);
        eprintln!(
            "seed {seed}: {} peers, free {}, ordered {}, undo {}, redo {}, drops {}, rejoins {}, checkpoint joiner {}, refused {}",
            PEERS[usize::try_from(seed % 7).unwrap_or(0)],
            t.free,
            t.ordered,
            t.undo,
            t.redo,
            t.drops,
            t.rejoins,
            t.joined,
            t.refused
        );
        assert!(t.free > 0 && t.ordered > 0 && t.undo > 0, "seed {seed}: the trace ran every kind of step");
    }
}
