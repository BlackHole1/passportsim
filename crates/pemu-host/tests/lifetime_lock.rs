//! The daemon lifetime lock, in a binary of its own: the `flock` lives on an open file
//! description, and a child forked by another test holds a copy until it execs, which made the
//! release look late beside the library's spawning tests.

use pemu_host::daemon::DiscoveryStore;
use pemu_host::paths::OwnerOnlyFiles;
use pemu_host::platform::fake::FakeOwnerOnly;

#[test]
fn the_lifetime_lock_admits_one_holder_until_it_is_released() {
    let dir = std::env::temp_dir().join(format!(
        "pemu-lifetime-lock-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("after the epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("a temp directory");
    let guard = FakeOwnerOnly::new();
    let store = DiscoveryStore::new(dir.join("run"), OwnerOnlyFiles::new(&guard));
    let first = store
        .lock_lifetime()
        .expect("lock")
        .expect("the first holder");
    assert!(
        store.lock_lifetime().expect("lock").is_none(),
        "a second open file description is refused while the first holds it"
    );
    drop(first);
    assert!(
        store.lock_lifetime().expect("lock").is_some(),
        "released on drop"
    );
    std::fs::remove_dir_all(&dir).ok();
}
