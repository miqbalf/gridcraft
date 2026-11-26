//! A renewed ticket while an ordered (row or column) edit is asked, waiting or running: the
//! window's hub (`craft_collab::Handover`, as on the web) switches links on a new ticket, and
//! the relay frees the old link's structure lock when that link closes. The edit must still
//! land once, on every peer, with no notice (GC1 fix round 1, I1).

use gridcraft_core::Value;
use serde_json::json;

use crate::support::{Room, a1, diff};

fn value_at(room: &Room, i: usize, cell: &str) -> Value {
    room.wb(i).sheet(0).and_then(|s| s.cell(a1(cell))).map(|c| c.value.clone()).unwrap_or(Value::Empty)
}

fn same(room: &Room, what: &str) {
    let f0 = room.finger(0);
    for i in 1..room.peers.len() {
        assert_eq!(f0, room.finger(i), "{what}: peers 0 and {i} differ:\n{}", diff(&f0, &room.finger(i)));
    }
}

fn switches(room: &Room, i: usize) -> u64 {
    room.peers[i].live.core().hub().inner().switches()
}

#[test]
fn a_row_insert_deferred_by_an_open_undo_group_across_a_ticket_renewal_lands_with_no_notice() {
    // Bob's insert has the lock but waits for his open undo group (a macro). The renewal's
    // switch is a rejoin and the relay frees the lock with the old link: the edit must not be
    // dropped as if the connection had dropped.
    let mut room = Room::new(2);
    let token = room.peers[1].session.group_begin("macro").unwrap();
    room.run(0, "cell.set", json!({"cell": "D5", "input": "from alice"})).unwrap();
    room.settle();
    room.run(1, "home.insertRows", json!({"rows": "3:3"})).unwrap();
    for _ in 0..6 {
        room.pump(1);
    }
    room.peers[1].live.renew_ticket("renewed".to_owned());
    for _ in 0..4 {
        room.pump(1);
        room.pump(0);
    }
    room.peers[1].session.group_end(token).unwrap();
    room.settle();
    room.settle();
    same(&room, "after the insert across the renewal");
    for i in 0..2 {
        assert_eq!(value_at(&room, i, "A3"), Value::Empty, "peer {i}: the row was inserted");
        assert_eq!(value_at(&room, i, "A4"), Value::Number(3.0), "peer {i}: once");
        assert_eq!(value_at(&room, i, "D6"), Value::Text("from alice".into()), "peer {i}: alice's cell moved down with it");
    }
    assert!(room.notices_of(1).is_empty(), "nothing dropped, so nothing is told: {:?}", room.notices_of(1));
    assert_eq!(switches(&room, 1), 1, "the renewal still switched the link");
    room.assert_all_live("after the renewal");
}

#[test]
fn a_row_insert_running_across_a_ticket_renewal_lands_with_no_notice() {
    let mut room = Room::new(2);
    // Alice's insert gets the lock and runs; the room's answer to its edit is still on the way.
    *room.peers[0].hold_ops.borrow_mut() = true;
    room.run(0, "home.insertRows", json!({"rows": "3:3"})).unwrap();
    for _ in 0..3 {
        room.pump(0);
    }
    // The page's ticket refresh comes now.
    room.peers[0].live.renew_ticket("renewed".to_owned());
    for _ in 0..3 {
        room.pump(0);
        room.pump(1);
    }
    *room.peers[0].hold_ops.borrow_mut() = false;
    room.settle();
    room.settle();
    same(&room, "after the insert across the renewal");
    for i in 0..2 {
        assert_eq!(value_at(&room, i, "A3"), Value::Empty, "peer {i}: the row was inserted once");
        assert_eq!(value_at(&room, i, "A4"), Value::Number(3.0), "peer {i}: the old row 3 moved down one");
        assert_eq!(value_at(&room, i, "A5"), Value::Number(4.0), "peer {i}: and only one");
    }
    assert!(room.notices_of(0).is_empty(), "nothing dropped, so nothing is told: {:?}", room.notices_of(0));
    assert_eq!(switches(&room, 0), 1, "the renewal still switched the link");
    room.assert_all_live("after the renewal");
}

