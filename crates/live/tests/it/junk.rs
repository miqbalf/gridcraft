//! Junk in the shared document: a peer with a ticket writes values no editor would. The other
//! peers stay `Live`, and the room's state still builds a workbook.

use craft_collab::{ClientMsg, Hub, Replica, ServerMsg, TextPayload};
use craft_live::testing::ROOM;
use gridcraft_live::{AxisKind, CellContent, GridDoc, Id, cell_key, workbook_from_state};
use serde_json::json;

use crate::support::{Room, finger};

#[test]
fn junk_values_are_dropped_and_the_room_survives() {
    let mut room = Room::new(3);
    room.run(0, "cell.set", json!({"cell": "A9", "input": "before"})).unwrap();
    room.settle();
    let mut mallory = room.relay.connect("mallory", "Mallory", true);
    mallory.send(ClientMsg::Join { doc: ROOM.into(), display: "Mallory".into() }).unwrap();
    let space = mallory
        .poll()
        .into_iter()
        .find_map(|m| if let ServerMsg::Joined { space, .. } = m { Some(space) } else { None })
        .expect("the relay granted a space");
    let mut raw = GridDoc::from_state(&room.replica_state(0)).unwrap();
    raw.set_client(space).unwrap();
    let sheet = Id::base(0);
    let line = |n: u32, part: &str| format!("{}/{part}", Id::base(n).text());
    let array = CellContent { f: Some("=1".into()), a: Some([2_000_000_000, 2_000_000_000]), ..CellContent::default() };
    // Too big for yrs to hand back as an integer: the cell reads as the base's (dropped).
    let huge = CellContent { f: Some("=1".into()), a: Some([u32::MAX, u32::MAX]), ..CellContent::default() };
    let update = raw
        .write(|t| {
            t.set_line(sheet, AxisKind::Rows, &line(1, "h"), json!(-5.0));
            t.set_line(sheet, AxisKind::Rows, &line(2, "h"), json!(1e300));
            t.set_line(sheet, AxisKind::Cols, &line(1, "w"), json!(-1.0));
            t.set_line(sheet, AxisKind::Cols, &line(2, "w"), json!("wide"));
            t.set_line(sheet, AxisKind::Rows, &line(3, "hide"), json!("yes"));
            t.set_line(sheet, AxisKind::Cols, &line(3, "st"), json!("no such style"));
            t.set_line(sheet, AxisKind::Cols, &line(4, "st"), json!(17));
            t.set_sheet_prop(sheet, "defw", json!(-3));
            t.set_sheet_prop(sheet, "defh", json!(0));
            t.set_cell(sheet, &cell_key(Id::base(6), Id::base(6)), &array);
            t.set_cell(sheet, &cell_key(Id::base(8), Id::base(8)), &huge);
            t.set_cell(sheet, &cell_key(Id::base(7), Id::base(7)), &CellContent { a: Some([0, 0]), f: Some("=2".into()), ..CellContent::default() });
        })
        .unwrap();
    assert!(craft_collab::update_is_sound(&update), "junk values are still a sound update");
    // The junk reaches the mapping as written (an integer, not a mangled float).
    let back = GridDoc::from_state(&raw.state()).unwrap();
    let key = cell_key(Id::base(6), Id::base(6));
    assert!(
        matches!(back.cell(sheet, &key), gridcraft_live::Entry::Set(c) if c.a == Some([2_000_000_000, 2_000_000_000])),
        "the array extent is stored as written: {:?}",
        back.cell(sheet, &key)
    );
    assert_eq!(back.cell(sheet, &cell_key(Id::base(8), Id::base(8))), gridcraft_live::Entry::Absent, "an extent no integer holds is dropped");
    let params = serde_json::to_value(TextPayload::new(&update)).unwrap();
    mallory.send(ClientMsg::Submit { doc: ROOM.into(), local: 1, command: craft_collab::CRDT_TEXT.into(), params, scope: None }).unwrap();
    room.settle();
    room.assert_all_live("junk");
    // The clamp ran: the array ends at the last cell of the grid, on every peer.
    for i in 0..3 {
        let cell = room.wb(i).sheet(0).unwrap().cell(gridcraft_core::CellRef::new(6, 6)).cloned();
        let end = cell.and_then(|c| c.formula.as_ref().and_then(|f| f.array)).map(|r| r.end);
        assert_eq!(
            end,
            Some(gridcraft_core::CellRef::new(gridcraft_core::MAX_ROWS - 1, gridcraft_core::MAX_COLS - 1)),
            "peer {i}: array clamped to the grid"
        );
    }
    room.run(1, "cell.set", json!({"cell": "A10", "input": "after"})).unwrap();
    room.settle();
    room.assert_all_live("typing after the junk");
    assert_eq!(room.finger(0), room.finger(1));
    assert_eq!(room.finger(0), room.finger(2));
    let state = room.replica_state(0);
    let oracle = workbook_from_state(&room.base(), &state, None).expect("the room's state still builds a workbook");
    assert_eq!(room.finger(0), finger(&oracle));
    let late = room.join("Dana", craft_live::Access::Edit, None);
    room.settle();
    assert_eq!(room.finger(0), room.finger(late));
}
