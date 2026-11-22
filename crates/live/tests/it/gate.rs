//! The command gate and its safety net.

use std::collections::BTreeSet;

use craft_live::Access;
use gridcraft_live::gate::{self, FREE, LOCAL, NOT_YET, ORDERED};
use gridcraft_live::{BaseFile, GridLive, LiveFail, NOT_YET_TEXT, Ran};
use serde_json::json;

use crate::support::{FILE, Room, a1};

#[test]
fn m2_command_is_refused_while_live_and_the_workbook_is_unchanged() {
    let mut room = Room::new(2);
    let (before, seq) = (room.finger(0), room.relay.seq());
    let r = room.run(0, "data.sort", json!({"range": "A1:A6", "header": false, "keys": [{"column": "A"}]}));
    assert_eq!(r, Err(NOT_YET_TEXT.to_owned()));
    room.settle();
    assert_eq!(room.finger(0), before, "the workbook is as it was");
    assert_eq!(room.relay.seq(), seq, "nothing was sent");
}

#[test]
fn safety_net_puts_back_an_unshared_change() {
    let mut room = Room::new(2);
    let (before, seq) = (room.finger(0), room.relay.seq());
    let merges = room.wb(0).sheet(0).unwrap().merges.clone();
    // Bypass the gate: the engine runs it, the observer hears it, the pump must catch it.
    room.peers[0].session.execute("home.mergeCells", json!({"range": "A1:B1"})).unwrap();
    assert_ne!(room.wb(0).sheet(0).unwrap().merges, merges, "the engine did merge");
    room.pump(0);
    assert_eq!(room.wb(0).sheet(0).unwrap().merges, merges, "the merge was put back");
    assert_eq!(room.finger(0), before);
    assert!(room.notices_of(0).iter().any(|n| n == NOT_YET_TEXT), "the user is told: {:?}", room.notices_of(0));
    room.settle();
    assert_eq!(room.relay.seq(), seq, "nothing was sent");
    assert_eq!(room.finger(1), before);
}

#[test]
fn every_catalog_command_is_classified_once() {
    let mut ids: BTreeSet<&str> = gridcraft_engine::catalog::CATALOG.iter().map(|c| c.0).collect();
    ids.extend(gridcraft_engine::command_specs().iter().map(|c| c.id));
    for id in &ids {
        assert_eq!(gate::tables_holding(id), 1, "{id} is in exactly one table");
    }
    for table in [FREE, ORDERED, LOCAL, NOT_YET] {
        assert!(table.windows(2).all(|w| w[0] < w[1]), "tables are sorted and hold no duplicates");
        assert!(table.iter().all(|id| ids.contains(id)), "no table names a command that does not exist");
    }
    assert_eq!(gate::tables_holding("no.such.command"), 0);
    assert_eq!(gate::class("no.such.command"), gate::Class::NotYet, "an unknown command is refused");
}

#[test]
fn ordered_commands_wait_for_the_lock_and_are_not_refused() {
    let mut room = Room::new(2);
    let before = room.finger(0);
    assert_eq!(room.run(0, "home.insertRows", json!({"rows": "3:3"})), Ok(Ran::Queued));
    // Nothing runs before the relay grants the lock.
    assert_eq!(room.finger(0), before);
    room.settle();
    assert_ne!(room.finger(0), before);
    assert_eq!(room.finger(0), room.finger(1));
    assert!(room.peers[0].live.allows("home.insertRows"));
    assert!(room.peers[0].live.allows("home.bold") && room.peers[0].live.allows("selection.set"));
}

#[test]
fn local_commands_run_and_share_nothing() {
    let mut room = Room::new(2);
    let seq = room.relay.seq();
    let r = room.run(0, "selection.set", json!({"cell": "C3"})).unwrap();
    assert!(matches!(r, Ran::Done(_)));
    room.settle();
    assert_eq!(room.relay.seq(), seq);
    assert_eq!(room.peers[0].session.active().unwrap().selection.active, a1("C3"));
}

#[test]
fn read_only_access_sends_nothing() {
    let mut room = Room::new(1);
    let ro = room.join("Rita", Access::Read, None);
    room.settle();
    let seq = room.relay.seq();
    assert!(!room.peers[ro].live.editable());
    let r = room.run(ro, "cell.set", json!({"cell": "D5", "input": "no"}));
    assert!(r.is_err(), "a read-only window takes no edit");
    assert!(room.run(ro, "selection.set", json!({"cell": "C3"})).is_ok(), "it can still look around");
    room.settle();
    assert_eq!(room.relay.seq(), seq, "nothing was sent");
    assert!(room.wb(ro).sheet(0).unwrap().cell(a1("D5")).is_none());
    // Edits made behind the gate are put back and not sent.
    room.peers[ro].session.execute("cell.set", json!({"cell": "D5", "input": "sneaky"})).unwrap();
    room.settle();
    assert_eq!(room.relay.seq(), seq);
    assert!(room.wb(ro).sheet(0).unwrap().cell(a1("D5")).is_none(), "put back");
}

#[test]
fn base_mismatch_refuses_the_start() {
    let relay = craft_live::testing::MemRelay::new();
    let hub = relay.connect("alice", "Alice", true);
    let mut session = gridcraft_engine::Session::new();
    let bytes = crate::support::base_bytes(false);
    let base = BaseFile { bytes: &bytes, name: FILE, expected_sha256: Some([0; 32]) };
    let r = GridLive::start(hub, craft_live::testing::ROOM, &base, Access::Edit, "alice", &mut session);
    assert!(matches!(r, Err(LiveFail::BaseMismatch)));
    assert!(session.documents().is_empty(), "nothing was opened");
    let hub = relay.connect("alice", "Alice", true);
    let junk = BaseFile { bytes: b"PK\x03\x04 truncated", name: FILE, expected_sha256: None };
    assert!(matches!(GridLive::start(hub, craft_live::testing::ROOM, &junk, Access::Edit, "alice", &mut session), Err(LiveFail::Open(_))));
}

#[test]
fn undo_is_told_not_available_and_stop_unhooks_the_engine() {
    let mut room = Room::new(2);
    room.run(0, "cell.set", json!({"cell": "D5", "input": "1"})).unwrap();
    room.settle();
    let seq = room.relay.seq();
    room.run(0, "edit.undo", json!({})).unwrap();
    room.settle();
    assert_eq!(room.relay.seq(), seq, "undo shares nothing yet");
    assert!(room.notices_of(0).iter().any(|n| n.contains("Undo")), "{:?}", room.notices_of(0));
    assert_eq!(room.finger(0), room.finger(1));
    let mut p = room.peers.remove(0);
    p.live.stop(&mut p.session);
    p.session.execute("cell.set", json!({"cell": "D6", "input": "after the end"})).unwrap();
    assert!(p.session.active().unwrap().wb.sheet(0).unwrap().cell(a1("D6")).is_some(), "an ordinary workbook again: edits stay");
}

#[test]
fn a_remote_change_waits_for_an_open_undo_group_then_lands() {
    let mut room = Room::new(2);
    let token = room.peers[1].session.group_begin("macro").unwrap();
    room.run(0, "cell.set", json!({"cell": "D5", "input": "from alice"})).unwrap();
    room.pump(1);
    assert!(room.wb(1).sheet(0).unwrap().cell(a1("D5")).is_none(), "held back while the group is open");
    room.peers[1].session.group_end(token).unwrap();
    room.settle();
    assert_eq!(room.finger(0), room.finger(1), "applied once the group ended");
}