#[test]
fn a_row_insert_waiting_for_the_lock_across_a_ticket_renewal_lands_with_no_notice() {
    let mut room = Room::new(2);
    // Bob holds the lock: his insert is granted but not run yet.
    room.run(1, "home.insertRows", json!({"rows": "2:2"})).unwrap();
    // Alice's insert asks and is refused for now: it waits for Bob.
    room.run(0, "home.insertRows", json!({"rows": "6:6"})).unwrap();
    room.pump(0);
    room.peers[0].live.renew_ticket("renewed".to_owned());
    room.pump(0);
    room.pump(0);
    room.settle();
    room.settle();
    same(&room, "after both inserts");
    for i in 0..2 {
        assert_eq!(value_at(&room, i, "A2"), Value::Empty, "peer {i}: Bob's row");
        assert_eq!(value_at(&room, i, "A3"), Value::Number(2.0), "peer {i}");
        // Alice's insert goes before the line she asked for (the old row 6), found again by id
        // after Bob's insert moved it down.
        assert_eq!(value_at(&room, i, "A6"), Value::Number(5.0), "peer {i}");
        assert_eq!(value_at(&room, i, "A7"), Value::Empty, "peer {i}: Alice's row");
        assert_eq!(value_at(&room, i, "A8"), Value::Number(6.0), "peer {i}");
    }
    assert!(room.notices_of(0).is_empty(), "nothing dropped, so nothing is told: {:?}", room.notices_of(0));
    assert_eq!(switches(&room, 0), 1, "the renewal still switched the link");
    room.assert_all_live("after the renewal");
}

/// A clock for peer `i`'s session that the test moves by hand, in ms.
fn hand_clock(room: &mut Room, i: usize) -> std::sync::Arc<std::sync::Mutex<f64>> {
    let now = std::sync::Arc::new(std::sync::Mutex::new(0.0f64));
    let read = now.clone();
    room.peers[i].live.set_clock(Box::new(move || *read.lock().unwrap()));
    now
}

fn fenced_submits(room: &Room, i: usize) -> usize {
    room.peers[i].fenced.borrow().iter().filter(|f| **f).count()
}

#[test]
fn a_row_insert_asked_while_the_renewed_link_has_not_joined_waits_for_it() {
    // GC1 fix 1 review N1: the reverse race. The ticket goes over while nothing is open, and
    // the new link is slow to join. An insert asked meanwhile is granted on the old link, which
    // the switch will close (freeing the lock): it must not run there.
    let mut room = Room::new(2);
    *room.peers[0].hold_join.borrow_mut() = true;
    room.peers[0].live.renew_ticket("renewed".to_owned());
    room.run(0, "home.insertRows", json!({"rows": "3:3"})).unwrap();
    for _ in 0..6 {
        room.pump(0);
        room.pump(1);
    }
    assert!(room.relay.holder("grid/struct").is_some(), "the old link was granted the lock");
    assert_eq!(fenced_submits(&room, 0), 0, "the grant on the link about to close is not used");
    assert_eq!(switches(&room, 0), 0, "the new link has not joined yet");
    // The new link's join answer arrives: the switch, then the rejoin asks again.
    *room.peers[0].hold_join.borrow_mut() = false;
    room.settle();
    room.settle();
    same(&room, "after the insert");
    for i in 0..2 {
        assert_eq!(value_at(&room, i, "A3"), Value::Empty, "peer {i}: the row was inserted");
        assert_eq!(value_at(&room, i, "A4"), Value::Number(3.0), "peer {i}: once");
        assert_eq!(value_at(&room, i, "A5"), Value::Number(4.0), "peer {i}");
    }
    assert!(fenced_submits(&room, 0) > 0, "it ran on the new link");
    assert!(room.notices_of(0).is_empty(), "nothing dropped, so nothing is told: {:?}", room.notices_of(0));
    assert_eq!(switches(&room, 0), 1);
    room.assert_all_live("after the switch");
}

