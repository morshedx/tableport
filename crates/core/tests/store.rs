use tableport_core::LocalStore;

fn temp_store(name: &str) -> (LocalStore, String) {
    let path = std::env::temp_dir().join(format!("tableport-store-{}-{name}.sqlite", std::process::id()));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    (LocalStore::open(path.clone()).unwrap(), path)
}

#[test]
fn history_is_per_connection_newest_first_and_searchable() {
    let (store, path) = temp_store("history");
    store.add_history("a".into(), "SELECT 1".into(), Some(3), false).unwrap();
    store.add_history("a".into(), "select * from Users".into(), None, true).unwrap();
    store.add_history("b".into(), "SELECT 2".into(), Some(1), false).unwrap();

    let a = store.history("a".into(), "".into(), 10).unwrap();
    assert_eq!(a.iter().map(|e| e.sql.as_str()).collect::<Vec<_>>(), ["select * from Users", "SELECT 1"]);
    assert!(a[0].failed && a[0].elapsed_ms.is_none());
    assert_eq!(a[1].elapsed_ms, Some(3));
    assert_eq!(store.history("a".into(), "USERS".into(), 10).unwrap().len(), 1, "search ignores case");

    store.clear_history("a".into()).unwrap();
    assert!(store.history("a".into(), "".into(), 10).unwrap().is_empty());
    assert_eq!(store.history("b".into(), "".into(), 10).unwrap().len(), 1);
    std::fs::remove_file(path).ok();
}

#[test]
fn history_keeps_the_newest_thousand() {
    let (store, path) = temp_store("limit");
    for index in 0..1005 {
        store.add_history("a".into(), format!("SELECT {index}"), None, false).unwrap();
    }
    let all = store.history("a".into(), "".into(), 5000).unwrap();
    assert_eq!(all.len(), 1000);
    assert_eq!(all[0].sql, "SELECT 1004");
    std::fs::remove_file(path).ok();
}

#[test]
fn saved_queries_save_update_rename_delete_and_survive_reopening() {
    let (store, path) = temp_store("saved");
    let first = store.save_query("a".into(), None, "  Daily orders ".into(), "SELECT 1".into()).unwrap();
    assert_eq!(first.name, "Daily orders");
    store.save_query("a".into(), None, "active users".into(), "SELECT 2".into()).unwrap();
    store.save_query("b".into(), None, "other".into(), "SELECT 3".into()).unwrap();

    let updated = store.save_query("a".into(), Some(first.id), "Daily orders".into(), "SELECT 10".into()).unwrap();
    assert_eq!(updated.id, first.id);
    store.rename_query(first.id, "Orders today".into()).unwrap();
    assert!(store.save_query("a".into(), None, "   ".into(), "x".into()).is_err(), "a name is needed");
    drop(store);

    let store = LocalStore::open(path.clone()).unwrap();
    let saved = store.saved_queries("a".into()).unwrap();
    assert_eq!(saved.iter().map(|q| (q.name.as_str(), q.sql.as_str())).collect::<Vec<_>>(), [("active users", "SELECT 2"), ("Orders today", "SELECT 10")]);
    store.delete_query(saved[0].id).unwrap();
    assert_eq!(store.saved_queries("a".into()).unwrap().len(), 1);
    store.forget_connection("b".into()).unwrap();
    assert!(store.saved_queries("b".into()).unwrap().is_empty());
    drop(store);
    std::fs::remove_file(&path).ok();
    std::fs::remove_file(format!("{path}-wal")).ok();
    std::fs::remove_file(format!("{path}-shm")).ok();
}
