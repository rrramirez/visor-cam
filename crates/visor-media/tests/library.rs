mod common;

use common::*;
use visor_media::library::cached_thumbnail;

#[test]
fn thumbnails_are_rendered_once_until_the_recording_changes() {
    let (dir, mut project) = project("lib-thumb", 2);
    let cache = temp_dir("lib-thumb-cache");
    let first = cached_thumbnail(&dir, &cache, 180).unwrap();
    let jpeg = std::fs::read(&first).unwrap();
    assert_eq!(&jpeg[..2], [0xFF, 0xD8]);
    let made = std::fs::metadata(&first).unwrap().modified().unwrap();

    // Again: the same file, not rendered again.
    std::thread::sleep(std::time::Duration::from_millis(20));
    assert_eq!(cached_thumbnail(&dir, &cache, 180).unwrap(), first);
    assert_eq!(std::fs::metadata(&first).unwrap().modified().unwrap(), made);

    // Edited: a new one, and the old one goes.
    project.thumbnail_us = Some(1_000_000);
    std::thread::sleep(std::time::Duration::from_millis(20));
    project.save(&dir).unwrap();
    let second = cached_thumbnail(&dir, &cache, 180).unwrap();
    assert_ne!(second, first);
    assert!(second.is_file() && !first.exists());
    // Another recording's stays.
    let (other, _) = common::project("lib-thumb-other", 1);
    let third = cached_thumbnail(&other, &cache, 180).unwrap();
    assert!(second.is_file() && third.is_file());
    // Recordings gone from the library lose theirs.
    visor_media::library::prune_thumbnails(&cache, std::slice::from_ref(&other), 180);
    assert!(!second.exists() && third.is_file());
    for d in [dir, other, cache] {
        common::cleanup(&d);
    }
}