#[test]
fn a_grant_waits_for_a_renewed_link_ten_seconds_at_most() {
    // N1: the new link never joins (refused elsewhere, or very slow). After SWITCH_WAIT_MS the
    // hub is taken to have kept the old link, and the granted insert runs there.
    let mut room = Room::new(2);
    let now = hand_clock(&mut room, 0);
    *room.peers[0].hold_join.borrow_mut() = true;
    room.peers[0].live.renew_ticket("renewed".to_owned());
    room.run(0, "home.insertRows", json!({"rows": "3:3"})).unwrap();
    for _ in 0..6 {
        room.pump(0);
        room.pump(1);
    }
    *now.lock().unwrap() = 9_000.0;
    room.pump(0);
    room.pump(0);
    assert_eq!(fenced_submits(&room, 0), 0, "still waiting at 9 s");
    *now.lock().unwrap() = 10_500.0;
    room.settle();
    room.settle();
    same(&room, "after the insert on the old link");
    for i in 0..2 {
        assert_eq!(value_at(&room, i, "A3"), Value::Empty, "peer {i}: the row was inserted");
        assert_eq!(value_at(&room, i, "A4"), Value::Number(3.0), "peer {i}: once");
    }
    assert_eq!(switches(&room, 0), 0, "the new link never joined");
    assert!(room.notices_of(0).is_empty(), "{:?}", room.notices_of(0));
    // When it does join at last, nothing is open: the switch is harmless.
    *room.peers[0].hold_join.borrow_mut() = false;
    room.settle();
    assert_eq!(switches(&room, 0), 1);
    same(&room, "after the late switch");
    assert!(room.notices_of(0).is_empty(), "{:?}", room.notices_of(0));
    room.assert_all_live("after the late switch");
}

#[test]
fn a_ticket_held_for_an_open_edit_goes_over_after_sixty_seconds() {
    // N1: the hold has a cap, so an edit that stays open (here a macro's open undo group) cannot
    // keep the renewed ticket until the old one expires. Past the cap the switch happens, and
    // the deferred insert meets the old rejoin path: it is dropped and told, as for any drop.
    let mut room = Room::new(2);
    let now = hand_clock(&mut room, 1);
    let token = room.peers[1].session.group_begin("macro").unwrap();
    room.run(1, "home.insertRows", json!({"rows": "3:3"})).unwrap();
    for _ in 0..6 {
        room.pump(1);
    }
    room.peers[1].live.renew_ticket("renewed".to_owned());
    for _ in 0..4 {
        room.pump(1);
        room.pump(0);
    }
    assert_eq!(switches(&room, 1), 0, "held while the insert is open");
    // N5: the host resends the ticket at 30 s; the hold still counts from the first.
    *now.lock().unwrap() = 30_000.0;
    room.peers[1].live.renew_ticket("renewed again".to_owned());
    *now.lock().unwrap() = 59_000.0;
    for _ in 0..4 {
        room.pump(1);
        room.pump(0);
    }
    assert_eq!(switches(&room, 1), 0, "still held at 59 s");
    *now.lock().unwrap() = 60_500.0;
    for _ in 0..4 {
        room.pump(1);
        room.pump(0);
    }
    assert_eq!(switches(&room, 1), 1, "the cap handed the ticket over and the link switched");
    room.peers[1].session.group_end(token).unwrap();
    room.settle();
    room.settle();
    same(&room, "after the dropped insert");
    for i in 0..2 {
        assert_eq!(value_at(&room, i, "A3"), Value::Number(3.0), "peer {i}: the insert was not made");
    }
    let notices = room.notices_of(1);
    assert!(notices.iter().any(|n| n.contains("connection dropped before your change") && n.contains("was not made")), "{notices:?}");
}
